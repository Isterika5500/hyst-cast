//! Окно приложения (egui).

use crate::capture::{self, Source};
use crate::config::{self, Config, PRESETS};
use crate::net::{Cmd, SharedRef, Status};
use eframe::egui;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

pub struct App {
    cfg: Config,
    shared: SharedRef,
    tx: UnboundedSender<Cmd>,
    sources: Vec<(Source, String)>,
    source_idx: usize,
    volumes: HashMap<u32, f32>,
    show_server: bool,
}

const RED: egui::Color32 = egui::Color32::from_rgb(229, 72, 77);
const GREEN: egui::Color32 = egui::Color32::from_rgb(61, 214, 140);
const DIM: egui::Color32 = egui::Color32::from_rgb(139, 143, 152);

impl App {
    pub fn new(shared: SharedRef, tx: UnboundedSender<Cmd>) -> App {
        let cfg = Config::load();
        let show_server = cfg.server.is_empty() || cfg.fingerprint.is_empty() || cfg.token.is_empty();
        App { cfg, shared, tx, sources: capture::list_sources(), source_idx: 0, volumes: HashMap::new(), show_server }
    }

    fn connect(&mut self) {
        self.cfg.name = self.cfg.name.trim().to_string();
        self.cfg.room = self.cfg.room.trim().to_lowercase();
        if self.cfg.name.is_empty() || self.cfg.room.is_empty() {
            return;
        }
        self.cfg.save();
        let _ = self.tx.send(Cmd::Connect(self.cfg.clone()));
    }

