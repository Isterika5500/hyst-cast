//! Выбор кодера/декодера H.264.
//!
//! Имена элементов зависят от системы (на ноутбуке с двумя видеокартами VA-кодер Intel
//! называется `varenderD129h264lpenc`), поэтому ищем по возможностям: все элементы GStreamer,
//! которые выдают/принимают video/x-h264, сортируем (аппаратные вперёд) и проверяем каждый
//! на коротком тестовом ролике — «есть в списке» ещё не значит «работает».

use gstreamer as gst;
use gst::prelude::*;
use std::sync::Mutex;
use tracing::{info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Nv,
    Qsv,
    Amf,
    Va,
    Mf,
    X264,
    OpenH264,
}

impl Family {
    fn order(self) -> u8 {
        match self {
            Family::Nv => 0,
            Family::Qsv => 1,
            Family::Amf => 2,
            Family::Va => 3,
            Family::Mf => 4,
            Family::X264 => 5,
            Family::OpenH264 => 6,
        }
    }
    pub fn hardware(self) -> bool {
        !matches!(self, Family::X264 | Family::OpenH264)
    }
}

/// Где живут кадры перед кодером — от этого зависит конвертер.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemPath {
    /// VA-API: vapostproc того же устройства, кадры не покидают видеокарту.
    Va,
    /// Windows: d3d11convert, кадры в видеопамяти.
    D3d11,
    /// Обычная память: videoconvert + videoscale (процессор).
    System,
}

#[derive(Clone, Debug)]
pub struct Encoder {
    pub factory: String,
    pub family: Family,
    pub path: MemPath,
    /// Аппаратный ли на самом деле (Media Foundation бывает и программным — смотрим описание элемента).
    pub hw: bool,
}

impl Encoder {
    pub fn label(&self) -> String {
        format!("{} ({})", self.factory, if self.hw { "GPU" } else { "CPU" })
    }

    /// Цепочка «конвертер ! caps» перед кодером, с масштабированием в width×height (поля — чёрные полосы).
    /// `src_d3d11` — источник отдаёт кадры в видеопамяти D3D11 (захват экрана Windows).
    pub fn convert_desc(&self, width: u32, height: u32, src_d3d11: bool) -> String {
        let dims = format!("width={width},height={height},pixel-aspect-ratio=1/1");
        match self.path {
            MemPath::Va => {
                let pp = va_postproc_for(&self.factory);
                format!("{pp} add-borders=true ! video/x-raw(memory:VAMemory),format=NV12,{dims}")
            }
            MemPath::D3d11 => {
                format!("d3d11upload ! d3d11convert add-borders=true ! video/x-raw(memory:D3D11Memory),format=NV12,{dims}")
            }
            MemPath::System => {
                let fmt = if self.family == Family::OpenH264 || self.family == Family::X264 { "I420" } else { "NV12" };
                if src_d3d11 {
                    // Масштабируем и меняем формат на видеокарте, в обычную память — уже готовый кадр
                    format!(
                        "d3d11convert add-borders=true ! video/x-raw(memory:D3D11Memory),format={fmt},{dims} ! d3d11download ! video/x-raw,format={fmt},{dims}"
                    )
                } else {
                    format!("videoconvert ! videoscale add-borders=true ! video/x-raw,format={fmt},{dims}")
                }
            }
        }
    }

    /// Настройки под низкую задержку: без B-кадров, CBR, ключевой кадр каждые 2 с.
    pub fn configure(&self, enc: &gst::Element, kbps: u32, fps: u32) {
        let gop = (fps * 2).to_string();
        let br = kbps.to_string();
        match self.family {
            Family::X264 => {
                set(enc, "tune", "zerolatency");
                set(enc, "speed-preset", if fps > 30 { "superfast" } else { "veryfast" });
                set(enc, "bitrate", &br);
                set(enc, "key-int-max", &gop);
                set(enc, "bframes", "0");
            }
            Family::OpenH264 => {
                set(enc, "bitrate", &(kbps * 1000).to_string());
                set(enc, "gop-size", &gop);
                set(enc, "rate-control", "bitrate");
                set(enc, "complexity", "low");
            }
            Family::Va => {
                set(enc, "rate-control", "cbr");
                set(enc, "bitrate", &br);
                set(enc, "key-int-max", &gop);
                set(enc, "b-frames", "0");
                set(enc, "ref-frames", "1");
            }
            Family::Nv => {
                set(enc, "rc-mode", "cbr");
                set(enc, "bitrate", &br);
                set(enc, "gop-size", &gop);
                set(enc, "bframes", "0");
                set(enc, "zerolatency", "true");
                set(enc, "preset", "p4");
                set(enc, "tune", "low-latency");
            }
            Family::Qsv => {
                set(enc, "rate-control", "cbr");
                set(enc, "bitrate", &br);
                set(enc, "gop-size", &gop);
                set(enc, "b-frames", "0");
            }
            Family::Amf => {
                set(enc, "usage", "low-latency");
                set(enc, "rate-control", "cbr");
                set(enc, "bitrate", &br);
                set(enc, "gop-size", &gop);
            }
            Family::Mf => {
                set(enc, "rc-mode", "cbr");
                set(enc, "bitrate", &br);
                set(enc, "gop-size", &gop);
                set(enc, "bframes", "0");
                set(enc, "low-latency", "true");
            }
        }
    }

