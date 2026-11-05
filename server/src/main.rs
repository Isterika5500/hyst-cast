//! hyst-cast-server — ретранслятор стримов (SFU) на QUIC.
//!
//! Стример отправляет на сервер один поток, сервер раздаёт его всем, кто смотрит.
//! Отстающему зрителю сервер выкидывает кадры до ближайшего ключевого (и просит его у стримера),
//! чтобы у остальных ничего не тормозило.
//!
//! Переменные окружения:
//!   HYST_CAST_PORT   — UDP-порт (4433)
//!   HYST_CAST_TOKEN  — общий пароль сервера (обязателен)
//!   HYST_CAST_MAX    — максимум человек в комнате (6)
//!   HYST_CAST_DATA   — где хранить сертификат (./data)

use anyhow::{Context, Result};
use bytes::Bytes;
use proto::*;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::info;

/// Очередь пакетов на одного зрителя одного стрима. ~2 с видео+звука при 60 fps.
const SUB_QUEUE: usize = 240;
/// Не чаще одного запроса ключевого кадра у стримера.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);

struct Sub {
    tx: mpsc::Sender<Bytes>,
    /// Зритель отстал/только подключился — ждём ключевой кадр, остальное видео выкидываем.
    need_key: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

struct Peer {
    name: String,
    ctl: mpsc::UnboundedSender<ServerMsg>,
    stream: Option<StreamInfo>,
    /// Кто смотрит этого участника: id зрителя → подписка.
    watchers: Arc<Mutex<HashMap<u32, Sub>>>,
    last_keyreq: Arc<Mutex<Instant>>,
}

#[derive(Default)]
struct Room {
    peers: HashMap<u32, Peer>,
}

struct State {
    rooms: Mutex<HashMap<String, Room>>,
    next_id: AtomicU32,
    token: String,
    max_peers: u32,
}

impl State {
    fn broadcast(&self, room: &str, msg: ServerMsg, except: u32) {
        let rooms = self.rooms.lock().unwrap();
        if let Some(r) = rooms.get(room) {
            for (id, p) in &r.peers {
                if *id != except {
                    let _ = p.ctl.send(msg.clone());
                }
            }
        }
    }

    fn send_viewers(room: &Room, publisher: u32) {
        if let Some(p) = room.peers.get(&publisher) {
            let mut names: Vec<String> = p
                .watchers
                .lock()
                .unwrap()
                .keys()
                .filter_map(|v| room.peers.get(v).map(|x| x.name.clone()))
                .collect();
            names.sort();
            let _ = p.ctl.send(ServerMsg::Viewers { names });
        }
    }
}

fn request_keyframe(ctl: &mpsc::UnboundedSender<ServerMsg>, last: &Mutex<Instant>) {
    let mut l = last.lock().unwrap();
    if l.elapsed() >= KEYFRAME_INTERVAL {
        *l = Instant::now();
        let _ = ctl.send(ServerMsg::KeyframeRequest);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let port: u16 = std::env::var("HYST_CAST_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(4433);
    let token = std::env::var("HYST_CAST_TOKEN").context("задай HYST_CAST_TOKEN")?;
    anyhow::ensure!(!token.is_empty(), "HYST_CAST_TOKEN пустой");
    let max_peers = std::env::var("HYST_CAST_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    let data = PathBuf::from(std::env::var("HYST_CAST_DATA").unwrap_or_else(|_| "data".into()));

    let (cert, key) = load_or_create_cert(&data)?;
    let fp = fingerprint_hex(&Sha256::digest(cert.as_ref()));
    info!("отпечаток сертификата: {fp}");

    let endpoint = quinn::Endpoint::server(server_config(cert, key)?, SocketAddr::from(([0, 0, 0, 0], port)))?;
    info!("hyst-cast-server слушает udp/{port}, максимум {max_peers} в комнате");

    let state = Arc::new(State { rooms: Mutex::new(HashMap::new()), next_id: AtomicU32::new(1), token, max_peers });

    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle_conn(state, conn).await {
                        info!("{remote}: {e:#}");
                    }
                }
                Err(e) => info!("{remote}: рукопожатие не удалось: {e}"),
            }
        });
    }
    Ok(())
}

fn load_or_create_cert(dir: &PathBuf) -> Result<(rustls::pki_types::CertificateDer<'static>, rustls::pki_types::PrivateKeyDer<'static>)> {
    std::fs::create_dir_all(dir)?;
    let cp = dir.join("cert.der");
    let kp = dir.join("key.der");
    if !cp.exists() || !kp.exists() {
        let ck = rcgen::generate_simple_self_signed(vec!["hyst-cast".into()])?;
        std::fs::write(&cp, ck.cert.der())?;
        std::fs::write(&kp, ck.signing_key.serialize_der())?;
        info!("создан новый сертификат в {}", dir.display());
    }
    let cert = rustls::pki_types::CertificateDer::from(std::fs::read(&cp)?);
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(std::fs::read(&kp)?.into());
    Ok((cert, key))
}

