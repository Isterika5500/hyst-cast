#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! hyst-cast — стрим экрана для своих.
//!
//!   hyst-cast                 — окно приложения
//!   hyst-cast --probe         — какие кодеры/декодеры есть и работают
//!   hyst-cast --selftest ...  — полный цикл без окна: стример (тест-картинка + тон) → сервер → зритель

mod capture;
mod codecs;
mod config;
mod net;
mod publish;
mod tls;
mod ui;
mod watch;

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() {
    setup_env();
    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--selftest" || a == "--probe" || a == "--elements");
    init_logging(headless);
    if let Err(e) = gst::init() {
        fatal(&format!("GStreamer не запустился: {e}"));
    }
    let r = if args.iter().any(|a| a == "--elements") {
        elements(&args)
    } else if args.iter().any(|a| a == "--probe") {
        probe()
    } else if args.iter().any(|a| a == "--selftest") {
        selftest(&args)
    } else {
        run_gui()
    };
    if let Err(e) = r {
        fatal(&format!("{e:#}"));
    }
}

fn fatal(msg: &str) -> ! {
    tracing::error!("{msg}");
    eprintln!("ОШИБКА: {msg}");
    std::process::exit(1);
}

/// Windows-сборка несёт GStreamer с собой: DLL рядом с exe, плагины в gstreamer-plugins.
fn setup_env() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(dir) = exe.parent() else { return };
    let plugins = dir.join("gstreamer-plugins");
    if plugins.is_dir() {
        let _ = std::fs::create_dir_all(config::dir());
        // SAFETY: вызывается в самом начале main, других потоков ещё нет.
        unsafe {
            std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", &plugins);
            std::env::set_var("GST_PLUGIN_PATH_1_0", &plugins);
            std::env::set_var("GST_REGISTRY_1_0", config::dir().join("gst-registry.bin"));
            std::env::set_var("GST_REGISTRY_FORK", "no");
        }
    }
}

