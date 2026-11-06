//! Источники картинки и звука для стрима.
//!
//! * Linux (Wayland): xdg-desktop-portal ScreenCast → PipeWire → `pipewiresrc`; звук — монитор
//!   звуковой карты через `pulsesrc` (pipewire-pulse).
//! * Windows: `d3d11screencapturesrc` (монитор или окно), звук — `wasapi2src loopback=true`.
//! * Камера (обе ОС): любое устройство из `DeviceMonitor` (PipeWire/V4L2 на Linux, Media Foundation
//!   на Windows); режим подбирается по возможностям камеры, звук — микрофон.
//! * Тест: `videotestsrc` + тон 440 Гц — для самопроверки и CI.

use anyhow::{Context, Result};
use gstreamer as gst;
use gst::prelude::*;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Test,
    #[cfg(target_os = "linux")]
    Portal,
    #[cfg(windows)]
    Monitor { index: i32 },
    #[cfg(windows)]
    Window { hwnd: u64 },
    /// Камера: начало конвейера (элемент-источник) и её возможности (caps) для выбора режима.
    Camera { src: String, caps: String },
}

impl Source {
    pub fn is_camera(&self) -> bool {
        matches!(self, Source::Camera { .. })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AudioKind {
    Test,
    /// Звук системы (то, что слышно в колонках).
    System,
    /// Микрофон (для камеры).
    Mic,
}

/// Что показать в выпадающем списке «Что стримить».
pub fn list_sources() -> Vec<(Source, String)> {
    let mut v = Vec::new();
    #[cfg(target_os = "linux")]
    v.push((Source::Portal, "Экран или окно…".to_string()));
    #[cfg(windows)]
    {
        v.extend(win::monitors());
        v.extend(win::windows());
    }
    v.extend(cameras());
    v.push((Source::Test, "Тестовая картинка".to_string()));
    v
}

/// Камеры через DeviceMonitor GStreamer: по каждому устройству — строка источника для конвейера.
pub fn cameras() -> Vec<(Source, String)> {
    let mon = gst::DeviceMonitor::new();
    mon.add_filter(Some("Video/Source"), None);
    let mut v = Vec::new();
    if mon.start().is_err() {
        return v;
    }
    for d in mon.devices() {
        let Some(props) = d.properties() else { continue };
        let factory = d.create_element(None).ok().and_then(|e| e.factory()).map(|f| f.name().to_string()).unwrap_or_default();
        let prop = |k: &str| -> Option<String> {
            let val = props.value(k).ok()?;
            val.get::<String>().ok().or_else(|| val.serialize().ok().map(|s| s.to_string()))
        };
        // Linux: если знаем /dev/videoN — берём камеру напрямую через v4l2 и выключаем «динамическую частоту»:
        // иначе в полумраке камера сама падает до 7–8 fps ради выдержки.
        let v4l2_path = prop("api.v4l2.path").or_else(|| if factory == "v4l2src" { prop("device.path") } else { None });
        let src = match factory.as_str() {
            _ if cfg!(target_os = "linux") && v4l2_path.is_some() => format!(
                "v4l2src device={} do-timestamp=true extra-controls=\"c,exposure_dynamic_framerate=0\"",
                v4l2_path.clone().unwrap()
            ),
            "pipewiresrc" => match prop("object.serial").or_else(|| prop("node.name")) {
                Some(id) => format!("pipewiresrc target-object={id} do-timestamp=true"),
                None => continue,
            },
            "v4l2src" => match prop("device.path").or_else(|| prop("api.v4l2.path")) {
                Some(p) => format!("v4l2src device={p} do-timestamp=true"),
                None => continue,
            },
            "mfvideosrc" => match prop("device.path") {
                // В синтаксисе конвейера «\\» внутри кавычек — экранирование, а путь вида \\?\usb#… — сплошь из них
                Some(p) => format!("mfvideosrc device-path=\"{}\"", p.replace('"', "").replace('\\', "\\\\")),
                None => format!("mfvideosrc device-name=\"{}\"", d.display_name().replace('"', "")),
            },
            _ => continue,
        };
        let caps = d.caps().map(|c| c.to_string()).unwrap_or_default();
        let name = d.display_name().replace(" (V4L2)", "");
        v.push((Source::Camera { src, caps }, format!("Камера: {name}")));
    }
    mon.stop();
    v
}

/// Лучший режим камеры: наибольшая картинка не больше max_w×max_h при ≥ 24 fps (если такой нет —
/// просто наибольшая). Возвращает (ширина, высота, кадров/с числитель, знаменатель).
pub fn best_camera_mode(caps: &str, max_w: u32, max_h: u32, want_fps: u32) -> Option<(u32, u32, i32, i32)> {
    let caps: gst::Caps = caps.parse().ok()?;
    let mut modes: Vec<(u32, u32, i32, i32)> = Vec::new();
    for s in caps.iter() {
        if s.name() != "video/x-raw" && s.name() != "image/jpeg" {
            continue;
        }
        let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) else { continue };
        // Частоты: одна, список или диапазон
        let mut rates: Vec<gst::Fraction> = Vec::new();
        if let Ok(f) = s.get::<gst::Fraction>("framerate") {
            rates.push(f);
        } else if let Ok(list) = s.get::<gst::List>("framerate") {
            rates.extend(list.iter().filter_map(|v| v.get::<gst::Fraction>().ok()));
        } else if let Ok(r) = s.get::<gst::FractionRange>("framerate") {
            let target = gst::Fraction::new(want_fps as i32, 1);
            rates.push(if target > r.max() { r.max() } else if target < r.min() { r.min() } else { target });
        }
        // Самая высокая частота, не выше нужной
        let best = rates
            .iter()
            .filter(|f| f.numer() > 0 && (f.numer() as f64 / f.denom() as f64) <= want_fps as f64 + 0.5)
            .max_by(|a, b| (a.numer() as f64 / a.denom() as f64).total_cmp(&(b.numer() as f64 / b.denom() as f64)))
            .or_else(|| rates.iter().min_by(|a, b| (a.numer() as f64 / a.denom() as f64).total_cmp(&(b.numer() as f64 / b.denom() as f64))));
        if let Some(f) = best {
            modes.push((w as u32, h as u32, f.numer(), f.denom()));
        }
    }
    let fits = |m: &&(u32, u32, i32, i32)| m.0 <= max_w && m.1 <= max_h;
    let fps_ok = |m: &&(u32, u32, i32, i32)| m.2 as f64 / m.3 as f64 >= 23.9;
    let area = |m: &&(u32, u32, i32, i32)| (m.0 * m.1, (m.2 as f64 / m.3 as f64 * 100.0) as u32);
    modes.iter().filter(fits).filter(fps_ok).max_by_key(area)
        .or_else(|| modes.iter().filter(fits).max_by_key(area))
        .or_else(|| modes.iter().min_by_key(|m| m.0 * m.1))
        .copied()
}

/// Открытый захват: описание начала конвейера + то, что надо держать живым, пока идёт стрим.
pub struct Capture {
    pub video_desc: String,
    /// Свой размер картинки (камера) — тогда кодируем в нём, а не в размере пресета.
    pub size: Option<(u32, u32)>,
    /// Новый токен портала (Linux), чтобы в следующий раз не спрашивать.
    pub portal_token: Option<String>,
    #[cfg(target_os = "linux")]
    _portal: Option<linux::PortalSession>,
}

pub fn audio_desc(kind: AudioKind) -> String {
    if kind == AudioKind::Test {
        return "audiotestsrc is-live=true wave=sine freq=440 volume=0.3".into();
    }
    if kind == AudioKind::Mic {
        #[cfg(windows)]
        {
            if gstreamer::ElementFactory::find("wasapi2src").is_some() {
                return "wasapi2src low-latency=true".into();
            }
            return "wasapisrc low-latency=true".into();
        }
        #[cfg(not(windows))]
        return "pulsesrc do-timestamp=true".into();
    }
    #[cfg(windows)]
    {
        // wasapi2 есть только в MSVC-сборке GStreamer; в mingw-сборке — старый wasapi, он тоже умеет loopback
        if gstreamer::ElementFactory::find("wasapi2src").is_some() {
            return "wasapi2src loopback=true low-latency=true".into();
        }
        return "wasapisrc loopback=true low-latency=true".into();
    }
    #[cfg(not(windows))]
    return "pulsesrc device=@DEFAULT_MONITOR@ do-timestamp=true".into();
}

/// Особый вариант звука на Linux: микшер из потоков отдельных приложений (собирается в publish.rs).
pub const PW_APPS: &str = "pw-apps";

/// Кого не захватывать: голосовые чаты (иначе голоса звучат у друзей дважды) и сам hyst-cast.
pub fn excluded_app(name: &str, binary: &str, pid: u32) -> bool {
    let s = format!("{} {}", name.to_lowercase(), binary.to_lowercase());
    pid == std::process::id()
        || ["discord", "vesktop", "webcord", "armcord", "legcord", "equibop", "hyst-cast"].iter().any(|x| s.contains(x))
}

/// Потоки воспроизведения приложений сейчас (Linux, через pw-dump): serial → «имя (программа)».
#[cfg(target_os = "linux")]
pub fn pw_app_streams() -> Vec<(u64, String, bool)> {
    let Ok(out) = std::process::Command::new("pw-dump").output() else { return Vec::new() };
    let Ok(all) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return Vec::new() };
    let mut v = Vec::new();
    for o in all.as_array().into_iter().flatten() {
        let p = &o["info"]["props"];
        if p["media.class"] != "Stream/Output/Audio" {
            continue;
        }
        let Some(serial) = p["object.serial"].as_u64() else { continue };
        let name = p["application.name"].as_str().unwrap_or("");
        let bin = p["application.process.binary"].as_str().unwrap_or("");
        let pid = p["application.process.id"].as_u64().unwrap_or(0) as u32;
        v.push((serial, format!("{name} ({bin})"), excluded_app(name, bin, pid)));
    }
    v
}

