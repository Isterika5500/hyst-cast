//! Зритель: пакеты от сервера → appsrc → декодер → окно (или fakesink в самопроверке).
//!
//! Синхронизация: время кадра стримера переводится в время нашего конвейера со сдвигом
//! «сейчас + буфер». Звук и видео используют один сдвиг, поэтому идут вместе. Если часы стримера
//! и наши расходятся (за 2 часа фильма это заметно), сдвиг аккуратно подправляется.

use crate::codecs;
use anyhow::{Context, Result};
use bytes::Bytes;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gst::prelude::*;
use proto::MediaKind;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SinkMode {
    /// Отдельное окно GStreamer (старый вариант, оставлен на всякий случай).
    Window,
    Fake,
    /// Кадры в RGBA отдаются в окно приложения (как в Discord).
    Embedded,
}

/// Готовый кадр для окна приложения.
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// «Слот» последнего кадра: зритель кладёт, интерфейс забирает при перерисовке.
pub struct FrameSlot {
    pub frame: Mutex<Option<Frame>>,
    pub seq: AtomicU64,
    pub repaint: crate::net::Repaint,
}

impl FrameSlot {
    pub fn new(repaint: crate::net::Repaint) -> Arc<FrameSlot> {
        Arc::new(FrameSlot { frame: Mutex::new(None), seq: AtomicU64::new(0), repaint })
    }
}

/// Подключить appsink к слоту кадров: копируем в плотный RGBA (у кадра бывает выравнивание строк)
/// и будим интерфейс. Используется и зрителем, и превью у стримера.
pub fn attach_frame_sink(sink: &gst_app::AppSink, slot: Arc<FrameSlot>) {
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let (Some(buf), Some(caps)) = (sample.buffer(), sample.caps()) else { return Ok(gst::FlowSuccess::Ok) };
                let Ok(info) = gstreamer_video::VideoInfo::from_caps(caps) else { return Ok(gst::FlowSuccess::Ok) };
                let Ok(vf) = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buf, &info) else { return Ok(gst::FlowSuccess::Ok) };
                let (w, h) = (info.width(), info.height());
                use gstreamer_video::prelude::VideoFrameExt;
                let stride = vf.plane_stride()[0] as usize;
                let Ok(src) = vf.plane_data(0) else { return Ok(gst::FlowSuccess::Ok) };
                let row = w as usize * 4;
                let mut rgba = Vec::with_capacity(row * h as usize);
                for y in 0..h as usize {
                    rgba.extend_from_slice(&src[y * stride..y * stride + row]);
                }
                *slot.frame.lock().unwrap() = Some(Frame { w, h, rgba });
                slot.seq.fetch_add(1, Ordering::Relaxed);
                (slot.repaint)();
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
}

/// Размер для показа в окне: не больше 1920×1080 с сохранением пропорций, чётный.
pub fn display_size(w: u32, h: u32) -> (u32, u32) {
    let k = (1920.0 / w.max(1) as f64).min(1080.0 / h.max(1) as f64).min(1.0);
    (((w as f64 * k) as u32) & !1, ((h as f64 * k) as u32) & !1)
}

/// Цепочка «декодер → RGBA нужного размера в обычной памяти»: масштабирует и переводит цвета видеокарта.
fn embed_chain(dec: &str, w: u32, h: u32) -> String {
    let has = |n: &str| gst::ElementFactory::find(n).is_some();
    let dims = format!("width={w},height={h},pixel-aspect-ratio=1/1");
    let va_pp = dec.strip_suffix("h264dec").map(|b| format!("{b}postproc")).unwrap_or_default();
    // Капсфильтры с именами vcaps/vcaps2 — чтобы менять размер на лету под размер видео в окне
    if dec.starts_with("va") && !dec.starts_with("vaapi") && has(&va_pp) {
        format!("{va_pp} add-borders=true ! capsfilter name=vcaps caps=\"video/x-raw,format=RGBA,{dims}\"")
    } else if dec.starts_with("d3d11") && has("d3d11convert") && has("d3d11download") {
        format!("d3d11convert add-borders=true ! capsfilter name=vcaps caps=\"video/x-raw(memory:D3D11Memory),format=RGBA,{dims}\" ! d3d11download ! capsfilter name=vcaps2 caps=\"video/x-raw,format=RGBA,{dims}\"")
    } else if dec.starts_with("d3d12") && has("d3d12convert") && has("d3d12download") {
        format!("d3d12convert add-borders=true ! capsfilter name=vcaps caps=\"video/x-raw(memory:D3D12Memory),format=RGBA,{dims}\" ! d3d12download ! capsfilter name=vcaps2 caps=\"video/x-raw,format=RGBA,{dims}\"")
    } else {
        format!("videoconvert ! videoscale add-borders=true ! capsfilter name=vcaps caps=\"video/x-raw,format=RGBA,{dims}\"")
    }
}