fn init_logging(headless: bool) {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let _ = std::fs::create_dir_all(config::dir());
    let filter = std::env::var("HYST_CAST_LOG").unwrap_or_else(|_| "info".into());
    let builder = tracing_subscriber::fmt().with_target(false).with_ansi(false).with_env_filter(filter);
    if headless {
        builder.with_writer(std::io::stderr).init();
    } else {
        match std::fs::File::create(config::dir().join("log.txt")) {
            Ok(f) => builder.with_writer(std::io::stderr.and(Mutex::new(f))).init(),
            Err(_) => builder.with_writer(std::io::stderr).init(),
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?)
}

fn run_gui() -> Result<()> {
    let rt = runtime()?;
    // Проверка кодеров заранее, в фоне — к нажатию «Начать стрим» всё уже известно
    rt.spawn_blocking(|| {
        codecs::working_encoders("");
        codecs::working_decoders();
    });
    let handle = rt.handle().clone();
    let shared: net::SharedRef = Arc::new(Mutex::new(net::Shared::default()));
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("hyst-cast")
            .with_app_id("hyst-cast")
            .with_inner_size([1040.0, 680.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(eframe::egui::IconData { rgba: include_bytes!("../assets/icon-256.rgba").to_vec(), width: 256, height: 256 }),
        ..Default::default()
    };
    eframe::run_native(
        "hyst-cast",
        opts,
        Box::new(move |cc| {
            ui::setup(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let repaint: net::Repaint = Arc::new(move || ctx.request_repaint());
            let tx = net::spawn(&handle, shared.clone(), repaint, watch::SinkMode::Embedded);
            Ok(Box::new(ui::App::new(shared, tx)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("окно не открылось: {e}"))?;
    drop(rt);
    Ok(())
}

fn probe() -> Result<()> {
    println!("GStreamer {}", gst::version_string());
    println!("\nКодеры H.264 (по порядку предпочтения):");
    for e in codecs::encoder_candidates() {
        let t = Instant::now();
        let ok = codecs::probe_encoder(&e);
        println!("  {:<28} {:<7} {:?}  {} ({} мс)", e.factory, if e.hw { "GPU" } else { "CPU" }, e.path, if ok { "OK" } else { "не работает" }, t.elapsed().as_millis());
    }
    println!("\nДекодеры H.264:");
    for d in codecs::decoder_candidates() {
        println!("  {:<28} {:<7} {}", d.factory, if d.hardware { "GPU" } else { "CPU" }, if codecs::probe_decoder(&d) { "OK" } else { "не работает" });
    }
    println!("\nИсточники:");
    let sources = capture::list_sources();
    for (_, l) in &sources {
        println!("  {l}");
    }
    println!("\nЗвук для «{}»:", sources.first().map(|s| s.1.as_str()).unwrap_or("—"));
    if let Some((src, _)) = sources.first() {
        for (d, hint) in capture::audio_candidates(capture::AudioKind::System, src) {
            println!("  {:<28} {}  [{d}]", hint, if publish::probe_audio(&d) { "OK" } else { "не работает" });
        }
    }
    Ok(())
}

/// Отладка на чужой машине: есть ли элементы и какие у них свойства.
///   hyst-cast --elements d3d11screencapturesrc gdiscreencapsrc …
fn elements(args: &[String]) -> Result<()> {
    use gst::prelude::*;
    let names: Vec<&String> = args.iter().skip_while(|a| *a != "--elements").skip(1).collect();
    for n in names {
        match gst::ElementFactory::make(n).build() {
            Ok(el) => {
                let props: Vec<String> = el.list_properties().iter().map(|p| p.name().to_string()).filter(|p| p != "name" && p != "parent").collect();
                println!("{n}: {}", props.join(", "));
            }
            Err(_) => println!("{n}: НЕТ"),
        }
    }
    Ok(())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<Result<()>>) -> Result<()> {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if let Some(r) = f() {
            return r;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("не дождались: {what}")
}

/// Полный цикл без окна. Код возврата 0 — всё работает.
fn selftest(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--system-audio") {
        // SAFETY: до запуска рантайма и потоков
        unsafe { std::env::set_var("HYST_CAST_SYSTEM_AUDIO", "1") };
    }
    let server = arg(args, "--server").unwrap_or_else(|| "127.0.0.1:4433".into());
    let viewer_server = arg(args, "--viewer-server").unwrap_or_else(|| server.clone());
    let fingerprint = match (arg(args, "--fp"), arg(args, "--cert")) {
        (Some(fp), _) => fp,
        (None, Some(cert)) => tls::fingerprint_of_file(&cert)?,
        (None, None) => config::Config::default().fingerprint,
    };
    let token = arg(args, "--token").or_else(|| std::env::var("HYST_CAST_TOKEN").ok()).unwrap_or_else(|| config::Config::default().token);
    let seconds: u64 = arg(args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(12);
    let preset = config::preset(&arg(args, "--preset").unwrap_or_else(|| "720p30".into()));
    let encoder = arg(args, "--encoder").unwrap_or_default();
    let show = args.iter().any(|a| a == "--show");
    let room = arg(args, "--room").unwrap_or_else(|| format!("selftest-{}", std::process::id()));

    let rt = runtime()?;
    let mk = |name: &str, server: &str| config::Config {
        server: server.to_string(),
        fingerprint: fingerprint.clone(),
        token: token.clone(),
        name: name.into(),
        room: room.clone(),
        latency_ms: arg(args, "--latency").and_then(|s| s.parse().ok()).unwrap_or(200),
        ..Default::default()
    };
    let nop: net::Repaint = Arc::new(|| {});
    let sa: net::SharedRef = Arc::new(Mutex::new(net::Shared::default()));
    let sb: net::SharedRef = Arc::new(Mutex::new(net::Shared::default()));
    let ta = net::spawn(rt.handle(), sa.clone(), nop.clone(), watch::SinkMode::Fake);
    let tb = net::spawn(
        rt.handle(),
        sb.clone(),
        nop,
        if show {
            watch::SinkMode::Window
        } else if args.iter().any(|a| a == "--embedded") {
            watch::SinkMode::Embedded
        } else {
            watch::SinkMode::Fake
        },
    );

    let online = |s: &net::SharedRef| {
        let st = s.lock().unwrap().status.clone();
        match st {
            net::Status::Online => Some(Ok(())),
            net::Status::Error(e) => Some(Err(anyhow::anyhow!(e))),
            _ => None,
        }
    };
    println!("[1/5] подключение к {server}");
    ta.send(net::Cmd::Connect(mk("Стример", &server))).ok();
    wait_until("стример в сети", Duration::from_secs(15), || online(&sa)).context("стример не подключился")?;
    tb.send(net::Cmd::Connect(mk("Зритель", &viewer_server))).ok();
    wait_until("зритель в сети", Duration::from_secs(15), || online(&sb)).context("зритель не подключился")?;
    let a_id = sa.lock().unwrap().my_id;

    println!("[2/5] запуск стрима {} (тест-картинка + тон 440 Гц)", preset.label);
    // --source screen — настоящий захват экрана (первый монитор / портал), иначе тест-картинка
    let source = match arg(args, "--source").as_deref() {
        Some("screen") => capture::list_sources().into_iter().next().map(|s| s.0).unwrap_or(capture::Source::Test),
        Some("camera") => capture::cameras().into_iter().next().map(|s| s.0).context("камер не найдено")?,
        Some("window") => {
            let want = arg(args, "--window").unwrap_or_default().to_lowercase();
            capture::list_sources()
                .into_iter()
                .find(|s| s.1.starts_with("Окно:") && s.1.to_lowercase().contains(&want))
                .map(|s| {
                    println!("      окно: {}", s.1);
                    s.0
                })
                .context("окон не найдено")?
        }
        _ => capture::Source::Test,
    };
    println!("      источник: {source:?}");
    ta.send(net::Cmd::Start { source, preset, audio: true, encoder, portal_token: None }).ok();
    wait_until("стрим запустился", Duration::from_secs(90), || {
        let s = sa.lock().unwrap();
        if s.publishing.is_some() {
            Some(Ok(()))
        } else {
            s.notice.clone().map(|n| Err(anyhow::anyhow!(n)))
        }
    })?;
    let enc = sa.lock().unwrap().publishing.as_ref().and_then(|p| p.info.clone()).map(|i| i.encoder).unwrap_or_default();
    println!("      кодер: {enc}");

    println!("[3/5] зритель открывает стрим");
    wait_until("зритель увидел стрим", Duration::from_secs(10), || sb.lock().unwrap().peers.get(&a_id).and_then(|p| p.stream.as_ref()).map(|_| Ok(())))?;
    tb.send(net::Cmd::Watch(a_id)).ok();
    wait_until("первый кадр у зрителя", Duration::from_secs(30), || {
        let s = sb.lock().unwrap();
        if let Some(n) = &s.notice {
            return Some(Err(anyhow::anyhow!(n.clone())));
        }
        s.watch.get(&a_id).filter(|w| w.rendered > 0).map(|_| Ok(()))
    })?;

    println!("[4/5] смотрим {seconds} с");
    let snap = |s: &net::SharedRef| s.lock().unwrap().watch.get(&a_id).cloned().unwrap_or_default();
    std::thread::sleep(Duration::from_secs(2));
    let w0 = snap(&sb);
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(seconds));
    let w1 = snap(&sb);
    let dt = t0.elapsed().as_secs_f64();
    let still_live = sa.lock().unwrap().publishing.is_some();
    let still_watching = sb.lock().unwrap().watch.contains_key(&a_id);
    let p = sa.lock().unwrap().publishing.clone().unwrap_or_default();
    // saturating: если просмотр пропал, счётчики обнулились — это провал, а не «огромный fps»
    let fps = w1.rendered.saturating_sub(w0.rendered) as f64 / dt;
    let audio_rate = w1.audio_rendered.saturating_sub(w0.audio_rendered) as f64 / dt;

    println!("[5/5] итог");
    println!("      стример: {}", p.stats);
    println!("      стример: кодер {enc}, {:.0} fps, {:.1} Мбит/с, выкинуто {} кадров, звук {} пакетов", p.fps, p.kbps / 1000.0, p.drops, p.audio_packets);
    println!("      зритель: декодер {}, показано {:.1} fps, звук {:.0} буф/с, запас {} мс, пересинхронизаций {}, выкинуто опоздавших звуков {}, задержка захват→показ {} мс", w1.decoder, fps, audio_rate, w1.margin_ms, w1.resyncs, w1.late_drops, w1.latency_ms);
    let report = format!(
        "{{\"encoder\":{:?},\"decoder\":{:?},\"fps\":{fps:.1},\"target_fps\":{},\"audio_buffers_per_s\":{audio_rate:.1},\"kbps\":{:.0},\"drops\":{},\"margin_ms\":{},\"resyncs\":{},\"latency_ms\":{}}}",
        enc, w1.decoder, preset.fps, p.kbps, p.drops, w1.margin_ms, w1.resyncs, w1.latency_ms
    );
    println!("{report}");
    if let Some(slot) = sb.lock().unwrap().frames.get(&a_id) {
        let f = slot.frame.lock().unwrap();
        println!("      кадры для окна: {} шт., последний {}", slot.seq.load(std::sync::atomic::Ordering::Relaxed),
            f.as_ref().map(|f| format!("{}×{} RGBA, {} байт", f.w, f.h, f.rgba.len())).unwrap_or_else(|| "нет".into()));
    }
    if !w1.jitter.is_empty() {
        println!("      джиттер звука (насколько позже самого быстрого пакета): {}", w1.jitter);
    }
    if let Some(path) = arg(args, "--report") {
        let _ = std::fs::write(path, &report);
    }

    let mut problems = Vec::new();
    if !still_live {
        problems.push("стрим остановился во время проверки".to_string());
    }
    if !still_watching {
        problems.push("просмотр прервался во время проверки".to_string());
    }
    if args.iter().any(|a| a == "--extended") {
        let ctx = Ext { rt: rt.handle().clone(), sa: sa.clone(), sb: sb.clone(), ta: ta.clone(), tb: tb.clone(), mk: &mk, server: server.clone(), preset };
        problems.extend(extended(&ctx));
    }

    ta.send(net::Cmd::Disconnect).ok();
    tb.send(net::Cmd::Disconnect).ok();
    std::thread::sleep(Duration::from_millis(500));

    if fps < preset.fps as f64 * 0.6 {
        problems.push(format!("мало кадров у зрителя: {fps:.1} из {}", preset.fps));
    }
    if audio_rate < 10.0 {
        problems.push(format!("звук почти не доходит: {audio_rate:.1} буф/с"));
    }
    if problems.is_empty() {
        println!("САМОПРОВЕРКА ПРОЙДЕНА");
        Ok(())
    } else {
        bail!("САМОПРОВЕРКА НЕ ПРОЙДЕНА: {}", problems.join("; "))
    }
}

// ---------------------------------------------------------------- расширенная самопроверка

struct Ext<'a> {
    rt: tokio::runtime::Handle,
    sa: net::SharedRef,
    sb: net::SharedRef,
    ta: tokio::sync::mpsc::UnboundedSender<net::Cmd>,
    tb: tokio::sync::mpsc::UnboundedSender<net::Cmd>,
    mk: &'a dyn Fn(&str, &str) -> config::Config,
    server: String,
    preset: config::Preset,
}

fn id_by_name(s: &net::SharedRef, name: &str) -> Option<u32> {
    s.lock().unwrap().peers.iter().find(|(_, p)| p.name == name).map(|(id, _)| *id)
}

/// Ждём, пока у зрителя пойдут кадры от стримера с этим именем.
fn frames_from(s: &net::SharedRef, name: &str, secs: u64) -> bool {
    wait_until("кадры", Duration::from_secs(secs), || {
        let st = s.lock().unwrap();
        let id = st.peers.iter().find(|(_, p)| p.name == name).map(|(id, _)| *id)?;
        st.watch.get(&id).filter(|w| w.rendered > 10).map(|_| Ok(()))
    })
    .is_ok()
}

fn status_is(s: &net::SharedRef, f: impl Fn(&net::Status) -> bool, secs: u64) -> bool {
    wait_until("статус", Duration::from_secs(secs), || f(&s.lock().unwrap().status).then_some(Ok(()))).is_ok()
}

fn extended(c: &Ext) -> Vec<String> {
    let mut bad = Vec::new();
    let mut check = |name: &str, ok: bool| {
        println!("      {} {name}", if ok { "✔" } else { "✘" });
        if !ok {
            bad.push(name.to_string());
        }
    };
    let new_client = |name: &str, token: Option<&str>| {
        let s: net::SharedRef = Arc::new(Mutex::new(net::Shared::default()));
        let t = net::spawn(&c.rt, s.clone(), Arc::new(|| {}), watch::SinkMode::Fake);
        let mut cfg = (c.mk)(name, &c.server);
        if let Some(tok) = token {
            cfg.token = tok.into();
        }
        t.send(net::Cmd::Connect(cfg)).ok();
        (s, t)
    };
    println!("[ext] сценарии");

    // 1. Закрыть и снова открыть просмотр
    let a = id_by_name(&c.sb, "Стример").unwrap_or(0);
    c.tb.send(net::Cmd::Unwatch(a)).ok();
    std::thread::sleep(Duration::from_secs(1));
    let closed = !c.sb.lock().unwrap().watch.contains_key(&a);
    c.tb.send(net::Cmd::Watch(a)).ok();
    check("закрыть и снова открыть просмотр", closed && frames_from(&c.sb, "Стример", 20));

    // 2. Стример перезапускает стрим — зритель подхватывает сам
    c.ta.send(net::Cmd::Stop).ok();
    let stopped = wait_until("стоп", Duration::from_secs(5), || {
        c.sb.lock().unwrap().peers.get(&a).filter(|p| p.stream.is_none()).map(|_| Ok(()))
    })
    .is_ok();
    c.ta.send(net::Cmd::Start { source: capture::Source::Test, preset: c.preset, audio: true, encoder: String::new(), portal_token: None }).ok();
    check("перезапуск стрима: зритель подхватил сам", stopped && frames_from(&c.sb, "Стример", 30));

    // 3. Обрыв связи у зрителя
    c.tb.send(net::Cmd::DebugDrop).ok();
    let dropped = status_is(&c.sb, |s| matches!(s, net::Status::Error(_) | net::Status::Connecting), 5);
    let back = status_is(&c.sb, |s| *s == net::Status::Online, 20);
    check("обрыв у зрителя: переподключился и смотрит дальше", dropped && back && frames_from(&c.sb, "Стример", 30));

    // 4. Обрыв связи у стримера
    c.ta.send(net::Cmd::DebugDrop).ok();
    let dropped = status_is(&c.sa, |s| !matches!(s, net::Status::Online), 5);
    let back = status_is(&c.sa, |s| *s == net::Status::Online, 20);
    let streaming = wait_until("стрим снова", Duration::from_secs(40), || c.sa.lock().unwrap().publishing.as_ref().map(|_| Ok(()))).is_ok();
    check("обрыв у стримера: стрим восстановился сам", dropped && back && streaming);
    check("обрыв у стримера: зритель снова видит", frames_from(&c.sb, "Стример", 30));

    // 5. Одинаковое имя
    let (sc, tc) = new_client("Зритель", None);
    let online = status_is(&sc, |s| *s == net::Status::Online, 15);
    std::thread::sleep(Duration::from_millis(500));
    check("одинаковое имя → «Зритель 2»", online && id_by_name(&c.sa, "Зритель 2").is_some());
    tc.send(net::Cmd::Disconnect).ok();

    // 6. Неверный пароль
    let (sd, td) = new_client("Чужой", Some("неверный"));
    let err = wait_until("ошибка пароля", Duration::from_secs(15), || match &sd.lock().unwrap().status {
        net::Status::Error(e) if e.contains("пароль") => Some(Ok(())),
        _ => None,
    })
    .is_ok();
    check("неверный пароль → понятная ошибка", err);
    td.send(net::Cmd::Disconnect).ok();

    // 7. Седьмой в комнату не попадает
    std::thread::sleep(Duration::from_secs(1));
    let extra: Vec<_> = (3..=6).map(|i| new_client(&format!("Гость {i}"), None)).collect();
    let all_in = extra.iter().all(|(s, _)| status_is(s, |st| *st == net::Status::Online, 15));
    let (s7, t7) = new_client("Седьмой", None);
    let full = wait_until("полная комната", Duration::from_secs(15), || match &s7.lock().unwrap().status {
        net::Status::Error(e) if e.contains("заполнена") => Some(Ok(())),
        _ => None,
    })
    .is_ok();
    check("6 человек входят, седьмой получает «Комната заполнена»", all_in && full);
    t7.send(net::Cmd::Disconnect).ok();
    for (_, t) in &extra {
        t.send(net::Cmd::Disconnect).ok();
    }
    std::thread::sleep(Duration::from_millis(500));
    bad
}