    /// Смена битрейта на лету (адаптация к каналу).
    pub fn set_bitrate(&self, enc: &gst::Element, kbps: u32) {
        let v = if self.family == Family::OpenH264 { kbps * 1000 } else { kbps };
        set(enc, "bitrate", &v.to_string());
    }
}

/// Установить свойство из строки, если оно есть и значение подходит. Ничего не роняет.
pub fn set(el: &gst::Element, name: &str, value: &str) {
    let Some(pspec) = el.find_property(name) else { return };
    match gst::glib::Value::deserialize_with_pspec(value, &pspec) {
        Ok(v) => el.set_property_from_value(name, &v),
        Err(_) => warn!("{}: свойство {name}={value} не принято", el.name()),
    }
}

/// `varenderD129h264lpenc` → `varenderD129postproc`, `vah264enc` → `vapostproc`.
fn va_postproc_for(enc: &str) -> String {
    let base = enc.strip_suffix("h264lpenc").or_else(|| enc.strip_suffix("h264enc")).unwrap_or("va");
    format!("{base}postproc")
}

fn family_of(name: &str) -> Option<Family> {
    let n = name.to_lowercase();
    if n == "x264enc" {
        return Some(Family::X264);
    }
    if !n.contains("h264") {
        return None;
    }
    Some(if n.starts_with("nv") {
        Family::Nv
    } else if n.starts_with("qsv") {
        Family::Qsv
    } else if n.starts_with("amf") {
        Family::Amf
    } else if n.starts_with("va") && !n.starts_with("vaapi") {
        Family::Va
    } else if n.starts_with("mf") {
        Family::Mf
    } else if n == "x264enc" {
        Family::X264
    } else if n == "openh264enc" {
        Family::OpenH264
    } else {
        return None;
    })
}

