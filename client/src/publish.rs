//! Стример: захват → (масштабирование) → H.264 + Opus → пакеты в очередь на отправку.

use crate::capture::{self, Capture, Source};
use crate::codecs::{self, Encoder};
use crate::config::Preset;
use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gst::prelude::*;
use proto::{MediaHeader, MediaKind, StreamInfo, FLAG_KEY};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Очередь пакетов на отправку (~1 с при 60 fps + звук). Переполнилась — канал не тянет.
pub const SEND_QUEUE: usize = 120;

/// Базовое время конвейера стримера (нс, системные часы) — для точного замера задержки в самопроверке,
/// где стример и зритель в одном процессе и часы у них общие.
pub static SENDER_BASE_NS: AtomicU64 = AtomicU64::new(0);

/// Пакет в очереди на отправку.
pub struct Out {
    pub pkt: Vec<u8>,
    pub video: bool,
    pub key: bool,
    pub t: Instant,
}

/// Общее между кодером и отправителем: ожидание ключевого кадра и признаки затора.
#[derive(Default)]
pub struct Link {
    pub need_key: AtomicBool,
    pub want_key: AtomicBool,
    /// Сглаженное время «кадр готов → сдан в QUIC», мкс. Растёт — канал не тянет.
    pub delay_us: AtomicU64,
    /// Выкинуто отправителем как устаревшее.
    pub stale: AtomicU64,
    /// Текущий сглаженный RTT соединения с сервером, мкс (растёт, когда копится очередь в роутере).
    pub rtt_us: AtomicU64,
}

#[derive(Default)]
pub struct PubStats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub drops: AtomicU64,
    pub audio_packets: AtomicU64,
}

struct Shared {
    tx: mpsc::Sender<Out>,
    link: Arc<Link>,
    stats: PubStats,
    first_frame: AtomicBool,
}

