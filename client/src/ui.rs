//! Окно приложения (egui): тёмная тема в духе macOS, раскладка как у Discord —
//! слева комната и участники, справа «сцена» с эфирами, снизу панель своего стрима.

use crate::capture::{self, Source};
use crate::config::{self, Config};
use crate::net::{Cmd, SharedRef, Status};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Layout, Margin, Pos2, Rect, RichText, Sense,
    Stroke, Vec2,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

// ------------------------------------------------------------------ палитра

const BG: Color32 = Color32::from_rgb(0x16, 0x16, 0x18);
const SIDEBAR: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x21);
const SURFACE: Color32 = Color32::from_rgb(0x26, 0x26, 0x2A);
const SURFACE_HI: Color32 = Color32::from_rgb(0x30, 0x30, 0x35);
const FIELD: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1D);
const TEXT: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF7);
const TEXT2: Color32 = Color32::from_rgb(0xA1, 0xA1, 0xAA);
const TEXT3: Color32 = Color32::from_rgb(0x6E, 0x6E, 0x76);
const ACCENT: Color32 = Color32::from_rgb(0x63, 0x66, 0xF1);
const ACCENT_HI: Color32 = Color32::from_rgb(0x7C, 0x7F, 0xF5);
const LIVE: Color32 = Color32::from_rgb(0xFF, 0x45, 0x3A);
const GREEN: Color32 = Color32::from_rgb(0x30, 0xD1, 0x58);
const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD6, 0x0A);

fn hairline() -> Stroke {
    Stroke::new(1.0, Color32::from_white_alpha(16))
}

fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}
fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// Цвет аватарки по имени — стабильный и приятный (оттенки «системных» цветов).
fn name_color(name: &str) -> Color32 {
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(0x0A, 0x84, 0xFF),
        Color32::from_rgb(0x5E, 0x5C, 0xE6),
        Color32::from_rgb(0xBF, 0x5A, 0xF2),
        Color32::from_rgb(0xFF, 0x37, 0x5F),
        Color32::from_rgb(0xFF, 0x9F, 0x0A),
        Color32::from_rgb(0x30, 0xD1, 0x58),
        Color32::from_rgb(0x64, 0xD2, 0xFF),
        Color32::from_rgb(0xAC, 0x8E, 0x68),
    ];
    let h = name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    PALETTE[(h % PALETTE.len() as u32) as usize]
}

fn initials(name: &str) -> String {
    let mut it = name.split_whitespace().filter_map(|w| w.chars().next());
    let a = it.next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
    match it.next() {
        Some(b) => format!("{a}{}", b.to_uppercase()),
        None => a,
    }
}

fn darken(c: Color32, k: f32) -> Color32 {
    Color32::from_rgb((c.r() as f32 * k) as u8, (c.g() as f32 * k) as u8, (c.b() as f32 * k) as u8)
}

// ------------------------------------------------------------------ тема и шрифты

pub fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (key, data) in [
        ("inter", &include_bytes!("../assets/Inter-400.ttf")[..]),
        ("inter-semibold", &include_bytes!("../assets/Inter-600.ttf")[..]),
        ("inter-bold", &include_bytes!("../assets/Inter-700.ttf")[..]),
    ] {
        fonts.font_data.insert(key.into(), Arc::new(egui::FontData::from_static(data)));
    }
    // Inter первым, встроенные шрифты egui — запасные (эмодзи и символы)
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    fonts.families.insert(FontFamily::Proportional, [vec!["inter".to_string()], fallback.clone()].concat());
    fonts.families.insert(semibold(), [vec!["inter-semibold".to_string()], fallback.clone()].concat());
    fonts.families.insert(bold(), [vec!["inter-bold".to_string()], fallback].concat());
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        use egui::TextStyle::*;
        style.text_styles = [
            (Heading, FontId::new(22.0, bold())),
            (Body, FontId::new(14.0, FontFamily::Proportional)),
            (Button, FontId::new(14.0, semibold())),
            (Small, FontId::new(12.0, FontFamily::Proportional)),
            (Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(14.0, 8.0);
        style.spacing.interact_size = Vec2::new(40.0, 32.0);
        style.spacing.combo_width = 240.0;
        style.spacing.slider_rail_height = 4.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(TEXT);
        v.weak_text_color = Some(TEXT2);
        v.panel_fill = BG;
        v.window_fill = SURFACE;
        v.window_stroke = hairline();
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.extreme_bg_color = FIELD;
        v.text_edit_bg_color = Some(FIELD);
        v.faint_bg_color = SURFACE;
        v.code_bg_color = FIELD;
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, TEXT);
        v.hyperlink_color = ACCENT_HI;
        v.error_fg_color = LIVE;
        v.warn_fg_color = YELLOW;
        v.slider_trailing_fill = true;
        v.collapsing_header_frame = false;
        v.indent_has_left_vline = false;
        v.window_shadow = egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(120) };
        v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(110) };

        let r = CornerRadius::same(8);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = SURFACE;
        w.noninteractive.weak_bg_fill = SURFACE;
        w.noninteractive.bg_stroke = hairline();
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT2);
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = SURFACE_HI;
        w.inactive.weak_bg_fill = SURFACE_HI;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;
        w.hovered.bg_fill = Color32::from_rgb(0x3A, 0x3A, 0x40);
        w.hovered.weak_bg_fill = Color32::from_rgb(0x3A, 0x3A, 0x40);
        w.hovered.bg_stroke = Stroke::NONE;
        w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = Color32::from_rgb(0x44, 0x44, 0x4B);
        w.active.weak_bg_fill = Color32::from_rgb(0x44, 0x44, 0x4B);
        w.active.bg_stroke = Stroke::NONE;
        w.active.fg_stroke = Stroke::new(1.5, TEXT);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;
        w.open = w.active.clone();
    });
}