fn has(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Все кандидаты в кодеры, лучшие вперёд.
pub fn encoder_candidates() -> Vec<Encoder> {
    let mut list: Vec<(Encoder, bool, u8)> = Vec::new();
    for f in gst::ElementFactory::factories_with_type(gst::ElementFactoryType::VIDEO_ENCODER, gst::Rank::NONE) {
        let name = f.name().to_string();
        let Some(family) = family_of(&name) else { continue };
        let lp = name.contains("lpenc");
        let paths: Vec<MemPath> = match family {
            Family::Va if has(&va_postproc_for(&name)) => vec![MemPath::Va, MemPath::System],
            Family::Nv | Family::Qsv | Family::Amf | Family::Mf if has("d3d11convert") => vec![MemPath::D3d11, MemPath::System],
            _ => vec![MemPath::System],
        };
        for path in paths {
            let hw = family.hardware() && (family != Family::Mf || f.klass().contains("Hardware"));
            list.push((Encoder { factory: name.clone(), family, path, hw }, lp, 0));
        }
    }
    // GPU-путь раньше системного для одного и того же кодера
    // Кодеры без управления битрейтом (у VA low-power на старых Intel только CQP) — после остальных
    // аппаратных: на фильмах они выдают непредсказуемый поток и не подстраиваются под канал.
    let mut cbr_cache = std::collections::HashMap::new();
    for (e, _, rc) in list.iter_mut() {
        let ok = *cbr_cache.entry(e.factory.clone()).or_insert_with(|| controls_bitrate(&e.factory, e.family));
        *rc = if ok { 0 } else { 1 };
    }
    // Программные: x264 (лучшее качество) → openh264 → программный Media Foundation
    let soft = |f: Family| match f {
        Family::X264 => 0,
        Family::OpenH264 => 1,
        _ => 2,
    };
    list.sort_by_key(|(e, lp, rc)| (!e.hw, *rc, if e.hw { e.family.order() } else { soft(e.family) }, !*lp, e.factory.clone(), e.path == MemPath::System));
    list.into_iter().map(|(e, _, _)| e).collect()
}

/// Умеет ли кодер держать заданный битрейт (CBR). Если свойства rate-control нет — считаем, что да.
fn controls_bitrate(factory: &str, family: Family) -> bool {
    let Ok(el) = gst::ElementFactory::make(factory).build() else { return false };
    let (prop, value) = match family {
        Family::Nv | Family::Mf => ("rc-mode", "cbr"),
        Family::OpenH264 => ("rate-control", "bitrate"),
        _ => ("rate-control", "cbr"),
    };
    match el.find_property(prop) {
        Some(pspec) => gst::glib::Value::deserialize_with_pspec(value, &pspec).is_ok(),
        None => true,
    }
}

/// Прогнать 10 кадров через кодер. Работает — значит годится.
pub fn probe_encoder(e: &Encoder) -> bool {
    let desc = format!(
        "videotestsrc num-buffers=10 ! video/x-raw,width=640,height=360,framerate=30/1 ! {} ! {} name=enc ! h264parse ! fakesink",
        e.convert_desc(1280, 720, false),
        e.factory
    );
    run_probe(&desc, |p| {
        if let Some(enc) = p.by_name("enc") {
            e.configure(&enc, 2000, 30);
        }
    })
}

fn run_probe(desc: &str, setup: impl FnOnce(&gst::Pipeline)) -> bool {
    let Ok(el) = gst::parse::launch(desc) else { return false };
    let Ok(p) = el.downcast::<gst::Pipeline>() else { return false };
    setup(&p);
    if p.set_state(gst::State::Playing).is_err() {
        let _ = p.set_state(gst::State::Null);
        return false;
    }
    let bus = p.bus().unwrap();
    let ok = loop {
        match bus.timed_pop_filtered(gst::ClockTime::from_seconds(8), &[gst::MessageType::Eos, gst::MessageType::Error]) {
            Some(m) => match m.view() {
                gst::MessageView::Eos(_) => break true,
                gst::MessageView::Error(err) => {
                    info!("  проверка не прошла: {}", err.error());
                    break false;
                }
                _ => {}
            },
            None => break false,
        }
    };
    let _ = p.set_state(gst::State::Null);
    ok
}

static WORKING: Mutex<Option<Vec<Encoder>>> = Mutex::new(None);

/// Рабочие кодеры по убыванию качества (результат кешируется на время работы программы).
pub fn working_encoders(force: &str) -> Vec<Encoder> {
    let mut cache = WORKING.lock().unwrap();
    if cache.is_none() {
        let mut ok = Vec::new();
        for e in encoder_candidates() {
            let t = std::time::Instant::now();
            let good = probe_encoder(&e);
            info!("кодер {} [{:?}]: {} за {} мс", e.factory, e.path, if good { "работает" } else { "НЕ работает" }, t.elapsed().as_millis());
            if good {
                ok.push(e);
            }
        }
        *cache = Some(ok);
    }
    let mut list = cache.clone().unwrap();
    if !force.is_empty() {
        list.sort_by_key(|e| e.factory != force);
    }
    list
}

// ------------------------------------------------------------------ декодеры

#[derive(Clone, Debug)]
pub struct Decoder {
    pub factory: String,
    pub hardware: bool,
}

pub fn decoder_candidates() -> Vec<Decoder> {
    let caps = gst::Caps::builder("video/x-h264").field("stream-format", "byte-stream").field("alignment", "au").build();
    let mut list: Vec<(Decoder, gst::Rank)> = gst::ElementFactory::factories_with_type(gst::ElementFactoryType::DECODER, gst::Rank::NONE)
        .into_iter()
        .filter(|f| f.can_sink_any_caps(&caps))
        .filter(|f| f.klass().contains("Video"))
        .filter(|f| !f.name().contains("alpha") && !f.name().contains("fake"))
        .map(|f| {
            let hw = f.klass().contains("Hardware");
            (Decoder { factory: f.name().to_string(), hardware: hw }, f.rank())
        })
        .collect();
    list.sort_by_key(|(d, rank)| (!d.hardware, std::cmp::Reverse(*rank)));
    list.into_iter().map(|(d, _)| d).collect()
}

/// Проверка декодера: кодируем тестовый ролик любым программным кодером и декодируем.
pub fn probe_decoder(d: &Decoder) -> bool {
    let enc = if has("openh264enc") {
        "openh264enc"
    } else if has("x264enc") {
        "x264enc tune=zerolatency"
    } else {
        return d.factory == "avdec_h264";
    };
    let desc = format!(
        "videotestsrc num-buffers=10 ! video/x-raw,width=640,height=360,framerate=30/1,format=I420 ! {enc} ! h264parse ! video/x-h264,stream-format=byte-stream,alignment=au ! {} ! fakesink",
        d.factory
    );
    run_probe(&desc, |_| {})
}

static DECODERS: Mutex<Option<Vec<Decoder>>> = Mutex::new(None);

pub fn working_decoders() -> Vec<Decoder> {
    let mut cache = DECODERS.lock().unwrap();
    if cache.is_none() {
        let mut ok = Vec::new();
        for d in decoder_candidates() {
            let good = probe_decoder(&d);
            info!("декодер {}: {}", d.factory, if good { "работает" } else { "НЕ работает" });
            if good {
                ok.push(d);
            }
        }
        *cache = Some(ok);
    }
    cache.clone().unwrap()
}