pub struct Publisher {
    pipeline: gst::Pipeline,
    enc: gst::Element,
    pub encoder: Encoder,
    pub info: StreamInfo,
    shared: Arc<Shared>,
    error: Arc<Mutex<Option<String>>>,
    stop_bus: Arc<AtomicBool>,
    target_kbps: u32,
    pub kbps: AtomicU32,
    last_drops: u64,
    /// Минимальный RTT за последнее время — «пустой канал». Медленно забывается.
    min_rtt_us: u64,
    min_rtt_at: Instant,
    /// Битрейт, выше которого канал уже не справлялся (медленно отпускается).
    ceiling_kbps: u32,
    ceiling_at: Instant,
    calm: u32,
    last: (Instant, u64, u64),
    pub fps_now: f64,
    pub kbps_now: f64,
    pub portal_token: Option<String>,
    /// Что за звук идёт в стрим («весь звук, кроме Discord», «только звук этого окна»…).
    pub audio_hint: String,
    slow_viewers: u32,
    pub slow_name: String,
    _capture: Option<Capture>,
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.stop_bus.store(true, Ordering::Relaxed);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub async fn start(
    source: Source,
    preset: Preset,
    audio: bool,
    force_encoder: String,
    portal_token: Option<String>,
    tx: mpsc::Sender<Out>,
    link: Arc<Link>,
    preview: Option<Arc<crate::watch::FrameSlot>>,
) -> Result<Publisher> {
    let capture = capture::open(&source, preset.fps, (preset.width, preset.height), portal_token).await?;
    let akind = if source == Source::Test && std::env::var("HYST_CAST_SYSTEM_AUDIO").is_ok() {
        capture::AudioKind::System
    } else if source == Source::Test {
        capture::AudioKind::Test
    } else if source.is_camera() {
        capture::AudioKind::Mic
    } else {
        capture::AudioKind::System
    };
    // Камера со своим размером: кодируем в нём, битрейт — пропорционально площади кадра
    let mut preset = preset;
    if let Some((w, h)) = capture.size {
        let ratio = (w * h) as f64 / (preset.width * preset.height) as f64;
        preset.kbps = ((preset.kbps as f64 * ratio.clamp(0.35, 1.0)) as u32).max(800);
        preset.width = w;
        preset.height = h;
    }
    let video_desc = capture.video_desc.clone();
    let src2 = source.clone();
    let (encoders, audio_ok) = tokio::task::spawn_blocking(move || {
        let enc = codecs::working_encoders(&force_encoder);
        // Первый вариант звука, который реально работает (захват по процессам может не поддерживаться)
        let a = if audio {
            capture::audio_candidates(akind, &src2).into_iter().find(|(d, _)| probe_audio(d))
        } else {
            None
        };
        (enc, a)
    })
    .await?;
    if audio && audio_ok.is_none() {
        warn!("звук системы не захватывается — стрим пойдёт без звука");
    }
    if encoders.is_empty() {
        bail!("Не найден ни один рабочий кодер H.264");
    }
    let mut last_err = None;
    for enc in encoders {
        let vd = video_desc.clone();
        let tx = tx.clone();
        let e2 = enc.clone();
        let link = link.clone();
        let ad = audio_ok.as_ref().map(|a| a.0.clone());
        let pv = preview.clone();
        let r = tokio::task::spawn_blocking(move || {
            // Сначала с превью для себя; если на этом железе ветка превью не заводится — без него
            match try_start(&vd, &e2, preset, ad.clone(), tx.clone(), link.clone(), pv.clone()) {
                Err(e) if pv.is_some() => {
                    warn!("превью не завелось ({e:#}) — стрим без превью");
                    try_start(&vd, &e2, preset, ad, tx, link, None)
                }
                r => r,
            }
        })
        .await?;
        match r {
            Ok(mut p) => {
                p.portal_token = capture.portal_token.clone();
                p.audio_hint = audio_ok.as_ref().map(|a| a.1.clone()).unwrap_or_else(|| "без звука".into());
                p._capture = Some(capture);
                return Ok(p);
            }
            Err(e) => {
                warn!("кодер {} не запустился: {e:#}", enc.factory);
                // Ошибка в самом захвате (экран/камера) — другие кодеры не помогут
                let text = format!("{e:#}");
                let src_failed = ["d3d11screencapturesrc", "pipewiresrc", "mfvideosrc", "v4l2src"].iter().any(|n| text.contains(&format!(":{n}")) || text.contains(&format!("Gst{}", n)));
                last_err = Some(e);
                if src_failed {
                    break;
                }
            }
        }
    }
    Err(last_err.unwrap()).context("Ни один кодер не смог запуститься")
}

pub fn probe_audio(desc: &str) -> bool {
    if desc == capture::PW_APPS {
        // Проверяем, что pw-dump отвечает: дальше источники подключаются по ходу
        return cfg!(target_os = "linux") && std::process::Command::new("pw-dump").output().is_ok_and(|o| o.status.success());
    }
    let Ok(el) = gst::parse::launch(&format!("{desc} ! fakesink name=s")) else { return false };
    let Ok(p) = el.downcast::<gst::Pipeline>() else { return false };
    let got = Arc::new(AtomicBool::new(false));
    if let Some(pad) = p.by_name("s").and_then(|s| s.static_pad("sink")) {
        let g = got.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            g.store(true, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    if p.set_state(gst::State::Playing).is_err() {
        let _ = p.set_state(gst::State::Null);
        return false;
    }
    let t = Instant::now();
    let bus = p.bus().unwrap();
    let mut ok = false;
    while t.elapsed() < Duration::from_secs(3) {
        if got.load(Ordering::Relaxed) {
            ok = true;
            break;
        }
        if let Some(m) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(50), &[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = m.view() {
                warn!("звук: {}", e.error());
            }
            break;
        }
    }
    let _ = p.set_state(gst::State::Null);
    ok
}

fn try_start(
    video_desc: &str,
    encoder: &Encoder,
    preset: Preset,
    audio_desc: Option<String>,
    tx: mpsc::Sender<Out>,
    link: Arc<Link>,
    preview: Option<Arc<crate::watch::FrameSlot>>,
) -> Result<Publisher> {
    let audio = audio_desc.is_some();
    // С превью: после масштабирования tee — одна ветка в кодер, вторая — маленький RGBA для своего окна
    let tee = if preview.is_some() { "tee name=pt ! queue max-size-buffers=2 max-size-time=0 max-size-bytes=0 ! " } else { "" };
    let mut desc = format!(
        "{video_desc} ! queue max-size-buffers=3 max-size-time=0 max-size-bytes=0 leaky=downstream ! {} ! {tee}{} name=enc ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=vsink sync=false max-buffers=8",
        encoder.convert_desc(preset.width, preset.height, video_desc.contains("D3D11Memory")),
        encoder.factory
    );
    if preview.is_some() {
        let pw = 640u32;
        let ph = ((pw as f64 * preset.height as f64 / preset.width as f64) as u32) & !1;
        desc.push_str(&format!(
            " pt. ! queue leaky=downstream max-size-buffers=1 max-size-time=0 max-size-bytes=0 ! videorate drop-only=true max-rate=15 ! {} ! appsink name=pview sync=false max-buffers=1 drop=true",
            encoder.preview_desc(pw, ph)
        ));
    }
    if audio {
        desc.push_str(&format!(
            " {} ! queue max-size-time=200000000 max-size-buffers=0 max-size-bytes=0 leaky=downstream ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=128000 frame-size=20 audio-type=generic ! appsink name=asink sync=false max-buffers=50",
            if audio_desc.as_deref() == Some(capture::PW_APPS) {
                // Тишина держит микшер «живым», приложения подключаются к amix на лету
                "audiotestsrc is-live=true wave=silence ! audio/x-raw,rate=48000,channels=2 ! audiomixer name=amix latency=50000000"
            } else {
                audio_desc.as_deref().unwrap_or_default()
            }
        ));
    }
    info!("конвейер стрима: {desc}");
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("не конвейер"))?;
    let enc = pipeline.by_name("enc").context("нет кодера")?;
    if let (Some(slot), Some(pv)) = (preview, pipeline.by_name("pview")) {
        if let Ok(sink) = pv.downcast::<gst_app::AppSink>() {
            crate::watch::attach_frame_sink(&sink, slot);
        }
    }
    encoder.configure(&enc, preset.kbps, preset.fps);

    let shared = Arc::new(Shared {
        tx,
        link: {
            link.need_key.store(true, Ordering::Relaxed);
            link
        },
        stats: PubStats::default(),
        first_frame: AtomicBool::new(false),
    });

    let vsink = pipeline.by_name("vsink").unwrap().downcast::<gst_app::AppSink>().unwrap();
    let sh = shared.clone();
    vsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                on_sample(&sh, MediaKind::Video, &sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    if audio {
        let asink = pipeline.by_name("asink").unwrap().downcast::<gst_app::AppSink>().unwrap();
        let sh = shared.clone();
        asink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    on_sample(&sh, MediaKind::Audio, &sample);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        // Подробная причина лежит в шине (камера занята, нет доступа и т. п.)
        let detail = pipeline
            .bus()
            .and_then(|b| b.timed_pop_filtered(gst::ClockTime::from_mseconds(200), &[gst::MessageType::Error]))
            .and_then(|m| match m.view() {
                gst::MessageView::Error(err) => Some(format!("{} ({})", err.error(), err.debug().unwrap_or_default())),
                _ => None,
            })
            .unwrap_or_else(|| e.to_string());
        let _ = pipeline.set_state(gst::State::Null);
        bail!("конвейер не запустился: {detail}");
    }

    // Ждём первый закодированный кадр — тогда кодер точно работает
    let bus = pipeline.bus().unwrap();
    let t = Instant::now();
    loop {
        if shared.first_frame.load(Ordering::Relaxed) {
            break;
        }
        if t.elapsed() > Duration::from_secs(8) {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("за 8 с не пришло ни одного кадра");
        }
        if let Some(m) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(50), &[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = m.view() {
                let msg = format!("{} ({})", e.error(), e.debug().unwrap_or_default());
                let _ = pipeline.set_state(gst::State::Null);
                bail!(msg);
            }
        }
    }
    info!("стрим идёт: кодер {} за {} мс", encoder.label(), t.elapsed().as_millis());
    SENDER_BASE_NS.store(pipeline.base_time().map(|b| b.nseconds()).unwrap_or(0), Ordering::Relaxed);
    let apps_watch = audio_desc.as_deref() == Some(capture::PW_APPS);

    // Дальше ошибки конвейера ловит отдельный поток
    let error = Arc::new(Mutex::new(None));
    let stop_bus = Arc::new(AtomicBool::new(false));
    {
        let error = error.clone();
        let stop = stop_bus.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(m) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(250), &[gst::MessageType::Error, gst::MessageType::Eos]) {
                    // Ошибки ветки отдельного приложения (оно закрылось) — не повод останавливать стрим
                    if m.src().is_some_and(|s| s.path_string().contains(":app_")) {
                        info!("звук приложения отключился: {:?}", m.src().map(|s| s.path_string()));
                        continue;
                    }
                    let text = match m.view() {
                        gst::MessageView::Error(e) => format!("{}", e.error()),
                        _ => "источник картинки закрылся".to_string(),
                    };
                    warn!("стрим: {text}");
                    *error.lock().unwrap() = Some(text);
                    break;
                }
            }
        });
    }

    #[cfg(target_os = "linux")]
    if apps_watch {
        spawn_app_mixer(pipeline.clone(), stop_bus.clone());
    }
    let _ = apps_watch;
    Ok(Publisher {
        pipeline,
        enc,
        encoder: encoder.clone(),
        info: StreamInfo { width: preset.width, height: preset.height, fps: preset.fps, has_audio: audio, encoder: encoder.label() },
        shared,
        error,
        stop_bus,
        target_kbps: preset.kbps,
        kbps: AtomicU32::new(preset.kbps),
        last_drops: 0,
        min_rtt_us: u64::MAX,
        min_rtt_at: Instant::now(),
        ceiling_kbps: preset.kbps,
        ceiling_at: Instant::now(),
        calm: 0,
        last: (Instant::now(), 0, 0),
        fps_now: 0.0,
        kbps_now: 0.0,
        portal_token: None,
        audio_hint: String::new(),
        slow_viewers: 0,
        slow_name: String::new(),
        _capture: None,
    })
}