// ------------------------------------------------------------------ свои элементы

fn avatar(ui: &mut egui::Ui, name: &str, size: f32, live: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    let col = name_color(name);
    if live {
        p.circle_stroke(c, size / 2.0 - 1.0, Stroke::new(2.0, LIVE));
        p.circle_filled(c, size / 2.0 - 4.0, col);
    } else {
        p.circle_filled(c, size / 2.0, col);
    }
    p.text(c, Align2::CENTER_CENTER, initials(name), FontId::new(size * 0.38, bold()), Color32::WHITE);
    resp
}

/// Небольшая плашка: «LIVE», «1080p60», «GPU».
fn pill(ui: &mut egui::Ui, text: &str, fill: Color32, fg: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::new(11.0, bold()), fg);
    let size = galley.size() + Vec2::new(14.0, 6.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
}

fn paint_pill(p: &egui::Painter, at: Pos2, text: &str, fill: Color32, fg: Color32) -> Rect {
    let galley = p.layout_no_wrap(text.to_string(), FontId::new(11.0, bold()), fg);
    let rect = Rect::from_min_size(at, galley.size() + Vec2::new(14.0, 6.0));
    p.rect_filled(rect, CornerRadius::same(6), fill);
    p.galley(rect.center() - galley.size() / 2.0, galley, fg);
    rect
}

/// Тумблер в стиле macOS.
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = Vec2::new(40.0, 24.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let p = ui.painter();
    let bg = egui::ecolor::tint_color_towards(SURFACE_HI, ACCENT).lerp_to_gamma(ACCENT, t);
    p.rect_filled(rect, CornerRadius::same(12), if *on { bg } else { SURFACE_HI.lerp_to_gamma(ACCENT, t) });
    let x = egui::lerp((rect.left() + 12.0)..=(rect.right() - 12.0), t);
    p.circle_filled(Pos2::new(x, rect.center().y), 9.5, Color32::WHITE);
    resp
}

/// Сегментный переключатель (как в настройках macOS).
fn segmented(ui: &mut egui::Ui, value: &mut String, options: &[(&str, &str)]) -> bool {
    let mut changed = false;
    Frame::new().fill(FIELD).corner_radius(CornerRadius::same(9)).inner_margin(Margin::same(3)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.horizontal(|ui| {
            for (key, label) in options {
                let selected = value == key;
                let galley = ui.painter().layout_no_wrap(label.to_string(), FontId::new(13.0, semibold()), TEXT);
                let size = Vec2::new(galley.size().x + 20.0, 26.0);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                let fill = if selected {
                    Color32::from_rgb(0x48, 0x48, 0x50)
                } else if resp.hovered() {
                    SURFACE
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(rect, CornerRadius::same(7), fill);
                let color = if selected { TEXT } else { TEXT2 };
                let galley = ui.painter().layout_no_wrap(label.to_string(), FontId::new(13.0, semibold()), color);
                ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
                if resp.clicked() && !selected {
                    *value = key.to_string();
                    changed = true;
                }
            }
        });
    });
    changed
}

/// Большая кнопка с заливкой.
fn big_button(ui: &mut egui::Ui, text: &str, fill: Color32, width: f32, enabled: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 40.0), if enabled { Sense::click() } else { Sense::hover() });
    let hover = enabled && resp.hovered();
    let fill = if !enabled {
        darken(fill, 0.55)
    } else if resp.is_pointer_button_down_on() {
        darken(fill, 0.85)
    } else if hover {
        fill.lerp_to_gamma(Color32::WHITE, 0.12)
    } else {
        fill
    };
    ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, FontId::new(14.5, semibold()), if enabled { Color32::WHITE } else { TEXT2 });
    if hover {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    enabled && resp.clicked()
}

fn small_button(ui: &mut egui::Ui, text: &str, fill: Color32) -> bool {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::new(13.0, semibold()), Color32::WHITE);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + Vec2::new(24.0, 14.0), Sense::click());
    let fill = if resp.hovered() { fill.lerp_to_gamma(Color32::WHITE, 0.12) } else { fill };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// Кнопка с нарисованной иконкой «обновить» (в шрифтах нужного символа нет).
fn refresh_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    let fill = if resp.hovered() { Color32::from_rgb(0x3A, 0x3A, 0x40) } else { SURFACE_HI };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    let c = rect.center();
    let r = 7.0;
    let pts: Vec<Pos2> = (0..=24)
        .map(|i| {
            let a = -0.4 + i as f32 / 24.0 * 5.0;
            c + Vec2::new(a.cos(), a.sin()) * r
        })
        .collect();
    let stroke = Stroke::new(1.8, TEXT);
    ui.painter().add(egui::Shape::line(pts.clone(), stroke));
    let end = *pts.last().unwrap();
    let a = -0.4 + 5.0_f32;
    let tangent = Vec2::new(-a.sin(), a.cos());
    let normal = Vec2::new(a.cos(), a.sin());
    ui.painter().add(egui::Shape::convex_polygon(
        vec![end + tangent * 4.0, end - normal * 3.5, end + normal * 3.5],
        TEXT,
        Stroke::NONE,
    ));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).font(FontId::new(11.0, bold())).color(TEXT3));
}

