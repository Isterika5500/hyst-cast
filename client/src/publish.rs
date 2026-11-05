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

#[derive(Default)]
pub struct PubStats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub drops: AtomicU64,
    pub audio_packets: AtomicU64,
}

struct Shared {
    tx: mpsc::Sender<Vec<u8>>,
    need_key: AtomicBool,
    want_key: AtomicBool,
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
    calm: u32,
    last: (Instant, u64, u64),
    pub fps_now: f64,
    pub kbps_now: f64,
    pub portal_token: Option<String>,
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
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<Publisher> {
    let capture = capture::open(&source, preset.fps, portal_token).await?;
    let test = source == Source::Test;
    let video_desc = capture.video_desc.clone();
    let (encoders, audio_ok) = tokio::task::spawn_blocking(move || {
        let enc = codecs::working_encoders(&force_encoder);
        let a = audio && probe_audio(&capture::audio_desc(test));
        (enc, a)
    })
    .await?;
    if audio && !audio_ok {
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
        let r = tokio::task::spawn_blocking(move || try_start(&vd, &e2, preset, audio_ok, test, tx)).await?;
        match r {
            Ok(mut p) => {
                p.portal_token = capture.portal_token.clone();
                p._capture = Some(capture);
                return Ok(p);
            }
            Err(e) => {
                warn!("кодер {} не запустился: {e:#}", enc.factory);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap()).context("Ни один кодер не смог запуститься")
}

fn probe_audio(desc: &str) -> bool {
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

fn try_start(video_desc: &str, encoder: &Encoder, preset: Preset, audio: bool, test: bool, tx: mpsc::Sender<Vec<u8>>) -> Result<Publisher> {
    let mut desc = format!(
        "{video_desc} ! queue max-size-buffers=3 max-size-time=0 max-size-bytes=0 leaky=downstream ! {} ! {} name=enc ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=vsink sync=false max-buffers=8",
        encoder.convert_desc(preset.width, preset.height),
        encoder.factory
    );
    if audio {
        desc.push_str(&format!(
            " {} ! queue max-size-time=200000000 max-size-buffers=0 max-size-bytes=0 leaky=downstream ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=128000 frame-size=20 audio-type=generic ! appsink name=asink sync=false max-buffers=50",
            capture::audio_desc(test)
        ));
    }
    info!("конвейер стрима: {desc}");
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("не конвейер"))?;
    let enc = pipeline.by_name("enc").context("нет кодера")?;
    encoder.configure(&enc, preset.kbps, preset.fps);

    let shared = Arc::new(Shared {
        tx,
        need_key: AtomicBool::new(true),
        want_key: AtomicBool::new(false),
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

    pipeline.set_state(gst::State::Playing).context("конвейер не запустился")?;

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

    // Дальше ошибки конвейера ловит отдельный поток
    let error = Arc::new(Mutex::new(None));
    let stop_bus = Arc::new(AtomicBool::new(false));
    {
        let error = error.clone();
        let stop = stop_bus.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(m) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(250), &[gst::MessageType::Error, gst::MessageType::Eos]) {
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
        calm: 0,
        last: (Instant::now(), 0, 0),
        fps_now: 0.0,
        kbps_now: 0.0,
        portal_token: None,
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
        if sh.need_key.load(Ordering::Relaxed) {
            if !key {
                sh.stats.drops.fetch_add(1, Ordering::Relaxed);
                sh.want_key.store(true, Ordering::Relaxed);
                return;
            }
            sh.need_key.store(false, Ordering::Relaxed);
        }
    }

    let mut pkt = Vec::with_capacity(MediaHeader::LEN + map.len());
    MediaHeader { publisher: 0, kind, flags: if key { FLAG_KEY } else { 0 }, pts_us }.write(&mut pkt);
    pkt.extend_from_slice(map.as_slice());
    let len = pkt.len() as u64;
    match sh.tx.try_send(pkt) {
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
                sh.need_key.store(true, Ordering::Relaxed);
                sh.want_key.store(true, Ordering::Relaxed);
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
        self.shared.stats.drops.load(Ordering::Relaxed)
    }

    pub fn audio_packets(&self) -> u64 {
        self.shared.stats.audio_packets.load(Ordering::Relaxed)
    }

    pub fn frames(&self) -> u64 {
        self.shared.stats.frames.load(Ordering::Relaxed)
    }

    /// Раз в ~0.5 с: ключевой кадр по запросу, статистика, подстройка битрейта под канал.
    pub fn tick(&mut self) {
        if self.shared.want_key.swap(false, Ordering::Relaxed) {
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
        if drops > self.last_drops {
            let new = ((cur as f64 * 0.7) as u32).max(min);
            if new != cur {
                info!("канал не тянет: битрейт {cur} → {new} кбит/с");
                self.encoder.set_bitrate(&self.enc, new);
                self.kbps.store(new, Ordering::Relaxed);
            }
            self.calm = 0;
        } else {
            self.calm += 1;
            if self.calm >= 6 && cur < self.target_kbps {
                let new = ((cur as f64 * 1.15) as u32).min(self.target_kbps);
                self.encoder.set_bitrate(&self.enc, new);
                self.kbps.store(new, Ordering::Relaxed);
                self.calm = 3;
            }
        }
        self.last_drops = drops;
    }
}