fn on_sample(sh: &Shared, kind: MediaKind, sample: &gst::Sample) {
    let Some(buf) = sample.buffer() else { return };
    let rt = sample
        .segment()
        .and_then(|s| s.downcast_ref::<gst::ClockTime>().cloned())
        .and_then(|seg| seg.to_running_time(buf.pts()))
        .or(buf.pts());
    let pts_us = rt.map(|t| t.useconds()).unwrap_or(0);
    let key = kind == MediaKind::Audio || !buf.flags().contains(gst::BufferFlags::DELTA_UNIT);
    let Ok(map) = buf.map_readable() else { return };

    if kind == MediaKind::Video {
        sh.first_frame.store(true, Ordering::Relaxed);
        if sh.link.need_key.load(Ordering::Relaxed) {
            if !key {
                sh.stats.drops.fetch_add(1, Ordering::Relaxed);
                sh.link.want_key.store(true, Ordering::Relaxed);
                return;
            }
            sh.link.need_key.store(false, Ordering::Relaxed);
        }
    }

    let mut pkt = Vec::with_capacity(MediaHeader::LEN + map.len());
    MediaHeader { publisher: 0, kind, flags: if key { FLAG_KEY } else { 0 }, pts_us }.write(&mut pkt);
    pkt.extend_from_slice(map.as_slice());
    let len = pkt.len() as u64;
    let video = kind == MediaKind::Video;
    match sh.tx.try_send(Out { pkt, video, key, t: Instant::now() }) {
        Ok(()) => {
            if kind == MediaKind::Video {
                sh.stats.frames.fetch_add(1, Ordering::Relaxed);
                sh.stats.bytes.fetch_add(len, Ordering::Relaxed);
            } else {
                sh.stats.audio_packets.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            sh.stats.drops.fetch_add(1, Ordering::Relaxed);
            if kind == MediaKind::Video {
                // Дальше P-кадры без выброшенного бессмысленны — ждём ключевой
                sh.link.need_key.store(true, Ordering::Relaxed);
                sh.link.want_key.store(true, Ordering::Relaxed);
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

impl Publisher {
    pub fn force_keyframe(&self) {
        if let Some(pad) = self.enc.static_pad("src") {
            let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
            pad.send_event(ev);
        }
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    pub fn drops(&self) -> u64 {
        self.shared.stats.drops.load(Ordering::Relaxed) + self.shared.link.stale.load(Ordering::Relaxed)
    }

    /// Задержка отправки, мс (сглаженная).
    pub fn send_delay_ms(&self) -> u64 {
        self.shared.link.delay_us.load(Ordering::Relaxed) / 1000
    }

    pub fn info_target_kbps(&self) -> u32 {
        self.target_kbps
    }

    /// Сервер сообщил, что зрителю не хватает канала.
    pub fn slow_viewer(&mut self, name: &str) {
        self.slow_viewers += 1;
        self.slow_name = name.to_string();
    }

    pub fn encoder_element(&self) -> gst::Element {
        self.enc.clone()
    }

    pub fn audio_packets(&self) -> u64 {
        self.shared.stats.audio_packets.load(Ordering::Relaxed)
    }

    pub fn frames(&self) -> u64 {
        self.shared.stats.frames.load(Ordering::Relaxed)
    }

    /// Раз в ~0.5 с: ключевой кадр по запросу, статистика, подстройка битрейта под канал.
    pub fn tick(&mut self) {
        let slow = std::mem::take(&mut self.slow_viewers) > 0;
        if self.shared.link.want_key.swap(false, Ordering::Relaxed) {
            self.force_keyframe();
        }
        let (t0, f0, b0) = self.last;
        let dt = t0.elapsed().as_secs_f64();
        if dt < 1.0 {
            return;
        }
        let f = self.shared.stats.frames.load(Ordering::Relaxed);
        let b = self.shared.stats.bytes.load(Ordering::Relaxed);
        self.fps_now = (f - f0) as f64 / dt;
        self.kbps_now = (b - b0) as f64 * 8.0 / dt / 1000.0;
        self.last = (Instant::now(), f, b);

        let drops = self.drops();
        let cur = self.kbps.load(Ordering::Relaxed);
        let min = (self.target_kbps / 6).max(500);
        let delay = self.send_delay_ms();
        // Рост RTT: очередь копится в роутере, хотя наша очередь пуста — канал на пределе
        let rtt = self.shared.link.rtt_us.load(Ordering::Relaxed);
        if rtt > 0 && (rtt < self.min_rtt_us || self.min_rtt_at.elapsed() > Duration::from_secs(30)) {
            self.min_rtt_us = rtt;
            self.min_rtt_at = Instant::now();
        }
        let queued_ms = rtt.saturating_sub(self.min_rtt_us) / 1000;
        let bloat = rtt > 0 && queued_ms > 80;
        if drops > self.last_drops || delay > 150 || slow || bloat {
            // Запоминаем потолок: битрейт, на котором канал не справился (чуть ниже). Выше него
            // быстро не лезем — иначе «пила»: разогнались → упёрлись → провал картинки.
            self.ceiling_kbps = ((cur as f64 * 0.9) as u32).max(min);
            self.ceiling_at = Instant::now();
            let new = ((cur as f64 * 0.7) as u32).max(min);
            if new != cur {
                let who = if bloat { format!(", пинг вырос на {queued_ms} мс") } else { String::new() }
                    + &if slow { format!(", зрителю {} не хватает канала", self.slow_name) } else { String::new() };
                info!("канал не тянет (задержка {delay} мс{who}): битрейт {cur} → {new} кбит/с");
                self.encoder.set_bitrate(&self.enc, new);
                self.kbps.store(new, Ordering::Relaxed);
            }
            self.calm = 0;
        } else {
            self.calm += 1;
            // Потолок медленно отпускаем: раз в 30 с спокойной работы +10%
            if self.ceiling_kbps < self.target_kbps && self.ceiling_at.elapsed() > Duration::from_secs(30) {
                self.ceiling_kbps = ((self.ceiling_kbps as f64 * 1.1) as u32).min(self.target_kbps);
                self.ceiling_at = Instant::now();
            }
            let cap = self.ceiling_kbps.min(self.target_kbps);
            if self.calm >= 6 && cur < cap && delay < 60 && queued_ms < 30 {
                let new = ((cur as f64 * 1.08) as u32).min(cap);
                self.encoder.set_bitrate(&self.enc, new);
                self.kbps.store(new, Ordering::Relaxed);
                self.calm = 4;
            }
        }
        self.last_drops = drops;
    }
}

/// Linux: раз в 2 с сверяем потоки приложений в PipeWire с ветками микшера amix:
/// новые (кроме Discord и себя) подключаем, исчезнувшие отключаем.
#[cfg(target_os = "linux")]
fn spawn_app_mixer(pipeline: gst::Pipeline, stop: Arc<AtomicBool>) {
    use std::collections::HashMap;
    std::thread::spawn(move || {
        let Some(amix) = pipeline.by_name("amix") else { return };
        let mut branches: HashMap<u64, (gst::Element, gst::Pad)> = HashMap::new();
        while !stop.load(Ordering::Relaxed) {
            let streams = capture::pw_app_streams();
            let want: HashMap<u64, String> = streams.iter().filter(|s| !s.2).map(|s| (s.0, s.1.clone())).collect();
            for (serial, name) in &want {
                if branches.contains_key(serial) {
                    continue;
                }
                let desc = format!(
                    "pipewiresrc target-object={serial} do-timestamp=true ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! queue max-size-time=200000000 max-size-buffers=0 max-size-bytes=0 leaky=downstream"
                );
                let Ok(bin) = gst::parse::bin_from_description(&desc, true) else { continue };
                let bin = bin.upcast::<gst::Element>();
                let _ = bin.set_property("name", format!("app_{serial}"));
                let Some(sink) = amix.request_pad_simple("sink_%u") else { continue };
                if pipeline.add(&bin).is_err() {
                    amix.release_request_pad(&sink);
                    continue;
                }
                if bin.static_pad("src").map(|p| p.link(&sink)).is_none_or(|r| r.is_err()) || bin.sync_state_with_parent().is_err() {
                    let _ = bin.set_state(gst::State::Null);
                    let _ = pipeline.remove(&bin);
                    amix.release_request_pad(&sink);
                    continue;
                }
                info!("звук в стрим: {name}");
                branches.insert(*serial, (bin, sink));
            }
            let gone: Vec<u64> = branches.keys().filter(|k| !want.contains_key(k)).copied().collect();
            for serial in gone {
                if let Some((bin, sink)) = branches.remove(&serial) {
                    let _ = bin.set_state(gst::State::Null);
                    let _ = pipeline.remove(&bin);
                    amix.release_request_pad(&sink);
                }
            }
            for s in streams.iter().filter(|s| s.2) {
                tracing::debug!("не захватываю: {}", s.1);
            }
            for _ in 0..8 {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    });
}