/// Сколько опозданий за 2 с считать «сеть стала медленнее» и сдвигать синхронизацию.
const LATE_RESYNC: u32 = 12;

#[derive(Default)]
pub struct ViewerStats {
    pub rendered: AtomicU64,
    pub received: AtomicU64,
    pub bytes: AtomicU64,
    pub audio_rendered: AtomicU64,
    /// Минимальный запас до показа за последнее окно, мс (отрицательный — кадры опаздывают).
    pub margin_ms: AtomicI64,
    pub resyncs: AtomicU64,
    /// Звуковых пакетов выброшено из-за опоздания (замаскированы декодером).
    pub late_drops: AtomicU64,
    /// Задержка «захват → показ», мс, сглаженная (-1 — неизвестно).
    pub latency_ms: AtomicI64,
    /// Задержки прихода звуковых пакетов относительно самого быстрого, мс (для анализа джиттера).
    pub jitter_samples: Mutex<Vec<i64>>,
}

struct SyncState {
    offset: Option<i64>,
    win_min: i64,
    win_start: Instant,
    /// Сколько кадров видео опоздало подряд за короткое окно.
    late_video: u32,
    late_start: Instant,
}

pub struct Viewer {
    pipeline: gst::Pipeline,
    vsrc: gst_app::AppSrc,
    asrc: Option<gst_app::AppSrc>,
    volume: Option<gst::Element>,
    sync: Mutex<SyncState>,
    latency_ns: i64,
    got_key: AtomicBool,
    offset_pub: Arc<AtomicI64>,
    pub stats: Arc<ViewerStats>,
    pub closed: Arc<Mutex<Option<String>>>,
    stop_bus: Arc<AtomicBool>,
    pub decoder: String,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop_bus.store(true, Ordering::Relaxed);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn video_sink(mode: SinkMode) -> &'static str {
    match mode {
        SinkMode::Fake => "fakesink name=vout sync=true",
        SinkMode::Embedded => "appsink name=vout sync=true max-buffers=2 drop=true",
        SinkMode::Window => {
            if cfg!(windows) && gst::ElementFactory::find("d3d11videosink").is_some() {
                "d3d11videosink name=vout sync=true"
            } else if gst::ElementFactory::find("glimagesink").is_some() {
                "glimagesink name=vout sync=true"
            } else {
                "videoconvert ! autovideosink name=vout sync=true"
            }
        }
    }
}

impl Viewer {
    pub fn new(title: &str, has_audio: bool, latency_ms: u32, mode: SinkMode, size: (u32, u32), slot: Option<Arc<FrameSlot>>) -> Result<Viewer> {
        let decoders = codecs::working_decoders();
        let dec = decoders.first().context("Не найден ни один рабочий декодер H.264")?;
        let convert = if mode == SinkMode::Embedded {
            let (w, h) = display_size(size.0, size.1);
            format!("{} ! ", embed_chain(&dec.factory, w, h))
        } else {
            String::new()
        };
        let mut desc = format!(
            "appsrc name=vsrc is-live=true format=time do-timestamp=false max-bytes=0 max-buffers=0 \
             caps=video/x-h264,stream-format=byte-stream,alignment=au \
             ! h264parse ! {} ! queue max-size-buffers=3 max-size-time=0 max-size-bytes=0 ! {convert}{}",
            dec.factory,
            video_sink(mode)
        );
        if has_audio {
            let asink = if mode == SinkMode::Fake { "fakesink name=aout sync=true" } else { "autoaudiosink name=aout" };
            desc.push_str(&format!(
                " appsrc name=asrc is-live=true format=time do-timestamp=false max-bytes=0 max-buffers=0 \
                 caps=audio/x-opus,rate=48000,channels=2,channel-mapping-family=0,stream-count=1,coupled-count=1 \
                 ! opusdec plc=true ! audioconvert ! audioresample ! volume name=vol ! {asink}"
            ));
        }
        info!("конвейер зрителя: {desc}");
        let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("не конвейер"))?;
        let vsrc = pipeline.by_name("vsrc").unwrap().downcast::<gst_app::AppSrc>().unwrap();
        let asrc = pipeline.by_name("asrc").map(|e| e.downcast::<gst_app::AppSrc>().unwrap());
        let volume = pipeline.by_name("vol");
        let stats = Arc::new(ViewerStats::default());
        stats.latency_ms.store(-1, Ordering::Relaxed);
        let offset_pub = Arc::new(AtomicI64::new(i64::MIN));

