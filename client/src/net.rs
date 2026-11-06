//! Сетевой контроллер: соединение с сервером, комната, стрим и просмотр.
//! Интерфейс (или самопроверка) шлёт сюда команды `Cmd` и читает состояние из `Shared`.

use crate::capture::Source;
use crate::config::{Config, Preset};
use crate::publish::{self, Publisher};
use crate::tls;
use crate::watch::{SinkMode, Viewer};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use proto::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Offline,
    Connecting,
    Online,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct PeerView {
    pub name: String,
    pub stream: Option<StreamInfo>,
    pub watching: bool,
    pub stats: String,
}

#[derive(Clone, Debug, Default)]
pub struct PubView {
    pub info: Option<StreamInfo>,
    pub viewers: Vec<String>,
    pub stats: String,
    pub fps: f64,
    pub kbps: f64,
    pub drops: u64,
    pub frames: u64,
    pub audio_packets: u64,
}

#[derive(Clone, Debug, Default)]
pub struct WatchStats {
    pub rendered: u64,
    pub received: u64,
    pub audio_rendered: u64,
    pub fps: f64,
    pub kbps: f64,
    pub margin_ms: i64,
    pub resyncs: u64,
    pub late_drops: u64,
    /// Задержка «захват → показ», мс (только в самопроверке, когда стример в том же процессе).
    pub latency_ms: i64,
    /// Перцентили опоздания звуковых пакетов относительно самого быстрого.
    pub jitter: String,
    pub decoder: String,
}

pub struct Shared {
    pub status: Status,
    pub my_id: u32,
    pub peers: BTreeMap<u32, PeerView>,
    pub starting: bool,
    pub publishing: Option<PubView>,
    pub watch: HashMap<u32, WatchStats>,
    pub rtt_ms: Option<u32>,
    pub notice: Option<String>,
    /// Новый токен портала — интерфейс сохранит его в настройки.
    pub portal_token: Option<String>,
    /// Кадры стримов для показа в окне (режим Embedded).
    pub frames: HashMap<u32, Arc<crate::watch::FrameSlot>>,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            status: Status::Offline,
            my_id: 0,
            peers: BTreeMap::new(),
            starting: false,
            publishing: None,
            watch: HashMap::new(),
            rtt_ms: None,
            notice: None,
            portal_token: None,
            frames: HashMap::new(),
        }
    }
}

pub enum Cmd {
    Connect(Config),
    Disconnect,
    Start { source: Source, preset: Preset, audio: bool, encoder: String, portal_token: Option<String> },
    Stop,
    Watch(u32),
    Unwatch(u32),
    Volume(u32, f64),
    /// Только для самопроверки: оборвать соединение, как при сбое сети.
    DebugDrop,
    /// Размер видео в окне (в пикселях), чтобы не гонять лишнее.
    DisplaySize(u32, u32, u32),
}

pub type SharedRef = Arc<Mutex<Shared>>;
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

pub fn spawn(rt: &tokio::runtime::Handle, shared: SharedRef, repaint: Repaint, mode: SinkMode) -> mpsc::UnboundedSender<Cmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    // Слабая ссылка на свой же канал: контроллер сам себе шлёт команды (восстановление после обрыва),
    // но не мешает каналу закрыться, когда окно закрыли.
    rt.spawn(controller(rx, tx.downgrade(), shared, repaint, mode));
    tx
}

/// Что было запущено — чтобы после обрыва связи всё вернулось само.
#[derive(Default)]
struct Resume {
    /// Параметры стрима, если стримили.
    publish: Option<(Source, Preset, bool, String, Option<String>)>,
    /// Кого смотрели (по имени: после переподключения id другие).
    watch: std::collections::HashSet<String>,
}

enum Exit {
    User,
    Closed,
}

/// Ошибка, при которой переподключаться бессмысленно (неверный пароль, полная комната…).
#[derive(Debug)]
struct Fatal(String);
impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Fatal {}