/// Варианты захвата звука по убыванию предпочтения: (описание для конвейера, подпись для человека).
/// Windows 10 2004+/11 умеет захват по процессам: окно — только звук этого приложения, экран —
/// всё, кроме Discord (иначе голоса из звонка у друзей звучат дважды). Если не вышло — обычный захват.
pub fn audio_candidates(kind: AudioKind, source: &Source) -> Vec<(String, String)> {
    let mut v = Vec::new();
    #[cfg(windows)]
    if kind == AudioKind::System && gstreamer::ElementFactory::find("wasapi2src").is_some() {
        let base = "wasapi2src loopback=true low-latency=true";
        match source {
            Source::Window { hwnd } => {
                if let Some(pid) = win::window_pid(*hwnd) {
                    v.push((format!("{base} loopback-mode=include-process-tree loopback-target-pid={pid}"), "только звук этого окна".to_string()));
                }
            }
            _ => {
                if let Some(pid) = win::discord_pid() {
                    v.push((format!("{base} loopback-mode=exclude-process-tree loopback-target-pid={pid}"), "весь звук, кроме Discord".to_string()));
                } else {
                    // Discord не запущен — исключаем себя, чтобы не захватывать чужие стримы, которые смотришь
                    v.push((format!("{base} loopback-mode=exclude-process-tree loopback-target-pid={}", std::process::id()), "весь звук системы".to_string()));
                }
            }
        }
    }
    // Linux: свой микшер приложений через PipeWire — без Discord и без самого hyst-cast
    #[cfg(target_os = "linux")]
    if kind == AudioKind::System && gstreamer::ElementFactory::find("pipewiresrc").is_some() && gstreamer::ElementFactory::find("audiomixer").is_some() {
        v.push((PW_APPS.to_string(), "звук приложений, кроме Discord".to_string()));
    }
    let _ = source;
    let hint = match kind {
        AudioKind::Mic => "микрофон",
        AudioKind::Test => "тестовый тон",
        AudioKind::System => "весь звук системы",
    };
    v.push((audio_desc(kind), hint.to_string()));
    v
}