        // Кадры в окно приложения
        if let (Some(slot), Some(out)) = (slot, pipeline.by_name("vout")) {
            if let Ok(sink) = out.downcast::<gst_app::AppSink>() {
                attach_frame_sink(&sink, slot);
            }
        }
        if let Some(out) = pipeline.by_name("vout") {
            if out.find_property("fullscreen-toggle-mode").is_some() {
                codecs::set(&out, "fullscreen-toggle-mode", "alt-enter");
            }
            if let Some(pad) = out.static_pad("sink") {
                let st = stats.clone();
                let off = offset_pub.clone();
                let el = out.clone();
                pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    st.rendered.fetch_add(1, Ordering::Relaxed);
                    // Задержка: сейчас − (база стримера + время кадра у стримера). Имеет смысл, только когда
                    // стример в этом же процессе (самопроверка) и часы одни и те же.
                    let base = crate::publish::SENDER_BASE_NS.load(Ordering::Relaxed) as i64;
                    let o = off.load(Ordering::Relaxed);
                    if let (Some(buf), Some(clock), true) = (info.buffer(), el.clock(), base > 0 && o != i64::MIN) {
                        if let (Some(pts), now) = (buf.pts(), clock.time()) {
                            let sender_pts = pts.nseconds() as i64 - o;
                            let lat = (now.nseconds() as i64 - (base + sender_pts)) / 1_000_000;
                            if (0..10_000).contains(&lat) {
                                let old = st.latency_ms.load(Ordering::Relaxed);
                                st.latency_ms.store(if old < 0 { lat } else { (old * 7 + lat) / 8 }, Ordering::Relaxed);
                            }
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }
        if let Some(pad) = pipeline.by_name("vol").and_then(|v| v.static_pad("src")) {
            let st = stats.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                st.audio_rendered.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        }

        // Заголовок окна берут из тега title (d3d11videosink/glimagesink)
        let mut tags = gst::TagList::new();
        tags.get_mut().unwrap().add::<gst::tags::Title>(&title, gst::TagMergeMode::Replace);
        let _ = vsrc.send_event(gst::event::Tag::new(tags));

        pipeline.set_state(gst::State::Playing).context("конвейер зрителя не запустился")?;

        let closed = Arc::new(Mutex::new(None));
        let stop_bus = Arc::new(AtomicBool::new(false));
        {
            let bus = pipeline.bus().unwrap();
            let closed = closed.clone();
            let stop = stop_bus.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(m) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(250), &[gst::MessageType::Error]) {
                        if let gst::MessageView::Error(e) = m.view() {
                            let text = e.error().to_string();
                            warn!("зритель: {text} ({:?})", e.debug());
                            *closed.lock().unwrap() = Some(text);
                            break;
                        }
                    }
                }
            });
        }

        Ok(Viewer {
            pipeline,
            vsrc,
            asrc,
            volume,
            sync: Mutex::new(SyncState { offset: None, win_min: i64::MAX, win_start: Instant::now(), late_video: 0, late_start: Instant::now() }),
            latency_ns: latency_ms as i64 * 1_000_000,
            got_key: AtomicBool::new(false),
            offset_pub,
            stats,
            closed,
            stop_bus,
            decoder: format!("{} ({})", dec.factory, if dec.hardware { "GPU" } else { "CPU" }),
        })
    }

    /// Размер кадра под размер видео в окне (меньше картинка — меньше работы на перерисовку).
    pub fn set_display_size(&self, w: u32, h: u32) {
        for name in ["vcaps", "vcaps2"] {
            let Some(cf) = self.pipeline.by_name(name) else { continue };
            let Some(old) = cf.property::<Option<gst::Caps>>("caps") else { continue };
            let mut caps = old.copy();
            if let Some(s) = caps.make_mut().structure_mut(0) {
                s.set("width", (w & !1).max(2) as i32);
                s.set("height", (h & !1).max(2) as i32);
            }
            cf.set_property("caps", &caps);
        }
    }

    pub fn set_volume(&self, v: f64) {
        if let Some(vol) = &self.volume {
            vol.set_property("volume", v.clamp(0.0, 2.0));
        }
    }

    pub fn push(&self, kind: MediaKind, key: bool, pts_us: u64, data: Bytes) {
        let Some(now) = self.pipeline.current_running_time() else { return };
        let now = now.nseconds() as i64;
        if kind == MediaKind::Video {
            if !self.got_key.load(Ordering::Relaxed) {
                if !key {
                    return;
                }
                self.got_key.store(true, Ordering::Relaxed);
            }
            self.stats.received.fetch_add(1, Ordering::Relaxed);
            self.stats.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        let pts = pts_us as i64 * 1000;
        if kind == MediaKind::Audio {
            let mut j = self.stats.jitter_samples.lock().unwrap();
            if j.len() < 20_000 {
                j.push((now - pts) / 1_000_000);
            }
        }
        let out = {
            let mut st = self.sync.lock().unwrap();
            let off = *st.offset.get_or_insert(now + self.latency_ns - pts);
            let mut out = pts + off;
            let margin = out - now;
            // Одиночный опоздавший пакет (выброс в сети) не сдвигает синхронизацию: звук выкидываем —
            // декодер Opus замаскирует 20 мс, кадр видео отдаём как есть (он нужен декодеру как опора).
            // Сдвигаемся, только если опаздывает много пакетов подряд — значит, сеть стала медленнее.
            let mut drop_packet = false;
            if margin < 5_000_000 {
                if st.late_start.elapsed() > Duration::from_secs(2) {
                    st.late_video = 0;
                    st.late_start = Instant::now();
                }
                st.late_video += 1;
                if st.late_video >= LATE_RESYNC {
                    st.late_video = 0;
                    let new_off = now + self.latency_ns - pts;
                    st.offset = Some(new_off);
                    out = pts + new_off;
                    self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
                    st.win_min = i64::MAX;
                    st.win_start = Instant::now();
                } else if kind == MediaKind::Audio {
                    drop_packet = true;
                    self.stats.late_drops.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                st.win_min = st.win_min.min(margin);
                if st.win_start.elapsed() > Duration::from_secs(5) {
                    // Запас стабильно больше нужного (сеть ускорилась / часы стримера спешат) — догоняем
                    if st.win_min > self.latency_ns + 100_000_000 {
                        let back = st.win_min - self.latency_ns;
                        st.offset = Some(off - back);
                        self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
                    }
                    self.stats.margin_ms.store(st.win_min / 1_000_000, Ordering::Relaxed);
                    st.win_min = i64::MAX;
                    st.win_start = Instant::now();
                } else if self.stats.margin_ms.load(Ordering::Relaxed) == 0 {
                    self.stats.margin_ms.store(margin / 1_000_000, Ordering::Relaxed);
                }
            }
            if drop_packet {
                return;
            }
            self.offset_pub.store(st.offset.unwrap_or(i64::MIN), Ordering::Relaxed);
            out
        };
        if out < 0 {
            return;
        }
        let mut buf = gst::Buffer::from_slice(data);
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_nseconds(out as u64));
            if kind == MediaKind::Video && !key {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        let src = match kind {
            MediaKind::Video => &self.vsrc,
            MediaKind::Audio => match &self.asrc {
                Some(a) => a,
                None => return,
            },
        };
        let _ = src.push_buffer(buf);
    }
}