async fn controller(mut rx: mpsc::UnboundedReceiver<Cmd>, me: mpsc::WeakUnboundedSender<Cmd>, shared: SharedRef, repaint: Repaint, mode: SinkMode) {
    loop {
        let mut resume = Resume::default();
        let cfg = loop {
            match rx.recv().await {
                Some(Cmd::Connect(c)) => break c,
                Some(_) => continue,
                None => return,
            }
        };
        loop {
            set(&shared, &repaint, |s| s.status = Status::Connecting);
            let r = session(&cfg, &mut rx, &me, &mut resume, &shared, &repaint, mode).await;
            reset(&shared);
            match r {
                Ok(Exit::User) => {
                    set(&shared, &repaint, |s| s.status = Status::Offline);
                    break;
                }
                Ok(Exit::Closed) => return,
                Err(e) => {
                    let fatal = e.downcast_ref::<Fatal>().is_some();
                    let text = format!("{e:#}");
                    warn!("соединение: {text}");
                    set(&shared, &repaint, |s| s.status = Status::Error(text.clone()));
                    if fatal {
                        break;
                    }
                    // Переподключение через 3 с, если пользователь не нажал «Отключиться»
                    let wait = tokio::time::sleep(Duration::from_secs(3));
                    tokio::pin!(wait);
                    let mut user_left = false;
                    loop {
                        tokio::select! {
                            _ = &mut wait => break,
                            c = rx.recv() => match c {
                                Some(Cmd::Disconnect) => { user_left = true; break }
                                None => return,
                                _ => {}
                            }
                        }
                    }
                    if user_left {
                        set(&shared, &repaint, |s| s.status = Status::Offline);
                        break;
                    }
                }
            }
        }
    }
}

fn set(shared: &SharedRef, repaint: &Repaint, f: impl FnOnce(&mut Shared)) {
    f(&mut shared.lock().unwrap());
    repaint();
}

