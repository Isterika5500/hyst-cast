//! Источники картинки и звука для стрима.
//!
//! * Linux (Wayland): xdg-desktop-portal ScreenCast → PipeWire → `pipewiresrc`; звук — монитор
//!   звуковой карты через `pulsesrc` (pipewire-pulse).
//! * Windows: `d3d11screencapturesrc` (монитор или окно), звук — `wasapi2src loopback=true`.
//! * Тест: `videotestsrc` + тон 440 Гц — для самопроверки и CI.

use anyhow::Result;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Test,
    #[cfg(target_os = "linux")]
    Portal,
    #[cfg(windows)]
    Monitor { index: i32 },
    #[cfg(windows)]
    Window { hwnd: u64 },
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
    v.push((Source::Test, "Тестовая картинка".to_string()));
    v
}

/// Открытый захват: описание начала конвейера + то, что надо держать живым, пока идёт стрим.
pub struct Capture {
    pub video_desc: String,
    /// Новый токен портала (Linux), чтобы в следующий раз не спрашивать.
    pub portal_token: Option<String>,
    #[cfg(target_os = "linux")]
    _portal: Option<linux::PortalSession>,
}

pub fn audio_desc(test: bool) -> String {
    if test {
        return "audiotestsrc is-live=true wave=sine freq=440 volume=0.3".into();
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

pub async fn open(source: &Source, fps: u32, portal_token: Option<String>) -> Result<Capture> {
    let _ = &portal_token;
    match source {
        Source::Test => Ok(Capture {
            video_desc: format!(
                "videotestsrc is-live=true pattern=ball motion=sweep ! video/x-raw,width=960,height=540,framerate={fps}/1"
            ),
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
            Ok(Capture { video_desc, portal_token: token, _portal: Some(session) })
        }
        #[cfg(windows)]
        Source::Monitor { index } => Ok(Capture {
            video_desc: format!(
                "d3d11screencapturesrc monitor-index={index} show-cursor=true ! video/x-raw(memory:D3D11Memory),framerate={fps}/1"
            ),
            portal_token: None,
        }),
        #[cfg(windows)]
        Source::Window { hwnd } => Ok(Capture {
            video_desc: format!(
                "d3d11screencapturesrc window-handle={hwnd} capture-api=wgc show-cursor=true ! video/x-raw(memory:D3D11Memory),framerate={fps}/1"
            ),
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

    pub fn windows() -> Vec<(Source, String)> {
        let mut v: Vec<(Source, String)> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut v as *mut _ as isize));
        }
        v
    }
}