    fn login(&mut self, ui: &mut egui::Ui, status: &Status) {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.heading("hyst-cast");
            ui.label(egui::RichText::new("стрим экрана для своих").color(DIM));
        });
        ui.add_space(16.0);
        let mut submit = false;
        egui::Grid::new("login").num_columns(2).spacing([8.0, 8.0]).show(ui, |ui| {
            ui.label("Имя");
            let r = ui.add(egui::TextEdit::singleline(&mut self.cfg.name).hint_text("как тебя видят").desired_width(f32::INFINITY));
            submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.end_row();
            ui.label("Комната");
            let r = ui.add(egui::TextEdit::singleline(&mut self.cfg.room).hint_text("одна на всех друзей").desired_width(f32::INFINITY));
            submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.end_row();
        });
        ui.add_space(8.0);
        let connecting = *status == Status::Connecting;
        let ready = !self.cfg.name.trim().is_empty() && !self.cfg.room.trim().is_empty();
        ui.add_enabled_ui(!connecting && ready, |ui| {
            if ui.add_sized([ui.available_width(), 32.0], egui::Button::new(if connecting { "Подключение…" } else { "Войти" })).clicked() {
                submit = true;
            }
        });
        if submit && !connecting {
            self.connect();
        }
        if let Status::Error(e) = status {
            ui.add_space(6.0);
            ui.colored_label(RED, e);
        }
        ui.add_space(12.0);
        ui.collapsing("Сервер", |ui| {
            egui::Grid::new("srv").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Адрес");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.server).hint_text("host:4433"));
                ui.end_row();
                ui.label("Отпечаток");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.fingerprint).hint_text("sha256 hex"));
                ui.end_row();
                ui.label("Пароль");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.token).password(true));
                ui.end_row();
                ui.label("Буфер, мс");
                ui.add(egui::Slider::new(&mut self.cfg.latency_ms, 80..=1000));
                ui.end_row();
            });
        });
        let _ = self.show_server;
    }

    fn room(&mut self, ui: &mut egui::Ui) {
        let (peers, publishing, starting, notice, watch) = {
            let s = self.shared.lock().unwrap();
            (s.peers.clone(), s.publishing.clone(), s.starting, s.notice.clone(), s.watch.clone())
        };

        ui.add_space(4.0);
        ui.label(egui::RichText::new(format!("Комната «{}»", self.cfg.room)).strong());
        ui.add_space(4.0);
        if peers.is_empty() {
            ui.label(egui::RichText::new("Пока никого. Скинь друзьям название комнаты.").color(DIM));
        }
        for (id, p) in &peers {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&p.name).strong());
                    if let Some(s) = &p.stream {
                        ui.colored_label(RED, "● стримит");
                        ui.label(egui::RichText::new(format!("{}p{}", s.height, s.fps)).color(DIM));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if p.watching {
                                if ui.button("Закрыть").clicked() {
                                    let _ = self.tx.send(Cmd::Unwatch(*id));
                                }
                            } else if ui.button("▶ Смотреть").clicked() {
                                let _ = self.tx.send(Cmd::Watch(*id));
                            }
                        });
                    }
                });
                if p.watching {
                    if p.stream.as_ref().is_some_and(|s| s.has_audio) {
                        let vol = self.volumes.entry(*id).or_insert(1.0);
                        ui.horizontal(|ui| {
                            ui.label("🔊");
                            if ui.add(egui::Slider::new(vol, 0.0..=1.5).show_value(false)).changed() {
                                let _ = self.tx.send(Cmd::Volume(*id, *vol as f64));
                            }
                        });
                    }
                    if !p.stats.is_empty() {
                        ui.label(egui::RichText::new(&p.stats).small().color(DIM));
                    }
                    if let Some(w) = watch.get(id) {
                        if w.received > 0 && w.rendered == 0 {
                            ui.label(egui::RichText::new("данные идут, ждём первый кадр…").small().color(DIM));
                        }
                    }
                }
            });
        }

        ui.add_space(10.0);
        ui.separator();
        ui.label(egui::RichText::new("Мой стрим").strong());
        ui.add_space(4.0);
        if let Some(pv) = publishing {
            ui.horizontal(|ui| {
                ui.colored_label(RED, "● в эфире");
                if let Some(i) = &pv.info {
                    ui.label(format!("{}×{} · {} fps{}", i.width, i.height, i.fps, if i.has_audio { " · со звуком" } else { " · без звука" }));
                }
            });
            ui.label(egui::RichText::new(&pv.stats).small().color(DIM));
            ui.label(if pv.viewers.is_empty() { "Пока никто не смотрит".to_string() } else { format!("Смотрят: {}", pv.viewers.join(", ")) });
            ui.add_space(6.0);
            if ui.add_sized([ui.available_width(), 30.0], egui::Button::new("■ Остановить стрим")).clicked() {
                let _ = self.tx.send(Cmd::Stop);
            }
        } else {
            egui::Grid::new("pub").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Что");
                ui.horizontal(|ui| {
                    if self.source_idx >= self.sources.len() {
                        self.source_idx = 0;
                    }
                    egui::ComboBox::from_id_salt("src")
                        .width(ui.available_width() - 34.0)
                        .selected_text(self.sources.get(self.source_idx).map(|s| s.1.as_str()).unwrap_or("—"))
                        .show_ui(ui, |ui| {
                            for (i, (_, label)) in self.sources.iter().enumerate() {
                                ui.selectable_value(&mut self.source_idx, i, label);
                            }
                        });
                    if ui.button("⟳").on_hover_text("Обновить список окон").clicked() {
                        self.sources = capture::list_sources();
                    }
                });
                ui.end_row();
                ui.label("Качество");
                egui::ComboBox::from_id_salt("q").selected_text(config::preset(&self.cfg.preset).label).show_ui(ui, |ui| {
                    for p in PRESETS {
                        ui.selectable_value(&mut self.cfg.preset, p.key.to_string(), p.label);
                    }
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.cfg.audio, "Звук системы");
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.add_enabled_ui(!starting, |ui| {
                let label = if starting { "Запуск… (выбери экран, если спросят)" } else { "● Начать стрим" };
                if ui.add_sized([ui.available_width(), 32.0], egui::Button::new(label)).clicked() {
                    if let Some((src, _)) = self.sources.get(self.source_idx).cloned() {
                        self.cfg.save();
                        let _ = self.tx.send(Cmd::Start {
                            source: src,
                            preset: config::preset(&self.cfg.preset),
                            audio: self.cfg.audio,
                            encoder: self.cfg.encoder.clone(),
                            portal_token: self.cfg.portal_token.clone(),
                        });
                    }
                }
            });
        }
        if let Some(n) = notice {
            ui.add_space(8.0);
            ui.colored_label(RED, n);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (status, rtt, token) = {
            let mut s = self.shared.lock().unwrap();
            (s.status.clone(), s.rtt_ms, s.portal_token.take())
        };
        if let Some(t) = token {
            self.cfg.portal_token = Some(t);
            self.cfg.save();
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("hyst-cast").strong());
                match &status {
                    Status::Online => {
                        ui.colored_label(GREEN, "● в сети");
                        if let Some(r) = rtt {
                            ui.label(egui::RichText::new(format!("{r} мс")).color(DIM));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Выйти").clicked() {
                                let _ = self.tx.send(Cmd::Disconnect);
                            }
                            ui.label(egui::RichText::new(&self.cfg.name).color(DIM));
                        });
                    }
                    Status::Connecting => {
                        ui.colored_label(DIM, "подключение…");
                    }
                    Status::Error(_) => {
                        ui.colored_label(RED, "нет связи");
                    }
                    Status::Offline => {}
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if status == Status::Online {
                    self.room(ui);
                } else {
                    self.login(ui, &status);
                }
            });
        });
    }
}