fn reset(shared: &SharedRef) {
    let mut s = shared.lock().unwrap();
    s.peers.clear();
    s.publishing = None;
    s.starting = false;
    s.watch.clear();
    s.frames.clear();
    s.rtt_ms = None;
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

type Viewers = Arc<Mutex<HashMap<u32, Arc<Viewer>>>>;

#[allow(clippy::too_many_arguments)]
async fn session(
    cfg: &Config,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    me: &mpsc::WeakUnboundedSender<Cmd>,
    resume: &mut Resume,
    shared: &SharedRef,
    repaint: &Repaint,
    mode: SinkMode,
) -> Result<Exit> {
    let me = me.clone();
    let self_send = move |c: Cmd| {
        if let Some(tx) = me.upgrade() {
            let _ = tx.send(c);
        }
    };
    anyhow::ensure!(!cfg.server.is_empty(), Fatal("Не указан адрес сервера".into()));
    let addr = tokio::net::lookup_host(&cfg.server)
        .await
        .with_context(|| format!("не найден сервер {}", cfg.server))?
        .min_by_key(|a| a.is_ipv6())
        .ok_or_else(|| anyhow!("не найден сервер {}", cfg.server))?;
    let bind: std::net::SocketAddr = if addr.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(tls::client_config(&cfg.fingerprint).map_err(|e| Fatal(format!("{e:#}")))?);
    let conn = tokio::time::timeout(Duration::from_secs(8), endpoint.connect(addr, "hyst-cast")?)
        .await
        .map_err(|_| anyhow!("сервер не отвечает — проверь интернет; если Windows спросил про брандмауэр, нажми «Разрешить» (повторяю через 3 с)"))?
        .map_err(|e| {
            if e.to_string().contains("сертификат") || e.to_string().contains("certificate") {
                anyhow::Error::new(Fatal(format!("Сервер не тот: {e}")))
            } else {
                anyhow!("не удалось подключиться: {e}")
            }
        })?;
    info!("подключено к {addr}");

    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &ClientMsg::Hello { version: PROTOCOL_VERSION, token: cfg.token.clone(), name: cfg.name.clone(), room: cfg.room.clone() }).await?;
    let first: ServerMsg = read_msg(&mut recv).await?.context("сервер закрыл соединение")?;
    let my_id = match first {
        ServerMsg::Welcome { id, peers, .. } => {
            let mut s = shared.lock().unwrap();
            s.status = Status::Online;
            s.my_id = id;
            s.peers = peers.into_iter().map(|p| (p.id, PeerView { name: p.name, stream: p.stream, watching: false, stats: String::new() })).collect();
            s.notice = None;
            id
        }
        ServerMsg::Error { message } => return Err(Fatal(message).into()),
        other => return Err(anyhow!("неожиданный ответ сервера: {other:?}")),
    };
    repaint();

    // После обрыва связи — вернуть всё как было
    if let Some((source, preset, audio, encoder, portal_token)) = resume.publish.clone() {
        info!("восстанавливаю стрим после переподключения");
        self_send(Cmd::Start { source, preset, audio, encoder, portal_token });
    }
    let rewatch: Vec<u32> = {
        let s = shared.lock().unwrap();
        s.peers.iter().filter(|(_, p)| p.stream.is_some() && resume.watch.contains(&p.name)).map(|(id, _)| *id).collect()
    };
    for id in rewatch {
        info!("восстанавливаю просмотр {id}");
        self_send(Cmd::Watch(id));
    }
    let mut last_log = Instant::now();

    // Отправка управляющих сообщений
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if write_msg(&mut send, &m).await.is_err() {
                break;
            }
        }
    });
    // Приём управляющих сообщений
    let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let reader = tokio::spawn(async move {
        while let Ok(Some(m)) = read_msg::<_, ServerMsg>(&mut recv).await {
            if srv_tx.send(m).is_err() {
                break;
            }
        }
    });
    // Приём медиа: читаем параллельно, раздаём по порядку
    let viewers: Viewers = Arc::new(Mutex::new(HashMap::new()));
    let media = {
        let conn = conn.clone();
        let viewers = viewers.clone();
        tokio::spawn(async move {
            let (otx, mut orx) = mpsc::channel::<tokio::task::JoinHandle<Option<Vec<u8>>>>(256);
            let acceptor = tokio::spawn(async move {
                while let Ok(mut uni) = conn.accept_uni().await {
                    let h = tokio::spawn(async move { uni.read_to_end(MAX_MEDIA).await.ok() });
                    if otx.send(h).await.is_err() {
                        break;
                    }
                }
            });
            while let Some(h) = orx.recv().await {
                let Ok(Some(buf)) = h.await else { continue };
                let Ok((hdr, _)) = MediaHeader::parse(&buf) else { continue };
                let v = viewers.lock().unwrap().get(&hdr.publisher).cloned();
                if let Some(v) = v {
                    let data = Bytes::from(buf).slice(MediaHeader::LEN..);
                    v.push(hdr.kind, hdr.is_key(), hdr.pts_us, data);
                }
            }
            acceptor.abort();
        })
    };

    let mut publisher: Option<Publisher> = None;
    let mut sender: Option<tokio::task::JoinHandle<()>> = None;
    let (start_tx, mut start_rx) = mpsc::unbounded_channel::<Result<Publisher>>();
    let (vw_tx, mut vw_rx) = mpsc::unbounded_channel::<(u32, Result<Viewer>)>();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut last_ping = Instant::now() - Duration::from_secs(10);
    let mut prev: HashMap<u32, (Instant, u64, u64)> = HashMap::new();
    let mut volumes: HashMap<u32, f64> = HashMap::new();

    let result = loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break Ok(Exit::Closed),
                Some(Cmd::Disconnect) => break Ok(Exit::User),
                Some(Cmd::Connect(_)) => {}
                Some(Cmd::Start { source, preset, audio, encoder, portal_token }) => {
                    if publisher.is_some() || shared.lock().unwrap().starting { continue }
                    resume.publish = Some((source.clone(), preset, audio, encoder.clone(), portal_token.clone()));
                    set(shared, repaint, |s| { s.starting = true; s.notice = None; });
                    let (ptx, prx) = mpsc::channel::<publish::Out>(publish::SEND_QUEUE);
                    let st = start_tx.clone();
                    let conn2 = conn.clone();
                    let link = Arc::new(publish::Link::default());
                    tokio::spawn(async move {
                        let r = publish::start(source, preset, audio, encoder, portal_token, ptx, link.clone()).await;
                        if let Ok(p) = &r {
                            // отправитель запускается здесь, чтобы не потерять первые кадры
                            tokio::spawn(send_media(conn2, prx, link, p.encoder_element()));
                        }
                        let _ = st.send(r);
                    });
                }
                Some(Cmd::Stop) => {
                    resume.publish = None;
                    if publisher.take().is_some() {
                        let _ = ctl_tx.send(ClientMsg::StopStream);
                    }
                    if let Some(s) = sender.take() { s.abort(); }
                    set(shared, repaint, |s| s.publishing = None);
                }
                Some(Cmd::Watch(id)) => {
                    let info = shared.lock().unwrap().peers.get(&id).and_then(|p| p.stream.clone().map(|s| (p.name.clone(), s)));
                    let Some((name, info)) = info else { continue };
                    if viewers.lock().unwrap().contains_key(&id) { continue }
                    resume.watch.insert(name.clone());
                    prev.remove(&id);
                    set(shared, repaint, |s| if let Some(p) = s.peers.get_mut(&id) { p.watching = true; p.stats = "подключение…".into(); });
                    let latency = cfg.latency_ms;
                    let vt = vw_tx.clone();
                    let slot = (mode == SinkMode::Embedded).then(|| crate::watch::FrameSlot::new(repaint.clone()));
                    if let Some(sl) = &slot {
                        shared.lock().unwrap().frames.insert(id, sl.clone());
                    }
                    tokio::task::spawn_blocking(move || {
                        let r = Viewer::new(&format!("{name} — hyst-cast"), info.has_audio, latency, mode, (info.width, info.height), slot);
                        let _ = vt.send((id, r));
                    });
                }
                Some(Cmd::Unwatch(id)) => {
                    if let Some(n) = shared.lock().unwrap().peers.get(&id).map(|p| p.name.clone()) { resume.watch.remove(&n); }
                    viewers.lock().unwrap().remove(&id);
                    let _ = ctl_tx.send(ClientMsg::Unwatch { id });
                    set(shared, repaint, |s| { if let Some(p) = s.peers.get_mut(&id) { p.watching = false; p.stats.clear(); } s.watch.remove(&id); s.frames.remove(&id); });
                }
                Some(Cmd::DebugDrop) => break Err(anyhow!("тест: обрыв связи")),
                Some(Cmd::DisplaySize(id, w, h)) => {
                    if let Some(vw) = viewers.lock().unwrap().get(&id) { vw.set_display_size(w, h); }
                }
                Some(Cmd::Volume(id, v)) => {
                    volumes.insert(id, v);
                    if let Some(vw) = viewers.lock().unwrap().get(&id) { vw.set_volume(v); }
                }
            },

            msg = srv_rx.recv() => {
                let Some(msg) = msg else { break Err(anyhow!("связь с сервером потеряна")) };
                match msg {
                    ServerMsg::PeerJoined { peer } => set(shared, repaint, |s| { s.peers.insert(peer.id, PeerView { name: peer.name, stream: peer.stream, watching: false, stats: String::new() }); }),
                    ServerMsg::PeerLeft { id } => {
                        viewers.lock().unwrap().remove(&id);
                        set(shared, repaint, |s| { s.peers.remove(&id); s.watch.remove(&id); s.frames.remove(&id); });
                    }
                    ServerMsg::StreamStarted { id, info } => {
                        let watching = viewers.lock().unwrap().contains_key(&id);
                        if watching {
                            // Стример перезапустил стрим — пересоздаём окно под новые параметры
                            viewers.lock().unwrap().remove(&id);
                            let _ = ctl_tx.send(ClientMsg::Unwatch { id });
                        }
                        let name = shared.lock().unwrap().peers.get(&id).map(|p| p.name.clone()).unwrap_or_default();
                        set(shared, repaint, |s| if let Some(p) = s.peers.get_mut(&id) { p.stream = Some(info.clone()); p.watching = false; });
                        // Смотрели этого человека раньше (перезапуск стрима, переподключение) — открываем сами
                        if watching || resume.watch.contains(&name) {
                            self_send(Cmd::Watch(id));
                        }
                    }
                    ServerMsg::StreamStopped { id } => {
                        viewers.lock().unwrap().remove(&id);
                        set(shared, repaint, |s| { if let Some(p) = s.peers.get_mut(&id) { p.stream = None; p.watching = false; p.stats.clear(); } s.watch.remove(&id); s.frames.remove(&id); });
                    }
                    ServerMsg::KeyframeRequest => if let Some(p) = &publisher { p.force_keyframe(); },
                    ServerMsg::SlowViewer { name } => if let Some(p) = publisher.as_mut() { p.slow_viewer(&name); },
                    ServerMsg::Viewers { names } => set(shared, repaint, |s| if let Some(p) = s.publishing.as_mut() { p.viewers = names; }),
                    ServerMsg::Pong { ts } => set(shared, repaint, |s| s.rtt_ms = Some(now_ms().saturating_sub(ts) as u32)),
                    ServerMsg::Error { message } => set(shared, repaint, |s| s.notice = Some(message)),
                    ServerMsg::Welcome { .. } => {}
                }
            },

            r = start_rx.recv() => {
                let Some(r) = r else { continue };
                match r {
                    Ok(p) => {
                        info!("стрим начат: {:?}", p.info);
                        let _ = ctl_tx.send(ClientMsg::StartStream { info: p.info.clone() });
                        let tok = p.portal_token.clone();
                        set(shared, repaint, |s| {
                            s.starting = false;
                            s.publishing = Some(PubView { info: Some(p.info.clone()), ..Default::default() });
                            if tok.is_some() { s.portal_token = tok; }
                        });
                        publisher = Some(p);
                    }
                    Err(e) => set(shared, repaint, |s| { s.starting = false; s.notice = Some(format!("Стрим не запустился: {e:#}")); }),
                }
            },

            r = vw_rx.recv() => {
                let Some((id, r)) = r else { continue };
                let still_wanted = shared.lock().unwrap().peers.get(&id).is_some_and(|p| p.watching && p.stream.is_some());
                match r {
                    Ok(v) if still_wanted => {
                        if let Some(vol) = volumes.get(&id) { v.set_volume(*vol); }
                        let dec = v.decoder.clone();
                        viewers.lock().unwrap().insert(id, Arc::new(v));
                        let _ = ctl_tx.send(ClientMsg::Watch { id });
                        set(shared, repaint, |s| { s.watch.insert(id, WatchStats { decoder: dec, ..Default::default() }); });
                    }
                    Ok(_) => {}
                    Err(e) => set(shared, repaint, |s| { if let Some(p) = s.peers.get_mut(&id) { p.watching = false; } s.notice = Some(format!("Не получилось открыть стрим: {e:#}")); }),
                }
            },

            _ = tick.tick() => {
                if last_ping.elapsed() >= Duration::from_secs(2) {
                    last_ping = Instant::now();
                    let _ = ctl_tx.send(ClientMsg::Ping { ts: now_ms() });
                }
                // Стример
                if let Some(p) = publisher.as_mut() {
                    p.tick();
                    if let Some(err) = p.error() {
                        publisher = None;
                        let _ = ctl_tx.send(ClientMsg::StopStream);
                        set(shared, repaint, |s| { s.publishing = None; s.notice = Some(format!("Стрим остановлен: {err}")); });
                    } else {
                        let (fps, kbps, drops, frames, ap) = (p.fps_now, p.kbps_now, p.drops(), p.frames(), p.audio_packets());
                        let target = p.kbps.load(Ordering::Relaxed);
                        let enc = p.encoder.label();
                        let limited = if target < p.info_target_kbps() { format!(" · снижено (канал{})", if p.slow_name.is_empty() { String::new() } else { format!(" у {}", p.slow_name) }) } else { String::new() };
                        set(shared, repaint, |s| if let Some(pv) = s.publishing.as_mut() {
                            pv.fps = fps; pv.kbps = kbps; pv.drops = drops; pv.frames = frames; pv.audio_packets = ap;
                            pv.stats = format!("{fps:.0} fps · {:.1} Мбит/с (цель {:.1}){limited} · {enc}", kbps / 1000.0, target as f64 / 1000.0);
                        });
                    }
                }
                // Зрители
                let list: Vec<(u32, Arc<Viewer>)> = viewers.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect();
                for (id, v) in list {
                    if let Some(why) = v.closed.lock().unwrap().clone() {
                        viewers.lock().unwrap().remove(&id);
                        let _ = ctl_tx.send(ClientMsg::Unwatch { id });
                        let closed_by_user = why.to_lowercase().contains("closed") || why.to_lowercase().contains("закрыт");
                        if let Some(n) = shared.lock().unwrap().peers.get(&id).map(|p| p.name.clone()) { resume.watch.remove(&n); }
                        set(shared, repaint, |s| {
                            if let Some(p) = s.peers.get_mut(&id) { p.watching = false; p.stats.clear(); }
                            s.watch.remove(&id); s.frames.remove(&id);
                            if !closed_by_user { s.notice = Some(format!("Просмотр прерван: {why}")); }
                        });
                        continue;
                    }
                    let st = &v.stats;
                    let (rendered, received, bytes) = (st.rendered.load(Ordering::Relaxed), st.received.load(Ordering::Relaxed), st.bytes.load(Ordering::Relaxed));
                    let e = prev.entry(id).or_insert((Instant::now(), rendered, bytes));
                    let dt = e.0.elapsed().as_secs_f64();
                    if dt < 1.0 { continue }
                    let fps = rendered.saturating_sub(e.1) as f64 / dt;
                    let kbps = bytes.saturating_sub(e.2) as f64 * 8.0 / dt / 1000.0;
                    *e = (Instant::now(), rendered, bytes);
                    let margin = st.margin_ms.load(Ordering::Relaxed);
                    let resyncs = st.resyncs.load(Ordering::Relaxed);
                    let audio_rendered = st.audio_rendered.load(Ordering::Relaxed);
                    let latency_ms = st.latency_ms.load(Ordering::Relaxed);
                    let jitter = {
                        let mut j = st.jitter_samples.lock().unwrap().clone();
                        if j.len() > 50 {
                            j.sort_unstable();
                            let min = j[0];
                            let p = |q: f64| j[((j.len() - 1) as f64 * q) as usize] - min;
                            format!("50% ≤{} мс, 95% ≤{} мс, 99% ≤{} мс, макс {} мс", p(0.5), p(0.95), p(0.99), p(1.0))
                        } else {
                            String::new()
                        }
                    };
                    set(shared, repaint, |s| {
                        let dec = s.watch.get(&id).map(|w| w.decoder.clone()).unwrap_or_default();
                        if let Some(p) = s.peers.get_mut(&id) {
                            p.stats = format!("{fps:.0} fps · {:.1} Мбит/с · запас {margin} мс · {dec}", kbps / 1000.0);
                        }
                        s.watch.insert(id, WatchStats { rendered, received, audio_rendered, fps, kbps, margin_ms: margin, resyncs, late_drops: st.late_drops.load(Ordering::Relaxed), latency_ms, jitter, decoder: dec });
                    });
                }
                let _ = my_id;
                // Раз в 30 с — статистика в лог (по логу друга будет видно, что происходило)
                if last_log.elapsed() >= Duration::from_secs(30) {
                    last_log = Instant::now();
                    let s = shared.lock().unwrap();
                    if let Some(p) = &s.publishing { info!("статистика стрима: {} · зрители: {:?}", p.stats, p.viewers); }
                    for (id, w) in &s.watch {
                        info!("статистика просмотра {id}: {:.0} fps, {:.1} Мбит/с, запас {} мс, пересинхронизаций {}, задержка {} мс, {}",
                            w.fps, w.kbps / 1000.0, w.margin_ms, w.resyncs, w.latency_ms, w.decoder);
                    }
                    if let Some(r) = s.rtt_ms { info!("пинг до сервера: {r} мс"); }
                }
            }
        }
    };

    if let Some(s) = sender.take() {
        s.abort();
    }
    drop(publisher);
    viewers.lock().unwrap().clear();
    media.abort();
    reader.abort();
    writer.abort();
    conn.close(0u32.into(), b"bye");
    endpoint.wait_idle().await;
    result
}

