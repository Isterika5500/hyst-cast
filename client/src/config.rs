//! Настройки: файл config.toml в папке настроек пользователя.
//! Адрес сервера, отпечаток и пароль берутся из `server.toml` рядом с exe (кладётся в zip для друзей)
//! или вшиваются при локальной сборке (HYST_CAST_SERVER / _FP / _TOKEN). Друзьям достаточно
//! ввести имя и комнату. В публичном CI секретов нет — server.toml добавляется уже после сборки.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    pub key: &'static str,
    pub label: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Целевой битрейт видео, кбит/с.
    pub kbps: u32,
}

pub const PRESETS: &[Preset] = &[
    Preset { key: "720p30", label: "720p · 30 fps", width: 1280, height: 720, fps: 30, kbps: 3_000 },
    Preset { key: "720p60", label: "720p · 60 fps", width: 1280, height: 720, fps: 60, kbps: 4_500 },
    Preset { key: "1080p30", label: "1080p · 30 fps", width: 1920, height: 1080, fps: 30, kbps: 6_000 },
    Preset { key: "1080p60", label: "1080p · 60 fps", width: 1920, height: 1080, fps: 60, kbps: 9_000 },
    Preset { key: "1440p60", label: "1440p · 60 fps", width: 2560, height: 1440, fps: 60, kbps: 14_000 },
];

pub fn preset(key: &str) -> Preset {
    *PRESETS.iter().find(|p| p.key == key).unwrap_or(&PRESETS[2])
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub server: String,
    pub fingerprint: String,
    pub token: String,
    pub name: String,
    pub room: String,
    pub preset: String,
    pub audio: bool,
    /// Буфер у зрителя: больше — плавнее на плохой сети, меньше — меньше задержка.
    pub latency_ms: u32,
    /// Принудительно выбрать кодер (имя элемента GStreamer), пусто — автоматически.
    pub encoder: String,
    /// Linux: токен портала, чтобы не выбирать экран каждый раз.
    pub portal_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: option_env!("HYST_CAST_SERVER").unwrap_or("").into(),
            fingerprint: option_env!("HYST_CAST_FP").unwrap_or("").into(),
            token: option_env!("HYST_CAST_TOKEN").unwrap_or("").into(),
            name: String::new(),
            room: String::new(),
            preset: "1080p30".into(),
            audio: true,
            latency_ms: 200,
            encoder: String::new(),
            portal_token: None,
        }
    }
}

/// Настройки сервера, которые лежат рядом с exe.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ServerFile {
    server: String,
    fingerprint: String,
    token: String,
}

fn server_file() -> ServerFile {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("server.toml")))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("hyst-cast")
}

impl Config {
    pub fn load() -> Config {
        let mut c: Config = std::fs::read_to_string(dir().join("config.toml"))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        // Пустые поля сервера — из server.toml рядом с exe, иначе вшитые при сборке
        let mut d = Config::default();
        let f = server_file();
        if !f.server.is_empty() {
            d.server = f.server;
            d.fingerprint = f.fingerprint;
            d.token = f.token;
        }
        if c.server.is_empty() {
            c.server = d.server;
        }
        if c.fingerprint.is_empty() {
            c.fingerprint = d.fingerprint;
        }
        if c.token.is_empty() {
            c.token = d.token;
        }
        c
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(dir());
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(dir().join("config.toml"), s);
        }
    }
}
