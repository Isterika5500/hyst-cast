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
    let headless = args.iter().any(|a| a == "--selftest" || a == "--probe");
    init_logging(headless);
    if let Err(e) = gst::init() {
        fatal(&format!("GStreamer не запустился: {e}"));
    }
    let r = if args.iter().any(|a| a == "--probe") {
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
            .with_inner_size([420.0, 580.0])
            .with_min_inner_size([340.0, 380.0]),
        ..Default::default()
    };
    eframe::run_native(
        "hyst-cast",
        opts,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let repaint: net::Repaint = Arc::new(move || ctx.request_repaint());
            let tx = net::spawn(&handle, shared.clone(), repaint, watch::SinkMode::Window);
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
        println!("  {:<28} {:<7} {:?}  {} ({} мс)", e.factory, if e.family.hardware() { "GPU" } else { "CPU" }, e.path, if ok { "OK" } else { "не работает" }, t.elapsed().as_millis());
    }
    println!("\nДекодеры H.264:");
    for d in codecs::decoder_candidates() {
        println!("  {:<28} {:<7} {}", d.factory, if d.hardware { "GPU" } else { "CPU" }, if codecs::probe_decoder(&d) { "OK" } else { "не работает" });
    }
    println!("\nИсточники:");
    for (_, l) in capture::list_sources() {
        println!("  {l}");
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
    let server = arg(args, "--server").unwrap_or_else(|| "127.0.0.1:4433".into());
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
    let room = format!("selftest-{}", std::process::id());

    let rt = runtime()?;
    let mk = |name: &str| config::Config {
        server: server.clone(),
        fingerprint: fingerprint.clone(),
        token: token.clone(),
        name: name.into(),
        room: room.clone(),
        latency_ms: 200,
        ..Default::default()
    };
    let nop: net::Repaint = Arc::new(|| {});
    let sa: net::SharedRef = Arc::new(Mutex::new(net::Shared::default()));
    let sb: net::SharedRef = Arc::new(Mutex::new(net::Shared::default()));
    let ta = net::spawn(rt.handle(), sa.clone(), nop.clone(), watch::SinkMode::Fake);
    let tb = net::spawn(rt.handle(), sb.clone(), nop, if show { watch::SinkMode::Window } else { watch::SinkMode::Fake });

    let online = |s: &net::SharedRef| {
        let st = s.lock().unwrap().status.clone();
        match st {
            net::Status::Online => Some(Ok(())),
            net::Status::Error(e) => Some(Err(anyhow::anyhow!(e))),
            _ => None,
        }
    };
    println!("[1/5] подключение к {server}");
    ta.send(net::Cmd::Connect(mk("Стример"))).ok();
    wait_until("стример в сети", Duration::from_secs(15), || online(&sa)).context("стример не подключился")?;
    tb.send(net::Cmd::Connect(mk("Зритель"))).ok();
    wait_until("зритель в сети", Duration::from_secs(15), || online(&sb)).context("зритель не подключился")?;
    let a_id = sa.lock().unwrap().my_id;

    println!("[2/5] запуск стрима {} (тест-картинка + тон 440 Гц)", preset.label);
    ta.send(net::Cmd::Start { source: capture::Source::Test, preset, audio: true, encoder, portal_token: None }).ok();
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
    let p = sa.lock().unwrap().publishing.clone().unwrap_or_default();
    let fps = (w1.rendered - w0.rendered) as f64 / dt;
    let audio_rate = (w1.audio_rendered - w0.audio_rendered) as f64 / dt;

    println!("[5/5] итог");
    println!("      стример: кодер {enc}, {:.0} fps, {:.1} Мбит/с, выкинуто {} кадров, звук {} пакетов", p.fps, p.kbps / 1000.0, p.drops, p.audio_packets);
    println!("      зритель: декодер {}, показано {:.1} fps, звук {:.0} буф/с, запас {} мс, пересинхронизаций {}", w1.decoder, fps, audio_rate, w1.margin_ms, w1.resyncs);
    let report = format!(
        "{{\"encoder\":{:?},\"decoder\":{:?},\"fps\":{fps:.1},\"target_fps\":{},\"audio_buffers_per_s\":{audio_rate:.1},\"kbps\":{:.0},\"drops\":{},\"margin_ms\":{},\"resyncs\":{}}}",
        enc, w1.decoder, preset.fps, p.kbps, p.drops, w1.margin_ms, w1.resyncs
    );
    println!("{report}");
    if let Some(path) = arg(args, "--report") {
        let _ = std::fs::write(path, &report);
    }

    ta.send(net::Cmd::Disconnect).ok();
    tb.send(net::Cmd::Disconnect).ok();
    std::thread::sleep(Duration::from_millis(500));

    let mut problems = Vec::new();
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