/// Отправка пакетов стрима: каждый пакет — свой uni-поток, строго по порядку.
/// Звук идёт с приоритетом и обгоняет видео. Застрявшее в очереди видео выкидывается до ключевого кадра:
/// лучше пропустить полсекунды, чем копить задержку.
async fn send_media(conn: quinn::Connection, mut rx: mpsc::Receiver<publish::Out>, link: Arc<publish::Link>, enc: gstreamer::Element) {
    use gstreamer::prelude::*;
    const VIDEO_MAX_AGE: Duration = Duration::from_millis(250);
    const AUDIO_MAX_AGE: Duration = Duration::from_millis(400);
    let mut last_kf = Instant::now() - Duration::from_secs(1);
    while let Some(o) = rx.recv().await {
        let age = o.t.elapsed();
        if o.video {
            if link.need_key.load(Ordering::Relaxed) && !o.key {
                link.stale.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if o.key {
                link.need_key.store(false, Ordering::Relaxed);
            } else if age > VIDEO_MAX_AGE {
                link.stale.fetch_add(1, Ordering::Relaxed);
                link.need_key.store(true, Ordering::Relaxed);
                if last_kf.elapsed() > Duration::from_millis(300) {
                    last_kf = Instant::now();
                    if let Some(pad) = enc.static_pad("src") {
                        pad.send_event(gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build());
                    }
                }
                continue;
            }
        } else if age > AUDIO_MAX_AGE {
            link.stale.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Ok(mut s) = conn.open_uni().await else { break };
        let _ = s.set_priority(if o.video { 0 } else { 10 });
        if s.write_all(&o.pkt).await.is_err() {
            break;
        }
        let _ = s.finish();
        if o.video {
            link.rtt_us.store(conn.rtt().as_micros() as u64, Ordering::Relaxed);
            let d = o.t.elapsed().as_micros() as u64;
            let old = link.delay_us.load(Ordering::Relaxed);
            link.delay_us.store((old * 7 + d) / 8, Ordering::Relaxed);
        }
    }
}