/// Поле ввода с подписью сверху.
fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str, password: bool) -> egui::Response {
    ui.label(RichText::new(label).font(FontId::new(12.5, semibold())).color(TEXT2));
    ui.add_space(-2.0);
    let r = ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(RichText::new(hint).color(TEXT3))
            .password(password)
            .margin(Margin::symmetric(12, 10))
            .font(FontId::new(15.0, FontFamily::Proportional))
            .desired_width(f32::INFINITY),
    );
    ui.add_space(6.0);
    r
}

/// Логотип: скруглённый квадрат с градиентом и треугольником «эфир».
fn logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    gradient_rect(ui.painter(), rect, CornerRadius::same((size * 0.26) as u8), Color32::from_rgb(0x7C, 0x7F, 0xF5), Color32::from_rgb(0xBF, 0x5A, 0xF2));
    let c = rect.center();
    let s = size * 0.2;
    ui.painter().add(egui::Shape::convex_polygon(
        vec![Pos2::new(c.x - s * 0.7, c.y - s), Pos2::new(c.x - s * 0.7, c.y + s), Pos2::new(c.x + s, c.y)],
        Color32::WHITE,
        Stroke::NONE,
    ));
}

/// Диагональный градиент: рисуем сеткой полосок (скругление — подложкой того же цвета по краям).
fn gradient_rect(p: &egui::Painter, rect: Rect, r: CornerRadius, a: Color32, b: Color32) {
    p.rect_filled(rect, r, a.lerp_to_gamma(b, 0.5));
    let mut mesh = egui::Mesh::default();
    let inner = rect.shrink(r.nw as f32 * 0.3);
    mesh.colored_vertex(inner.left_top(), a);
    mesh.colored_vertex(inner.right_top(), a.lerp_to_gamma(b, 0.5));
    mesh.colored_vertex(inner.right_bottom(), b);
    mesh.colored_vertex(inner.left_bottom(), a.lerp_to_gamma(b, 0.5));
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(egui::Shape::mesh(mesh));
}

// ------------------------------------------------------------------ видео

/// Обновить текстуру стрима из последнего кадра (если пришёл новый) и вернуть её.
fn video_texture(ctx: &egui::Context, textures: &mut HashMap<u32, (egui::TextureHandle, u64)>, frames: &HashMap<u32, Arc<crate::watch::FrameSlot>>, id: u32) -> Option<egui::TextureHandle> {
    let slot = frames.get(&id)?;
    let seq = slot.seq.load(std::sync::atomic::Ordering::Relaxed);
    let have = textures.get(&id).map(|t| t.1);
    if have != Some(seq) {
        if let Some(f) = slot.frame.lock().unwrap().take() {
            let img = egui::ColorImage::from_rgba_premultiplied([f.w as usize, f.h as usize], &f.rgba);
            match textures.get_mut(&id) {
                Some((tex, s)) => {
                    tex.set(img, egui::TextureOptions::LINEAR);
                    *s = seq;
                }
                None => {
                    let tex = ctx.load_texture(format!("stream-{id}"), img, egui::TextureOptions::LINEAR);
                    textures.insert(id, (tex, seq));
                }
            }
        }
    }
    textures.get(&id).map(|t| t.0.clone())
}

/// Нарисовать текстуру в прямоугольнике, сохранив пропорции (по краям — чёрные поля).
fn paint_fit(p: &egui::Painter, rect: Rect, tex: &egui::TextureHandle) {
    let ts = tex.size_vec2();
    let k = (rect.width() / ts.x).min(rect.height() / ts.y);
    let r = Rect::from_center_size(rect.center(), ts * k);
    p.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
}

// ------------------------------------------------------------------ приложение

pub struct App {
    cfg: Config,
    shared: SharedRef,
    tx: UnboundedSender<Cmd>,
    sources: Vec<(Source, String)>,
    source_idx: usize,
    volumes: HashMap<u32, f32>,
    show_server: bool,
    notice_dismissed: Option<String>,
    /// Текстуры видео по id стримера: (текстура, номер последнего загруженного кадра).
    textures: HashMap<u32, (egui::TextureHandle, u64)>,
    /// Стрим, открытый крупно (как «фокус» в Discord).
    focus: Option<u32>,
    /// Когда включили фокус: пару секунд не сбрасываем его, пока контроллер не отметил просмотр.
    focus_at: std::time::Instant,
    fullscreen: bool,
    /// Последний отправленный размер видео по id.
    sent_size: HashMap<u32, (u32, u32)>,
}

const QUALITY: &[(&str, &str)] = &[("720p30", "720p"), ("720p60", "720p60"), ("1080p30", "1080p"), ("1080p60", "1080p60"), ("1440p60", "1440p")];

impl App {
    pub fn new(shared: SharedRef, tx: UnboundedSender<Cmd>) -> App {
        let cfg = Config::load();
        let show_server = cfg.server.is_empty() || cfg.fingerprint.is_empty() || cfg.token.is_empty();
        let auto = cfg.auto_join && !cfg.name.trim().is_empty() && !cfg.room.trim().is_empty() && !show_server;
        if auto {
            let _ = tx.send(Cmd::Connect(cfg.clone()));
        }
        App {
            cfg,
            shared,
            tx,
            sources: capture::list_sources(),
            source_idx: 0,
            volumes: HashMap::new(),
            show_server,
            notice_dismissed: None,
            textures: HashMap::new(),
            focus: None,
            focus_at: std::time::Instant::now(),
            fullscreen: false,
            sent_size: HashMap::new(),
        }
    }