pub async fn open(source: &Source, fps: u32, max: (u32, u32), portal_token: Option<String>) -> Result<Capture> {
    let _ = &portal_token;
    match source {
        Source::Camera { src, caps } => {
            let (w, h, fn_, fd) = best_camera_mode(caps, max.0, max.1, fps).context("камера не сообщила ни одного режима")?;
            // Любой формат (MJPEG или сырой) в выбранном режиме; decodebin сам разберётся, в обычную память
            let video_desc = format!(
                "{src} ! capsfilter caps=\"image/jpeg,width={w},height={h},framerate={fn_}/{fd};video/x-raw,width={w},height={h},framerate={fn_}/{fd}\" ! decodebin caps=video/x-raw ! videoconvert ! videorate drop-only=true max-rate={fps}"
            );
            tracing::info!("камера: {w}×{h} @ {fn_}/{fd}");
            // Чётные размеры — кодеры их любят
            Ok(Capture {
                video_desc,
                size: Some((w & !1, h & !1)),
                portal_token: None,
                #[cfg(target_os = "linux")]
                _portal: None,
            })
        }
        Source::Test => Ok(Capture {
            video_desc: format!(
                "videotestsrc is-live=true pattern=ball motion=sweep ! video/x-raw,width=960,height=540,framerate={fps}/1"
            ),
            size: None,
            portal_token: None,
            #[cfg(target_os = "linux")]
            _portal: None,
        }),
        #[cfg(target_os = "linux")]
        Source::Portal => {
            let (session, node, fd, token) = linux::open_portal(portal_token).await?;
            // Частоту ограничиваем videorate: портал шлёт кадры только когда экран меняется.
            let video_desc = format!(
                "pipewiresrc fd={fd} path={node} do-timestamp=true keepalive-time=1000 always-copy=false ! videorate drop-only=true max-rate={fps}"
            );
            Ok(Capture { video_desc, size: None, portal_token: token, _portal: Some(session) })
        }
        #[cfg(windows)]
        Source::Monitor { index } => Ok(Capture {
            video_desc: format!(
                "d3d11screencapturesrc monitor-index={index} show-cursor=true ! video/x-raw(memory:D3D11Memory),framerate={fps}/1"
            ),
            size: None,
            portal_token: None,
        }),
        #[cfg(windows)]
        Source::Window { hwnd } => Ok(Capture {
            video_desc: format!(
                "d3d11screencapturesrc window-handle={hwnd} capture-api=wgc show-cursor=true ! video/x-raw(memory:D3D11Memory),framerate={fps}/1"
            ),
            size: None,
            portal_token: None,
        }),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result};
    use ashpd::desktop::screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
    };
    use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
    use std::os::fd::{AsRawFd, OwnedFd};

    /// Сессия портала живёт, пока идёт стрим; при удалении закрывается.
    pub struct PortalSession {
        _proxy: Screencast,
        session: Option<Session<Screencast>>,
        _fd: OwnedFd,
    }

    impl Drop for PortalSession {
        fn drop(&mut self) {
            if let Some(s) = self.session.take() {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move {
                        let _ = s.close().await;
                    });
                }
            }
        }
    }

    pub async fn open_portal(token: Option<String>) -> Result<(PortalSession, u32, i32, Option<String>)> {
        let proxy = Screencast::new().await.context("нет xdg-desktop-portal (ScreenCast)")?;
        let session = proxy.create_session(CreateSessionOptions::default()).await?;
        let opts = SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::EMBEDDED)
            .set_sources(SourceType::MONITOR | SourceType::WINDOW)
            .set_multiple(false)
            .set_restore_token(token.as_deref())
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        proxy.select_sources(&session, opts).await?;
        let streams = proxy
            .start(&session, None, StartCastOptions::default())
            .await?
            .response()
            .context("выбор экрана отменён")?;
        let stream = streams.streams().first().context("портал не вернул ни одного экрана")?;
        let node = stream.pipe_wire_node_id();
        let new_token = streams.restore_token().map(str::to_string);
        let fd = proxy.open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default()).await?;
        let raw = fd.as_raw_fd();
        Ok((PortalSession { _proxy: proxy, session: Some(session), _fd: fd }, node, raw, new_token))
    }
}