pub fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_concurrent_uni_streams(4096u32.into());
    t.max_concurrent_bidi_streams(4u32.into());
    t.keep_alive_interval(Some(Duration::from_secs(4)));
    t.max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()));
    t.stream_receive_window((8u32 << 20).into());
    t.receive_window((32u32 << 20).into());
    // Небольшое окно отправки: если канал не тянет, пакеты копятся в НАШЕЙ очереди,
    // где мы умеем выкидывать старое, а не в недрах QUIC, где они превращаются в задержку.
    t.send_window(1_500_000);
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t
}

fn server_config(
    cert: rustls::pki_types::CertificateDer<'static>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    sc.transport_config(Arc::new(transport_config()));
    Ok(sc)
}

async fn handle_conn(state: Arc<State>, conn: quinn::Connection) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await??;
    let hello: ClientMsg = tokio::time::timeout(Duration::from_secs(10), read_msg(&mut recv)).await??.context("нет приветствия")?;
    let ClientMsg::Hello { version, token, name, room } = hello else { anyhow::bail!("первым должно быть Hello") };

    let fail = |m: &str| ServerMsg::Error { message: m.to_string() };
    if version != PROTOCOL_VERSION {
        write_msg(&mut send, &fail("Версия приложения не совпадает с сервером — обнови приложение")).await?;
        anyhow::bail!("версия {version}");
    }
    if token != state.token {
        write_msg(&mut send, &fail("Неверный пароль сервера")).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        anyhow::bail!("неверный токен");
    }
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(32).collect();
    let room: String = room.trim().to_lowercase().chars().filter(|c| !c.is_control()).take(64).collect();
    if name.is_empty() || room.is_empty() {
        write_msg(&mut send, &fail("Пустое имя или комната")).await?;
        anyhow::bail!("пустое имя/комната");
    }

    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let full = {
        let rooms = state.rooms.lock().unwrap();
        rooms.get(&room).is_some_and(|r| r.peers.len() as u32 >= state.max_peers)
    };
    if full {
        write_msg(&mut send, &fail(&format!("Комната заполнена (максимум {})", state.max_peers))).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        anyhow::bail!("комната полна");
    }
    {
        let mut rooms = state.rooms.lock().unwrap();
        let r = rooms.entry(room.clone()).or_default();
        let peers = r
            .peers
            .iter()
            .map(|(pid, p)| PeerInfo { id: *pid, name: p.name.clone(), stream: p.stream.clone() })
            .collect();
        let _ = ctl_tx.send(ServerMsg::Welcome { id, peers, max_peers: state.max_peers });
        r.peers.insert(
            id,
            Peer {
                name: name.clone(),
                ctl: ctl_tx.clone(),
                stream: None,
                watchers: Arc::new(Mutex::new(HashMap::new())),
                last_keyreq: Arc::new(Mutex::new(Instant::now() - KEYFRAME_INTERVAL)),
            },
        );
        info!("+ {name} ({id}) → {room} [{}] {}", r.peers.len(), conn.remote_address());
    }
    state.broadcast(&room, ServerMsg::PeerJoined { peer: PeerInfo { id, name: name.clone(), stream: None } }, id);

    // Отправка управляющих сообщений клиенту
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if write_msg(&mut send, &m).await.is_err() {
                break;
            }
        }
    });

    // Приём медиа от этого клиента (если он стримит) и раздача зрителям
    let media = {
        let (watchers, ctl, last) = {
            let rooms = state.rooms.lock().unwrap();
            let p = &rooms[&room].peers[&id];
            (p.watchers.clone(), p.ctl.clone(), p.last_keyreq.clone())
        };
        let conn = conn.clone();
        tokio::spawn(async move {
            // Пакеты читаем параллельно, а раздаём строго по порядку (порядок кадров H.264 важен).
            let (otx, mut orx) = mpsc::channel::<tokio::task::JoinHandle<Option<Vec<u8>>>>(128);
            let acceptor = tokio::spawn(async move {
                while let Ok(mut uni) = conn.accept_uni().await {
                    let h = tokio::spawn(async move { uni.read_to_end(MAX_MEDIA).await.ok() });
                    if otx.send(h).await.is_err() {
                        break;
                    }
                }
            });
            while let Some(h) = orx.recv().await {
                let Ok(Some(mut buf)) = h.await else { continue };
                let Ok((hdr, _)) = MediaHeader::parse(&buf) else { continue };
                MediaHeader::set_publisher(&mut buf, id);
                let pkt = Bytes::from(buf);
                let mut want_key = false;
                for sub in watchers.lock().unwrap().values() {
                    if hdr.kind == MediaKind::Video && sub.need_key.load(Ordering::Relaxed) {
                        if !hdr.is_key() {
                            want_key = true;
                            continue;
                        }
                        sub.need_key.store(false, Ordering::Relaxed);
                    }
                    if sub.tx.try_send(pkt.clone()).is_err() {
                        sub.dropped.fetch_add(1, Ordering::Relaxed);
                        if hdr.kind == MediaKind::Video {
                            sub.need_key.store(true, Ordering::Relaxed);
                            want_key = true;
                        }
                    }
                }
                if want_key {
                    request_keyframe(&ctl, &last);
                }
            }
            acceptor.abort();
        })
    };

    let res = control_loop(&state, &conn, &mut recv, id, &room).await;

    // Уборка
    media.abort();
    {
        let mut rooms = state.rooms.lock().unwrap();
        if let Some(r) = rooms.get_mut(&room) {
            r.peers.remove(&id);
            let pubs: Vec<u32> = r
                .peers
                .iter()
                .filter(|(_, p)| p.watchers.lock().unwrap().remove(&id).is_some())
                .map(|(pid, _)| *pid)
                .collect();
            for p in pubs {
                State::send_viewers(r, p);
            }
            info!("- {name} ({id}) ← {room} [{}]", r.peers.len());
            if r.peers.is_empty() {
                rooms.remove(&room);
            }
        }
    }
    state.broadcast(&room, ServerMsg::PeerLeft { id }, id);
    writer.abort();
    conn.close(0u32.into(), b"bye");
    res
}