    fn connect(&mut self) {
        self.cfg.name = self.cfg.name.trim().to_string();
        self.cfg.room = self.cfg.room.trim().to_lowercase();
        if self.cfg.name.is_empty() || self.cfg.room.is_empty() {
            return;
        }
        self.cfg.auto_join = true;
        self.cfg.save();
        let _ = self.tx.send(Cmd::Connect(self.cfg.clone()));
    }

    // -------------------------------------------------------------- вход

    fn login(&mut self, ui: &mut egui::Ui, status: &Status) {
        let avail = ui.available_rect_before_wrap();

        let width = 380.0_f32.min(avail.width() - 32.0);
        let top = (avail.height() - 520.0).max(24.0) / 2.0;
        ui.add_space(top);
        ui.vertical_centered(|ui| {
            logo(ui, 64.0);
            ui.add_space(10.0);
            ui.label(RichText::new("hyst-cast").font(FontId::new(28.0, bold())));
            ui.label(RichText::new("Стрим экрана для своих").color(TEXT2));
            ui.add_space(22.0);
        });
        ui.vertical_centered(|ui| {
            ui.set_max_width(width);
            Frame::new()
                .fill(SURFACE)
                .stroke(hairline())
                .corner_radius(CornerRadius::same(16))
                .inner_margin(Margin::same(22))
                .shadow(egui::Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(90) })
                .show(ui, |ui| {
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        let mut submit = false;
                        let r = field(ui, "Как тебя зовут", &mut self.cfg.name, "Имя для друзей", false);
                        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let r = field(ui, "Комната", &mut self.cfg.room, "Одна на всю компанию", false);
                        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(6.0);
                        let connecting = *status == Status::Connecting;
                        let ready = !self.cfg.name.trim().is_empty() && !self.cfg.room.trim().is_empty();
                        let label = if connecting { "Подключение…" } else { "Войти" };
                        if big_button(ui, label, ACCENT, ui.available_width(), ready && !connecting) {
                            submit = true;
                        }
                        if submit && ready && !connecting {
                            self.connect();
                        }
                        if let Status::Error(e) = status {
                            ui.add_space(10.0);
                            Frame::new()
                                .fill(Color32::from_rgba_unmultiplied(0xFF, 0x45, 0x3A, 28))
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::symmetric(12, 8))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(RichText::new(e).color(Color32::from_rgb(0xFF, 0x9A, 0x93)).size(13.0));
                                });
                        }
                    });
                });
            ui.add_space(14.0);
            let label = if self.show_server { "Скрыть настройки сервера" } else { "Настройки сервера" };
            if ui.add(egui::Label::new(RichText::new(label).color(TEXT3).size(12.5)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.show_server = !self.show_server;
            }
            if self.show_server {
                ui.add_space(6.0);
                Frame::new().fill(SURFACE).stroke(hairline()).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(16)).show(ui, |ui| {
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        field(ui, "Адрес сервера", &mut self.cfg.server, "host:4433", false);
                        field(ui, "Отпечаток", &mut self.cfg.fingerprint, "sha256", false);
                        field(ui, "Пароль сервера", &mut self.cfg.token, "", true);
                        ui.label(RichText::new("Буфер зрителя, мс").font(FontId::new(12.5, semibold())).color(TEXT2));
                        ui.add(egui::Slider::new(&mut self.cfg.latency_ms, 80..=800));
                        ui.label(RichText::new("Больше — плавнее на плохом интернете, меньше — меньше задержка.").small().color(TEXT3));
                    });
                });
            }
        });
    }

    // -------------------------------------------------------------- боковая панель

    fn sidebar(&mut self, ui: &mut egui::Ui, rtt: Option<u32>) {
        let (peers, publishing) = {
            let s = self.shared.lock().unwrap();
            (s.peers.clone(), s.publishing.is_some())
        };
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            logo(ui, 28.0);
            ui.label(RichText::new("hyst-cast").font(FontId::new(17.0, bold())));
        });
        ui.add_space(14.0);
        Frame::new().fill(SURFACE).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("#").font(FontId::new(18.0, bold())).color(TEXT3));
                ui.label(RichText::new(&self.cfg.room).font(FontId::new(15.0, semibold())));
            });
        });
        ui.add_space(16.0);
        caption(ui, &format!("В комнате — {}", peers.len() + 1));
        ui.add_space(2.0);

        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(ui.available_height() - 76.0).show(ui, |ui| {
            let me_sub = if publishing { "в эфире".to_string() } else { "в сети".to_string() };
            self.member_row(ui, &format!("{} (ты)", self.cfg.name), &self.cfg.name.clone(), &me_sub, publishing, false);
            for p in peers.values() {
                let sub = match (&p.stream, p.watching) {
                    (Some(s), true) => format!("смотришь · {}p{}", s.height, s.fps),
                    (Some(s), false) => format!("в эфире · {}p{}", s.height, s.fps),
                    (None, _) => "в комнате".to_string(),
                };
                self.member_row(ui, &p.name, &p.name, &sub, p.stream.is_some(), p.watching);
            }
        });

        // Профиль снизу, как у Discord
        let bottom = Rect::from_min_max(Pos2::new(ui.max_rect().left() - 12.0, ui.max_rect().bottom() - 64.0), Pos2::new(ui.max_rect().right() + 12.0, ui.max_rect().bottom() + 12.0));
        ui.painter().rect_filled(bottom, CornerRadius::ZERO, Color32::from_rgb(0x19, 0x19, 0x1C));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bottom.shrink2(Vec2::new(14.0, 12.0))).layout(Layout::left_to_right(Align::Center)));
        avatar(&mut child, &self.cfg.name, 34.0, false);
        child.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new(&self.cfg.name).font(FontId::new(14.0, semibold())));
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, GREEN);
                let ping = rtt.map(|r| format!("в сети · {r} мс")).unwrap_or_else(|| "в сети".into());
                ui.label(RichText::new(ping).size(12.0).color(TEXT2));
            });
        });
        child.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add(egui::Button::new(RichText::new("Выйти").size(12.5).color(TEXT2)).fill(Color32::TRANSPARENT)).on_hover_text("Отключиться от комнаты").clicked() {
                self.cfg.auto_join = false;
                self.cfg.save();
                let _ = self.tx.send(Cmd::Disconnect);
            }
        });
    }

    fn member_row(&self, ui: &mut egui::Ui, title: &str, avatar_name: &str, sub: &str, live: bool, watching: bool) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::hover());
        if resp.hovered() || watching {
            ui.painter().rect_filled(rect, CornerRadius::same(8), if watching { Color32::from_rgba_unmultiplied(0x63, 0x66, 0xF1, 30) } else { SURFACE });
        }
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(8.0, 6.0))).layout(Layout::left_to_right(Align::Center)));
        avatar(&mut child, avatar_name, 32.0, live);
        child.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(title).font(FontId::new(14.0, semibold())));
            ui.label(RichText::new(sub).size(12.0).color(if live { Color32::from_rgb(0xFF, 0x8A, 0x80) } else { TEXT3 }));
        });
        if live {
            child.with_layout(Layout::right_to_left(Align::Center), |ui| pill(ui, "LIVE", LIVE, Color32::WHITE));
        }
    }

    // -------------------------------------------------------------- сцена

    fn stage(&mut self, ui: &mut egui::Ui) {
        let (peers, watch, frames) = {
            let s = self.shared.lock().unwrap();
            (s.peers.clone(), s.watch.clone(), s.frames.clone())
        };
        let live: Vec<_> = peers.iter().filter(|(_, p)| p.stream.is_some()).collect();
        // Отладка/скриншоты: HYST_CAST_AUTOWATCH=1 — сам открыть первый эфир (=card — без «крупно»)
        if let Ok(mode) = std::env::var("HYST_CAST_AUTOWATCH") {
            if let Some((id, p)) = live.first() {
                if !p.watching && self.focus.is_none() && !self.textures.contains_key(id) && !frames.contains_key(id) {
                    let _ = self.tx.send(Cmd::Watch(**id));
                    if mode != "card" {
                        self.focus = Some(**id);
                        self.focus_at = std::time::Instant::now();
                    }
                }
            }
        }
        // Фокус на стриме, который уже не идёт, — сбрасываем
        if let Some(f) = self.focus {
            let alive = peers.get(&f).is_some_and(|p| p.stream.is_some() && (p.watching || self.focus_at.elapsed().as_secs_f32() < 3.0));
            if !alive {
                self.focus = None;
            }
        }
        if let Some(f) = self.focus {
            if let Some(p) = peers.get(&f) {
                self.focused(ui, f, p, &frames);
                return;
            }
        }
        // Текстуры стримов, которые больше не смотрим, — освобождаем память
        self.textures.retain(|id, _| frames.contains_key(id));

        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Сейчас в эфире").font(FontId::new(22.0, bold())));
            if !live.is_empty() {
                ui.add_space(4.0);
                pill(ui, &live.len().to_string(), SURFACE_HI, TEXT2);
            }
        });
        ui.add_space(12.0);

        if live.is_empty() {
            let avail = ui.available_rect_before_wrap();
            ui.scope_builder(egui::UiBuilder::new().max_rect(avail), |ui| {
                ui.add_space((avail.height() / 2.0 - 90.0).max(10.0));
                ui.vertical_centered(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(72.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 36.0, SURFACE);
                    ui.painter().rect_stroke(Rect::from_center_size(r.center(), Vec2::new(32.0, 22.0)), CornerRadius::same(4), Stroke::new(2.0, TEXT3), egui::StrokeKind::Middle);
                    ui.painter().line_segment([r.center() + Vec2::new(-8.0, 16.0), r.center() + Vec2::new(8.0, 16.0)], Stroke::new(2.0, TEXT3));
                    ui.add_space(12.0);
                    ui.label(RichText::new("Пока никто не стримит").font(FontId::new(17.0, semibold())));
                    ui.label(RichText::new("Начни свой стрим внизу или подожди друзей").color(TEXT2));
                });
            });
            return;
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let gap = 14.0;
            let cols = if live.len() > 1 && ui.available_width() > 700.0 { 2 } else { 1 };
            let w = ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).min(if cols == 1 { 780.0 } else { 10_000.0 });
            for row in live.chunks(cols) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (id, p) in row {
                        ui.allocate_ui(Vec2::new(w, 0.0), |ui| self.stream_card(ui, **id, p, watch.get(id), w, &frames));
                    }
                });
                ui.add_space(gap);
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_card(&mut self, ui: &mut egui::Ui, id: u32, p: &crate::net::PeerView, w: Option<&crate::net::WatchStats>, width: f32, frames: &HashMap<u32, Arc<crate::watch::FrameSlot>>) {
        let info = p.stream.clone().unwrap();
        Frame::new().fill(SURFACE).stroke(hairline()).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(0)).show(ui, |ui| {
            ui.set_width(width);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
            // Превью 16:9: градиент цвета стримера, большая аватарка, плашки
            let ph = width * 9.0 / 16.0;
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ph), Sense::click());
            let col = name_color(&p.name);
            let tex = if p.watching { video_texture(ui.ctx(), &mut self.textures, frames, id) } else { None };
            if let Some(tex) = &tex {
                ui.painter().rect_filled(rect, CornerRadius { nw: 14, ne: 14, sw: 0, se: 0 }, Color32::BLACK);
                paint_fit(ui.painter(), rect, tex);
                self.want_size(ui.ctx(), id, rect, p);
            } else {
                gradient_rect(ui.painter(), rect, CornerRadius { nw: 14, ne: 14, sw: 0, se: 0 }, darken(col, 0.45), darken(col, 0.18));
                let c = rect.center();
                ui.painter().circle_filled(c, 36.0, col);
                ui.painter().text(c, Align2::CENTER_CENTER, initials(&p.name), FontId::new(26.0, bold()), Color32::WHITE);
                if p.watching {
                    ui.painter().text(c + Vec2::new(0.0, 54.0), Align2::CENTER_CENTER, "подключение…", FontId::new(12.5, FontFamily::Proportional), Color32::from_white_alpha(170));
                }
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                if tex.is_some() {
                    ui.painter().text(rect.center_bottom() - Vec2::new(0.0, 18.0), Align2::CENTER_CENTER, "нажми, чтобы открыть крупно", FontId::new(12.0, semibold()), Color32::from_white_alpha(200));
                }
            }
            if resp.clicked() {
                if p.watching {
                    self.focus = Some(id);
                    self.focus_at = std::time::Instant::now();
                } else {
                    let _ = self.tx.send(Cmd::Watch(id));
                    self.focus = Some(id);
                    self.focus_at = std::time::Instant::now();
                }
            }
            paint_pill(ui.painter(), rect.left_top() + Vec2::new(12.0, 12.0), "● LIVE", LIVE, Color32::WHITE);
            let q = format!("{}p · {} fps", info.height, info.fps);
            let qg = ui.painter().layout_no_wrap(q.clone(), FontId::new(11.0, bold()), Color32::WHITE);
            paint_pill(ui.painter(), rect.right_top() + Vec2::new(-12.0 - qg.size().x - 14.0, 12.0), &q, Color32::from_black_alpha(140), Color32::WHITE);

            Frame::new().inner_margin(Margin::same(14)).show(ui, |ui| {
                ui.set_width(width - 28.0);
                ui.horizontal(|ui| {
                    avatar(ui, &p.name, 30.0, false);
                    let text_w = (ui.available_width() - 140.0).max(60.0);
                    ui.allocate_ui_with_layout(Vec2::new(text_w, 38.0), Layout::top_down(Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add(egui::Label::new(RichText::new(&p.name).font(FontId::new(15.0, semibold()))).truncate());
                        let enc = if info.encoder.contains("GPU") { "видеокарта" } else { "процессор" };
                        ui.add(egui::Label::new(RichText::new(format!("{}×{} · кодирует {enc}{}", info.width, info.height, if info.has_audio { " · со звуком" } else { "" })).size(12.0).color(TEXT3)).truncate());
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if p.watching {
                            if small_button(ui, "Закрыть", SURFACE_HI) {
                                let _ = self.tx.send(Cmd::Unwatch(id));
                            }
                            if small_button(ui, "Крупно", SURFACE_HI) {
                                self.focus = Some(id);
                    self.focus_at = std::time::Instant::now();
                            }
                        } else if small_button(ui, "▶  Смотреть", ACCENT) {
                            let _ = self.tx.send(Cmd::Watch(id));
                            self.focus = Some(id);
                    self.focus_at = std::time::Instant::now();
                        }
                    });
                });
                if p.watching {
                    ui.add_space(6.0);
                    if info.has_audio {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Громкость").size(12.5).color(TEXT2));
                            let vol = self.volumes.entry(id).or_insert(1.0);
                            ui.spacing_mut().slider_width = (width - 150.0).max(80.0);
                            if ui.add(egui::Slider::new(vol, 0.0..=1.5).show_value(false)).changed() {
                                let _ = self.tx.send(Cmd::Volume(id, *vol as f64));
                            }
                        });
                    }
                    match w {
                        Some(w) if w.received > 0 && w.rendered == 0 => {
                            ui.label(RichText::new("Данные идут, ждём первый кадр…").size(12.0).color(TEXT3));
                        }
                        _ if !p.stats.is_empty() => {
                            ui.label(RichText::new(&p.stats).size(12.0).color(TEXT3));
                        }
                        _ => {}
                    }
                }
            });
            });
        });
    }

    // -------------------------------------------------------------- стрим крупно

    fn focused(&mut self, ui: &mut egui::Ui, id: u32, p: &crate::net::PeerView, frames: &HashMap<u32, Arc<crate::watch::FrameSlot>>) {
        if !self.fullscreen {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if small_button(ui, "←  Все эфиры", SURFACE_HI) {
                    self.focus = None;
                }
                ui.add_space(6.0);
                avatar(ui, &p.name, 26.0, true);
                ui.label(RichText::new(&p.name).font(FontId::new(16.0, semibold())));
                pill(ui, "LIVE", LIVE, Color32::WHITE);
                if let Some(s) = &p.stream {
                    ui.label(RichText::new(format!("{}p · {} fps", s.height, s.fps)).size(12.5).color(TEXT3));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(&p.stats).size(12.0).color(TEXT3));
                });
            });
            ui.add_space(10.0);
        }
        let rect = ui.available_rect_before_wrap();
        let rect = if self.fullscreen { rect } else { rect.shrink2(Vec2::new(0.0, 0.0)) };
        let resp = ui.allocate_rect(rect, Sense::click());
        let r = if self.fullscreen { CornerRadius::ZERO } else { CornerRadius::same(14) };
        ui.painter().rect_filled(rect, r, Color32::BLACK);
        match video_texture(ui.ctx(), &mut self.textures, frames, id) {
            Some(tex) => {
                paint_fit(ui.painter(), rect, &tex);
                self.want_size(ui.ctx(), id, rect, p);
            }
            None => {
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, "подключение…", FontId::new(14.0, semibold()), TEXT3);
            }
        }
        if resp.double_clicked() {
            self.set_fullscreen(ui.ctx(), !self.fullscreen);
        }
        // Полоса управления — при наведении мыши, поверх видео снизу
        let hover = ui.rect_contains_pointer(rect);
        let recently_moved = ui.input(|i| i.pointer.time_since_last_movement() < 2.5);
        if hover && recently_moved {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            let bar = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - 64.0), rect.right_bottom());
            let mut mesh = egui::Mesh::default();
            mesh.colored_vertex(bar.left_top(), Color32::TRANSPARENT);
            mesh.colored_vertex(bar.right_top(), Color32::TRANSPARENT);
            mesh.colored_vertex(bar.right_bottom(), Color32::from_black_alpha(190));
            mesh.colored_vertex(bar.left_bottom(), Color32::from_black_alpha(190));
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(0, 2, 3);
            ui.painter().add(egui::Shape::mesh(mesh));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(Vec2::new(18.0, 14.0))).layout(Layout::left_to_right(Align::Max)));
            let has_audio = p.stream.as_ref().is_some_and(|s| s.has_audio);
            if has_audio {
                child.label(RichText::new("Громкость").size(12.5).color(Color32::from_white_alpha(220)));
                let vol = self.volumes.entry(id).or_insert(1.0);
                child.spacing_mut().slider_width = 140.0;
                if child.add(egui::Slider::new(vol, 0.0..=1.5).show_value(false)).changed() {
                    let _ = self.tx.send(Cmd::Volume(id, *vol as f64));
                }
            }
            child.with_layout(Layout::right_to_left(Align::Max), |ui| {
                if small_button(ui, "Закрыть", Color32::from_rgba_unmultiplied(0x30, 0x30, 0x35, 230)) {
                    let _ = self.tx.send(Cmd::Unwatch(id));
                    self.focus = None;
                    self.set_fullscreen(ui.ctx(), false);
                }
                let label = if self.fullscreen { "Выйти из полноэкранного" } else { "На весь экран" };
                if small_button(ui, label, Color32::from_rgba_unmultiplied(0x30, 0x30, 0x35, 230)) {
                    let fs = !self.fullscreen;
                    self.set_fullscreen(ui.ctx(), fs);
                }
            });
        }
    }

    /// Сообщить зрителю, сколько пикселей реально занимает видео (с шагом 64, не больше исходника и 1080p).
    fn want_size(&mut self, ctx: &egui::Context, id: u32, rect: Rect, p: &crate::net::PeerView) {
        let Some(s) = &p.stream else { return };
        let (sw, sh) = crate::watch::display_size(s.width, s.height);
        let ppp = ctx.pixels_per_point();
        let k = ((rect.width() * ppp) / sw as f32).min((rect.height() * ppp) / sh as f32).min(1.0);
        let w = (((sw as f32 * k) / 64.0).ceil() as u32 * 64).min(sw);
        let h = ((w as f32 * sh as f32 / sw as f32) as u32) & !1;
        let changed = self.sent_size.get(&id).is_none_or(|&(ow, _)| (ow as i32 - w as i32).abs() >= 64);
        if changed && w >= 64 {
            self.sent_size.insert(id, (w, h));
            let _ = self.tx.send(Cmd::DisplaySize(id, w, h));
        }
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        self.fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    // -------------------------------------------------------------- свой стрим

    fn my_stream(&mut self, ui: &mut egui::Ui) {
        let (publishing, starting) = {
            let s = self.shared.lock().unwrap();
            (s.publishing.clone(), s.starting)
        };
        Frame::new().fill(SURFACE).stroke(hairline()).corner_radius(CornerRadius::same(14)).inner_margin(Margin::symmetric(18, 14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(pv) = publishing {
                ui.horizontal(|ui| {
                    // Пульсирующая точка «в эфире»
                    let t = ui.input(|i| i.time);
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    let a = (0.55 + 0.45 * (t * 3.0).sin()) as f32;
                    ui.painter().circle_filled(r.center(), 7.0, LIVE.gamma_multiply(0.25 * a));
                    ui.painter().circle_filled(r.center(), 4.5, LIVE);
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        let what = pv.info.as_ref().map(|i| format!("{}p · {} fps{}", i.height, i.fps, if i.has_audio { " · со звуком" } else { "" })).unwrap_or_default();
                        ui.label(RichText::new(format!("Ты в эфире  ·  {what}")).font(FontId::new(15.0, semibold())));
                        ui.label(RichText::new(&pv.stats).size(12.0).color(TEXT3));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if small_button(ui, "Остановить", LIVE) {
                            let _ = self.tx.send(Cmd::Stop);
                        }
                        ui.add_space(10.0);
                        if pv.viewers.is_empty() {
                            ui.label(RichText::new("пока никто не смотрит").size(12.5).color(TEXT3));
                        } else {
                            ui.label(RichText::new(format!("смотрят: {}", pv.viewers.len())).size(12.5).color(TEXT2));
                            for v in pv.viewers.iter().take(5) {
                                avatar(ui, v, 24.0, false).on_hover_text(v);
                            }
                        }
                    });
                });
            } else {
                let mut start_clicked = false;
                let mut shown = false;
                let wide = ui.available_width() > 860.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(18.0, 6.0);
                    ui.vertical(|ui| {
                        caption(ui, "Что показать");
                        ui.horizontal(|ui| {
                            if self.source_idx >= self.sources.len() {
                                self.source_idx = 0;
                            }
                            egui::ComboBox::from_id_salt("src")
                                .width(if wide { 210.0 } else { 170.0 })
                                .selected_text(self.sources.get(self.source_idx).map(|s| s.1.as_str()).unwrap_or("—"))
                                .show_ui(ui, |ui| {
                                    for (i, (_, label)) in self.sources.iter().enumerate() {
                                        ui.selectable_value(&mut self.source_idx, i, label);
                                    }
                                });
                            if refresh_button(ui).on_hover_text("Обновить список окон").clicked() {
                                self.sources = capture::list_sources();
                            }
                        });
                    });
                    ui.vertical(|ui| {
                        caption(ui, "Качество");
                        segmented(ui, &mut self.cfg.preset, QUALITY);
                    });
                    ui.allocate_ui_with_layout(Vec2::new(64.0, 52.0), Layout::top_down(Align::Min), |ui| {
                        let cam = self.sources.get(self.source_idx).is_some_and(|s| s.0.is_camera());
                        caption(ui, if cam { "Микрофон" } else { "Звук" });
                        toggle(ui, &mut self.cfg.audio);
                    });
                    let rest = ui.available_width();
                    if rest >= 150.0 {
                        shown = true;
                        ui.allocate_ui_with_layout(Vec2::new(rest, 52.0), Layout::right_to_left(Align::Max), |ui| {
                            let label = if starting { "Запуск…" } else { "Начать стрим" };
                            start_clicked = big_button(ui, label, LIVE, 160.0, !starting);
                        });
                    }
                });
                let p = config::preset(&self.cfg.preset);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}×{} · {} fps · до {:.1} Мбит/с · подстраивается под интернет зрителей", p.width, p.height, p.fps, p.kbps as f64 / 1000.0)).size(11.5).color(TEXT3));
                    if !shown {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let label = if starting { "Запуск…" } else { "Начать стрим" };
                            start_clicked |= big_button(ui, label, LIVE, 170.0, !starting);
                        });
                    }
                });
                if start_clicked {
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
                if starting {
                    ui.label(RichText::new("Если появится окно выбора экрана — выбери, что показывать.").size(12.0).color(TEXT3));
                }
            }
        });
    }

    fn toast(&mut self, ctx: &egui::Context, notice: Option<String>) {
        let Some(n) = notice else { return };
        if self.notice_dismissed.as_deref() == Some(n.as_str()) {
            return;
        }
        egui::Area::new(egui::Id::new("toast")).anchor(Align2::RIGHT_TOP, Vec2::new(-18.0, 18.0)).order(egui::Order::Foreground).show(ctx, |ui| {
            Frame::new()
                .fill(Color32::from_rgb(0x3A, 0x1D, 0x1C))
                .stroke(Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xFF, 0x45, 0x3A, 90)))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::symmetric(14, 10))
                .shadow(egui::Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(120) })
                .show(ui, |ui| {
                    ui.set_max_width(360.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&n).color(Color32::from_rgb(0xFF, 0xB4, 0xAE)).size(13.0));
                        if ui.add(egui::Button::new(RichText::new("✕").color(TEXT2)).fill(Color32::TRANSPARENT)).clicked() {
                            self.notice_dismissed = Some(n.clone());
                        }
                    });
                });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (status, rtt, token, notice) = {
            let mut s = self.shared.lock().unwrap();
            (s.status.clone(), s.rtt_ms, s.portal_token.take(), s.notice.clone())
        };
        if let Some(t) = token {
            self.cfg.portal_token = Some(t);
            self.cfg.save();
        }
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.fullscreen {
                self.set_fullscreen(&ctx, false);
            } else {
                self.focus = None;
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F)) && self.focus.is_some() && !ctx.egui_wants_keyboard_input() {
            let fs = !self.fullscreen;
            self.set_fullscreen(&ctx, fs);
        }
        if self.fullscreen && (self.focus.is_none() || status != Status::Online) {
            self.set_fullscreen(&ctx, false);
        }

        if status == Status::Online && self.fullscreen {
            egui::CentralPanel::default().frame(Frame::new().fill(Color32::BLACK)).show(ui, |ui| self.stage(ui));
        } else if status == Status::Online {
            egui::Panel::left("sidebar")
                .resizable(false)
                .exact_size(250.0)
                .frame(Frame::new().fill(SIDEBAR).inner_margin(Margin { left: 12, right: 12, top: 0, bottom: 0 }))
                .show(ui, |ui| self.sidebar(ui, rtt));
            egui::Panel::bottom("me")
                .frame(Frame::new().fill(BG).inner_margin(Margin { left: 20, right: 20, top: 6, bottom: 18 }))
                .show(ui, |ui| self.my_stream(ui));
            egui::CentralPanel::default()
                .frame(Frame::new().fill(BG).inner_margin(Margin { left: 22, right: 22, top: 0, bottom: 0 }))
                .show(ui, |ui| self.stage(ui));
        } else {
            egui::CentralPanel::default().frame(Frame::new().fill(BG)).show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.login(ui, &status));
            });
        }
        self.toast(&ctx, notice);
    }
}