#[cfg(windows)]
mod win {
    use super::Source;
    use gstreamer as gst;
    use gst::prelude::*;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, IsIconic, IsWindowVisible, GWL_EXSTYLE,
        WS_EX_TOOLWINDOW,
    };

    pub fn monitors() -> Vec<(Source, String)> {
        // Мониторы перечисляет сам GStreamer (провайдер устройств d3d11)
        let mon = gst::DeviceMonitor::new();
        mon.add_filter(Some("Source/Monitor"), None);
        let mut v = Vec::new();
        if mon.start().is_ok() {
            for (i, d) in mon.devices().iter().enumerate() {
                let mut label = format!("Экран {}", i + 1);
                if let Some(p) = d.properties() {
                    let w = p.get::<i32>("display.coordinates.right").ok().zip(p.get::<i32>("display.coordinates.left").ok());
                    let h = p.get::<i32>("display.coordinates.bottom").ok().zip(p.get::<i32>("display.coordinates.top").ok());
                    if let (Some((r, l)), Some((b, t))) = (w, h) {
                        label = format!("Экран {} ({}×{})", i + 1, r - l, b - t);
                    }
                    if p.get::<bool>("device.primary").unwrap_or(false) {
                        label.push_str(" — основной");
                    }
                }
                v.push((Source::Monitor { index: i as i32 }, label));
            }
            mon.stop();
        }
        if v.is_empty() {
            v.push((Source::Monitor { index: 0 }, "Экран 1".to_string()));
        }
        v
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<(Source, String)>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return BOOL(1);
            }
            if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return BOOL(1);
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            if title == "Program Manager" || title.starts_with("hyst-cast") {
                return BOOL(1);
            }
            let short: String = title.chars().take(60).collect();
            out.push((Source::Window { hwnd: hwnd.0 as u64 }, format!("Окно: {short}")));
        }
        BOOL(1)
    }

    /// Процесс, которому принадлежит окно.
    pub fn window_pid(hwnd: u64) -> Option<u32> {
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)) };
        (pid != 0).then_some(pid)
    }

    /// Главный процесс Discord (тот, чей родитель — не Discord): его дерево — все процессы Discord.
    pub fn discord_pid() -> Option<u32> {
        use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
        let mut procs: Vec<(u32, u32, String)> = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(snap, &mut e).is_ok() {
                loop {
                    let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    procs.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase()));
                    if Process32NextW(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            let _ = windows::Win32::Foundation::CloseHandle(snap);
        }
        let is_discord = |n: &str| n.starts_with("discord") && n.ends_with(".exe");
        let ids: std::collections::HashSet<u32> = procs.iter().filter(|p| is_discord(&p.2)).map(|p| p.0).collect();
        procs.iter().find(|p| is_discord(&p.2) && !ids.contains(&p.1)).map(|p| p.0)
    }

    pub fn windows() -> Vec<(Source, String)> {
        let mut v: Vec<(Source, String)> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut v as *mut _ as isize));
        }
        v
    }
}