async fn control_loop(state: &Arc<State>, conn: &quinn::Connection, recv: &mut quinn::RecvStream, id: u32, room: &str) -> Result<()> {
    while let Some(msg) = read_msg::<_, ClientMsg>(recv).await? {
        match msg {
            ClientMsg::Hello { .. } => {}
            ClientMsg::Ping { ts } => {
                let rooms = state.rooms.lock().unwrap();
                if let Some(p) = rooms.get(room).and_then(|r| r.peers.get(&id)) {
                    let _ = p.ctl.send(ServerMsg::Pong { ts });
                }
            }
            ClientMsg::StartStream { info } => {
                {
                    let mut rooms = state.rooms.lock().unwrap();
                    let Some(p) = rooms.get_mut(room).and_then(|r| r.peers.get_mut(&id)) else { continue };
                    p.stream = Some(info.clone());
                    // Если кто-то уже смотрел (перезапуск стрима) — им нужен свежий ключевой кадр
                    for s in p.watchers.lock().unwrap().values() {
                        s.need_key.store(true, Ordering::Relaxed);
                    }
                }
                info!("▶ {id} стримит {}×{}@{} {}", info.width, info.height, info.fps, info.encoder);
                state.broadcast(room, ServerMsg::StreamStarted { id, info }, id);
            }
            ClientMsg::StopStream => {
                {
                    let mut rooms = state.rooms.lock().unwrap();
                    let Some(r) = rooms.get_mut(room) else { continue };
                    if let Some(p) = r.peers.get_mut(&id) {
                        p.stream = None;
                        p.watchers.lock().unwrap().clear();
                    }
                    State::send_viewers(r, id);
                }
                info!("■ {id} закончил стрим");
                state.broadcast(room, ServerMsg::StreamStopped { id }, id);
            }
            ClientMsg::Watch { id: target } => {
                if target == id {
                    continue;
                }
                let mut rooms = state.rooms.lock().unwrap();
                let Some(r) = rooms.get_mut(room) else { continue };
                let Some(t) = r.peers.get(&target) else { continue };
                if t.stream.is_none() {
                    continue;
                }
                let (tx, rx) = mpsc::channel::<Bytes>(SUB_QUEUE);
                let sub = Sub { tx, need_key: Arc::new(AtomicBool::new(true)), dropped: Arc::new(AtomicU64::new(0)) };
                spawn_forwarder(conn.clone(), rx);
                t.watchers.lock().unwrap().insert(id, sub);
                t.last_keyreq.lock().map(|mut l| *l = Instant::now() - KEYFRAME_INTERVAL).ok();
                request_keyframe(&t.ctl, &t.last_keyreq);
                State::send_viewers(r, target);
            }
            ClientMsg::Unwatch { id: target } => {
                let mut rooms = state.rooms.lock().unwrap();
                let Some(r) = rooms.get_mut(room) else { continue };
                if let Some(t) = r.peers.get(&target) {
                    t.watchers.lock().unwrap().remove(&id);
                }
                State::send_viewers(r, target);
            }
            ClientMsg::RequestKeyframe { id: target } => {
                let rooms = state.rooms.lock().unwrap();
                if let Some(t) = rooms.get(room).and_then(|r| r.peers.get(&target)) {
                    request_keyframe(&t.ctl, &t.last_keyreq);
                }
            }
        }
    }
    Ok(())
}

/// Пересылка пакетов одной подписки зрителю: каждый пакет — свой uni-поток.
fn spawn_forwarder(conn: quinn::Connection, mut rx: mpsc::Receiver<Bytes>) {
    tokio::spawn(async move {
        while let Some(pkt) = rx.recv().await {
            let Ok(mut s) = conn.open_uni().await else { break };
            if s.write_all(&pkt).await.is_err() {
                break;
            }
            let _ = s.finish();
        }
    });
}
