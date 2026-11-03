//! Графический интерфейс: главное окно, окна удалённых рабочих столов,
//! индикатор отдачи доступа и трей.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{
    self, Color32, ComboBox, Event, Frame, Image, Key, MouseWheelUnit, PointerButton, Pos2, Rect,
    RichText, TextureOptions, Ui, ViewportBuilder, ViewportClass, ViewportCommand, ViewportId,
    load::SizedTexture,
};
use serde::{Deserialize, Serialize};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::capture::{self, MonitorInfo};
use crate::control::{self, InputEvent};
use crate::icon;
use crate::net::{self, DEFAULT_PORT, GrantConfig, SendConfig, SenderHandle, SharedState};

const SETTINGS_KEY: &str = "settings";

const RED: Color32 = Color32::from_rgb(200, 60, 60);
const GREEN: Color32 = Color32::from_rgb(40, 140, 70);
const BLUE: Color32 = Color32::from_rgb(40, 90, 160);
const ERR: Color32 = Color32::from_rgb(220, 80, 80);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    target: String,
    monitor_id: Option<u32>,
    fps: u32,
    quality: u8,
    grant_target: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target: String::new(),
            monitor_id: None,
            fps: 15,
            quality: 70,
            grant_target: String::new(),
        }
    }
}

pub struct App {
    shared: SharedState,
    settings: Settings,
    monitors: Vec<MonitorInfo>,
    sender: Option<SenderHandle>,
    _tray: Option<TrayIcon>,
    tray_clicked: Arc<AtomicBool>,
    menu_events: Arc<Mutex<Vec<MenuId>>>,
    menu_show: MenuId,
    menu_exit: MenuId,
    allow_close: bool,
    local_ip: String,
    hostname: String,
    viewer_icon: Arc<egui::IconData>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cli: crate::CliArgs) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();

        let shared: SharedState = Arc::new(Mutex::new(net::Shared::new(cc.egui_ctx.clone())));
        net::start_listener(shared.clone(), DEFAULT_PORT);

        // --- Трей -----------------------------------------------------------
        let tray_clicked = Arc::new(AtomicBool::new(false));
        let menu_events: Arc<Mutex<Vec<MenuId>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let ctx = cc.egui_ctx.clone();
            let flag = tray_clicked.clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                let clicked = matches!(
                    event,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } | TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    }
                );
                if clicked {
                    flag.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            }));
            let ctx = cc.egui_ctx.clone();
            let events = menu_events.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event.id().clone());
                ctx.request_repaint();
            }));
        }

        let show_item = MenuItem::new("Открыть ScreenShare", true, None);
        let exit_item = MenuItem::new("Выход", true, None);
        let menu_show = show_item.id().clone();
        let menu_exit = exit_item.id().clone();
        let menu = Menu::new();
        if let Err(e) = menu.append_items(&[&show_item, &PredefinedMenuItem::separator(), &exit_item]) {
            log::warn!("tray menu: {e}");
        }

        let tray = Icon::from_rgba(icon::rgba(32), 32, 32)
            .ok()
            .and_then(|tray_icon| {
                TrayIconBuilder::new()
                    .with_tooltip("ScreenShare")
                    .with_icon(tray_icon)
                    .with_menu(Box::new(menu))
                    .build()
                    .map_err(|e| log::warn!("tray icon: {e}"))
                    .ok()
            });

        let mut app = Self {
            shared,
            settings,
            monitors: Vec::new(),
            sender: None,
            _tray: tray,
            tray_clicked,
            menu_events,
            menu_show,
            menu_exit,
            allow_close: false,
            local_ip: {
                let ips = net::local_ips();
                if ips.is_empty() { "?".into() } else { ips[..ips.len().min(3)].join(", ") }
            },
            hostname: net::hostname(),
            viewer_icon: Arc::new(icon::icon_data(64)),
        };
        app.refresh_monitors();

        if let Some(n) = cli.screen {
            match app.monitors.get(n.saturating_sub(1)) {
                Some(m) => app.settings.monitor_id = Some(m.id),
                None => log::warn!("--screen {n}: такого экрана нет"),
            }
        }
        if let Some(target) = cli.share {
            app.settings.target = target;
            app.start_sharing();
        }
        if let Some(target) = cli.grant {
            app.settings.grant_target = target;
            app.start_grant();
        }
        app
    }

    fn refresh_monitors(&mut self) {
        self.monitors = capture::list_monitors();
        let known = self
            .settings
            .monitor_id
            .filter(|id| self.monitors.iter().any(|m| m.id == *id));
        self.settings.monitor_id = known.or_else(|| self.monitors.first().map(|m| m.id));
    }

    fn show_main_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    fn parse_target(input: &str) -> Result<SocketAddr, String> {
        let s = input.trim();
        if s.is_empty() {
            return Err("Введите IP-адрес коллеги.".into());
        }
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(addr);
        }
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, DEFAULT_PORT));
        }
        Err(format!("«{s}» не похоже на IP-адрес (пример: 192.168.1.42)."))
    }

    // --- Показ своего экрана --------------------------------------------

    fn start_sharing(&mut self) {
        let target = match Self::parse_target(&self.settings.target) {
            Ok(t) => t,
            Err(e) => {
                net::lock(&self.shared).outgoing.error = Some(e);
                return;
            }
        };
        let Some(monitor_id) = self.settings.monitor_id else {
            net::lock(&self.shared).outgoing.error = Some("Экран не выбран.".into());
            return;
        };
        let cfg = SendConfig {
            target,
            monitor_id,
            fps: self.settings.fps,
            quality: self.settings.quality,
            name: self.hostname.clone(),
        };
        self.sender = Some(net::start_sender(self.shared.clone(), cfg));
    }

    fn stop_sharing(&mut self) {
        if let Some(handle) = self.sender.take() {
            handle.stop();
        }
    }

    // --- Отдача доступа к своему рабочему столу --------------------------

    fn is_granting(&self) -> bool {
        net::lock(&self.shared).grant.active
    }

    fn start_grant(&mut self) {
        let target = match Self::parse_target(&self.settings.grant_target) {
            Ok(t) => t,
            Err(e) => {
                net::lock(&self.shared).grant.error = Some(e);
                return;
            }
        };
        let cfg = GrantConfig {
            target,
            name: self.hostname.clone(),
        };
        net::start_grant(self.shared.clone(), cfg);
    }

    fn stop_grant(&self) {
        let mut sh = net::lock(&self.shared);
        for c in sh.controls.iter() {
            c.revoke();
        }
        if sh.grant.active {
            sh.grant.status = "Закрытие доступа…".into();
        }
        sh.ctx.request_repaint();
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        if self.tray_clicked.swap(false, Ordering::Relaxed) {
            self.show_main_window(ctx);
        }
        let events: Vec<MenuId> = std::mem::take(
            &mut *self.menu_events.lock().unwrap_or_else(|e| e.into_inner()),
        );
        for id in events {
            if id == self.menu_show {
                self.show_main_window(ctx);
            } else if id == self.menu_exit {
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    // --- Главное окно ----------------------------------------------------

    fn main_ui(&mut self, ui: &mut Ui) {
        ui.heading("ScreenShare");
        ui.label(
            RichText::new(format!("Мой IP: {}  ·  порт {}", self.local_ip, DEFAULT_PORT)).weak(),
        );

        self.active_control_ui(ui);

        ui.add_space(6.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.grant_ui(ui);
            ui.add_space(8.0);
            ui.separator();
            self.share_ui(ui);
            ui.add_space(8.0);
            ui.separator();
            self.incoming_ui(ui);
        });

        ui.add_space(4.0);
        ui.label(
            RichText::new("Закрытие окна сворачивает программу в трей. Выход — через меню трея.")
                .weak()
                .small(),
        );
    }

    /// Баннер: мы прямо сейчас отдаём кому-то управление своим столом.
    fn active_control_ui(&mut self, ui: &mut Ui) {
        let sessions: Vec<(u64, String, String, u64)> = {
            let sh = net::lock(&self.shared);
            sh.controls
                .iter()
                .map(|c| (c.id, c.controller.clone(), c.peer.ip().to_string(), c.started.elapsed().as_secs()))
                .collect()
        };
        for (id, _name, ip, secs) in sessions {
            ui.add_space(6.0);
            Frame::group(ui.style())
                .fill(Color32::from_rgb(70, 25, 25))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "● Вы отдали управление своим столом → {}   {:02}:{:02}",
                            ip,
                            secs / 60,
                            secs % 60
                        ))
                        .strong()
                        .color(Color32::from_rgb(255, 150, 150)),
                    );
                    if ui
                        .add(egui::Button::new(RichText::new("Прекратить").color(Color32::WHITE)).fill(RED))
                        .clicked()
                    {
                        self.revoke_control(id);
                    }
                });
        }
    }

    fn revoke_control(&self, id: u64) {
        let sh = net::lock(&self.shared);
        if let Some(c) = sh.controls.iter().find(|c| c.id == id) {
            c.revoke();
        }
    }

    fn grant_ui(&mut self, ui: &mut Ui) {
        let granting = self.is_granting();
        let status = net::lock(&self.shared).grant.clone();

        ui.label(RichText::new("Дать доступ к моему рабочему столу").strong());
        ui.label(
            RichText::new("Коллега сразу сможет управлять вашим ПК мышью и клавиатурой.")
                .weak()
                .small(),
        );
        ui.add_enabled_ui(!granting, |ui| {
            ui.label("IP управляющего (кому дать доступ):");
            let edit = egui::TextEdit::singleline(&mut self.settings.grant_target)
                .hint_text("например 192.168.1.42")
                .desired_width(f32::INFINITY);
            let resp = ui.add(edit);
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !granting {
                self.start_grant();
            }
        });

        let (label, color) = if granting {
            ("Прекратить доступ", RED)
        } else {
            ("Дать доступ", BLUE)
        };
        if ui
            .add(
                egui::Button::new(RichText::new(label).size(18.0).color(Color32::WHITE))
                    .fill(color)
                    .min_size(egui::vec2(ui.available_width(), 38.0)),
            )
            .clicked()
        {
            if granting {
                self.stop_grant();
            } else {
                self.start_grant();
            }
        }
        if let Some(err) = &status.error {
            ui.colored_label(ERR, err);
        } else if !status.status.is_empty() {
            ui.label(&status.status);
        }
    }

    fn share_ui(&mut self, ui: &mut Ui) {
        let outgoing = net::lock(&self.shared).outgoing.clone();
        let running = outgoing.running || self.sender.is_some();

        ui.label(RichText::new("Показать свой экран коллеге").strong());
        ui.label(
            RichText::new("Только картинка, без управления.").weak().small(),
        );
        ui.add_enabled_ui(!running, |ui| {
            ui.label("IP-адрес коллеги:");
            let edit = egui::TextEdit::singleline(&mut self.settings.target)
                .hint_text("например 192.168.1.42")
                .desired_width(f32::INFINITY);
            let response = ui.add(edit);
            if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !running {
                self.start_sharing();
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Экран:");
                let selected = self
                    .monitors
                    .iter()
                    .find(|m| Some(m.id) == self.settings.monitor_id)
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| "Экраны не найдены".into());
                ComboBox::from_id_salt("monitor")
                    .width(ui.available_width() - 90.0)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for m in &self.monitors {
                            ui.selectable_value(&mut self.settings.monitor_id, Some(m.id), &m.label);
                        }
                    });
                if ui.button("Обновить").clicked() {
                    self.refresh_monitors();
                }
            });

            ui.collapsing("Качество", |ui| {
                ui.add(egui::Slider::new(&mut self.settings.fps, 1..=30).text("кадров/с"));
                ui.add(egui::Slider::new(&mut self.settings.quality, 30..=95).text("качество JPEG"));
            });
        });

        let (label, color) = if running { ("Stop показ", RED) } else { ("Start показ", GREEN) };
        if ui
            .add(
                egui::Button::new(RichText::new(label).size(18.0).color(Color32::WHITE))
                    .fill(color)
                    .min_size(egui::vec2(ui.available_width(), 34.0)),
            )
            .clicked()
        {
            if running {
                self.stop_sharing();
            } else {
                self.start_sharing();
            }
        }
        if let Some(err) = &outgoing.error {
            ui.colored_label(ERR, err);
        } else if !outgoing.status.is_empty() {
            let mut text = outgoing.status.clone();
            if outgoing.running && outgoing.fps > 0.0 {
                text.push_str(&format!("   ({:.0} кадр/с, {:.0} КБ/с)", outgoing.fps, outgoing.kbps));
            }
            ui.label(text);
        }
    }

    fn incoming_ui(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Окна удалённых экранов").strong());
        ui.label(
            RichText::new(
                "Здесь появляются экраны тех, кто дал вам доступ или показывает вам экран. \
                 Чтобы управлять чужим ПК — попросите коллегу открыть «Дать доступ» и ввести ваш IP.",
            )
            .weak()
            .small(),
        );
        let mut to_show: Option<u64> = None;
        let mut to_hide: Option<u64> = None;
        {
            let sh = net::lock(&self.shared);
            if let Some(err) = &sh.listen_error {
                ui.colored_label(ERR, err);
            }
            let active: Vec<&net::IncomingStream> = sh.streams.iter().filter(|s| !s.finished).collect();
            if active.is_empty() {
                ui.label(RichText::new("Сейчас нет открытых экранов.").weak());
            }
            for s in active {
                ui.horizontal(|ui| {
                    let kind = if s.controllable { "управление" } else { "показ" };
                    let secs = s.started.elapsed().as_secs();
                    ui.label(format!(
                        "{} ({}) · {} · {}×{} · {:02}:{:02}",
                        s.name, s.peer.ip(), kind, s.size[0], s.size[1], secs / 60, secs % 60
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if s.hidden {
                            if ui.button("Показать").clicked() {
                                to_show = Some(s.id);
                            }
                        } else if ui.button("Скрыть").clicked() {
                            to_hide = Some(s.id);
                        }
                    });
                });
            }
        }
        if to_show.is_some() || to_hide.is_some() {
            let mut sh = net::lock(&self.shared);
            if let Some(s) = to_show.and_then(|id| sh.stream_mut(id)) {
                s.hidden = false;
            }
            if let Some(s) = to_hide.and_then(|id| sh.stream_mut(id)) {
                s.hidden = true;
            }
        }
    }

    /// Объявляет окна для всех активных удалённых экранов.
    fn show_stream_windows(&mut self, ctx: &egui::Context) {
        struct Pending {
            id: u64,
            viewport: ViewportId,
            title: String,
            size: [f32; 2],
            controllable: bool,
        }
        let pending: Vec<Pending> = {
            let mut sh = net::lock(&self.shared);
            sh.streams.retain(|s| !s.finished);
            sh.streams
                .iter()
                .filter(|s| !s.hidden)
                .map(|s| {
                    let (w, h) = (s.size[0].max(1) as f32, s.size[1].max(1) as f32);
                    let scale = (1280.0 / w).min(720.0 / h).min(1.0);
                    let size = if s.size[0] == 0 {
                        [960.0, 540.0]
                    } else {
                        [(w * scale).max(320.0), (h * scale).max(180.0)]
                    };
                    let title = if s.controllable {
                        format!("Управление: {} ({})", s.name, s.peer.ip())
                    } else {
                        format!("Экран: {} ({})", s.name, s.peer.ip())
                    };
                    Pending {
                        id: s.id,
                        viewport: s.viewport_id(),
                        title,
                        size,
                        controllable: s.controllable,
                    }
                })
                .collect()
        };

        for p in pending {
            let builder = ViewportBuilder::default()
                .with_title(p.title)
                .with_inner_size(p.size)
                .with_min_inner_size([320.0, 180.0])
                .with_icon(self.viewer_icon.clone())
                .with_active(true);
            let shared = self.shared.clone();
            let id = p.id;
            let controllable = p.controllable;
            ctx.show_viewport_deferred(p.viewport, builder, move |ui, class| {
                viewer_ui(ui, class, &shared, id, controllable);
            });
        }
    }

    /// Маленькое всплывающее окно-индикатор, пока мы отдаём кому-то управление.
    fn show_indicator(&self, ctx: &egui::Context) {
        let targets: Vec<String> = net::lock(&self.shared)
            .controls
            .iter()
            .map(|c| c.peer.ip().to_string())
            .collect();
        if targets.is_empty() {
            return;
        }
        let shared = self.shared.clone();
        let builder = ViewportBuilder::default()
            .with_title("Доступ открыт")
            .with_inner_size([360.0, 96.0])
            .with_resizable(false)
            .with_always_on_top()
            .with_icon(self.viewer_icon.clone());
        ctx.show_viewport_deferred(
            ViewportId::from_hash_of("screenshare-indicator"),
            builder,
            move |ui, _class| {
                egui::CentralPanel::default()
                    .frame(Frame::NONE.fill(Color32::from_rgb(70, 20, 20)).inner_margin(10.0))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("● Вашим ПК управляют: {}", targets.join(", ")))
                                .strong()
                                .color(Color32::from_rgb(255, 160, 160)),
                        );
                        if ui
                            .add(egui::Button::new(RichText::new("Прекратить доступ").color(Color32::WHITE)).fill(RED))
                            .clicked()
                        {
                            for c in net::lock(&shared).controls.iter() {
                                c.revoke();
                            }
                        }
                    });
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            },
        );
    }
}

/// Окно удалённого экрана. Для управляемого потока дополнительно захватывает ввод.
fn viewer_ui(ui: &mut Ui, _class: ViewportClass, shared: &SharedState, id: u64, controllable: bool) {
    let ctx = ui.ctx().clone();

    if ctx.input(|i| i.viewport().close_requested()) {
        if let Some(s) = net::lock(shared).stream_mut(id) {
            s.hidden = true;
        }
        ctx.request_repaint_of(ViewportId::ROOT);
    }

    let texture = {
        let mut sh = net::lock(shared);
        let Some(s) = sh.stream_mut(id) else {
            return;
        };
        if let Some(image) = s.pending.take() {
            match &mut s.texture {
                Some(t) => t.set(image, TextureOptions::LINEAR),
                None => {
                    s.texture =
                        Some(ctx.load_texture(format!("stream-{id}"), image, TextureOptions::LINEAR));
                }
            }
        }
        s.texture.as_ref().map(SizedTexture::from_handle)
    };

    egui::CentralPanel::default()
        .frame(Frame::NONE.fill(Color32::BLACK))
        .show(ui, |ui| {
            let rect = ui
                .centered_and_justified(|ui| match texture {
                    Some(t) => ui.add(Image::from_texture(t).shrink_to_fit()).rect,
                    None => {
                        ui.label(RichText::new("Ожидание изображения…").color(Color32::GRAY));
                        Rect::NOTHING
                    }
                })
                .inner;
            if controllable {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
                if rect.is_positive() {
                    capture_input(ui, shared, id, rect);
                }
            }
        });
}

/// Считывает мышь и клавиатуру в окне управления и шлёт события управляемому.
fn capture_input(ui: &Ui, shared: &SharedState, id: u64, rect: Rect) {
    let tx = {
        let sh = net::lock(shared);
        sh.streams
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.input_tx.clone())
    };
    let Some(tx) = tx else {
        return;
    };

    let (events, mods) = ui.input(|i| (i.events.clone(), i.modifiers));

    let prev = net::lock(shared).stream_mut(id).map(|s| s.prev_mods).unwrap_or_default();
    for (key_id, now) in control::modifier_ids(mods) {
        let was = match key_id {
            control::keyid::CONTROL => prev.ctrl || prev.command,
            control::keyid::ALT => prev.alt,
            control::keyid::META => prev.mac_cmd,
            _ => false,
        };
        if now != was {
            let _ = tx.send(InputEvent::Key { id: key_id, press: now });
        }
    }
    if let Some(s) = net::lock(shared).stream_mut(id) {
        s.prev_mods = mods;
    }

    let norm = |p: Pos2| {
        (
            ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0),
            ((p.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0),
        )
    };

    for ev in events {
        match ev {
            Event::PointerMoved(pos) if rect.contains(pos) => {
                let (x, y) = norm(pos);
                let changed = net::lock(shared)
                    .stream_mut(id)
                    .map(|s| {
                        let c = s.last_sent_pos != Some((x, y));
                        if c {
                            s.last_sent_pos = Some((x, y));
                        }
                        c
                    })
                    .unwrap_or(false);
                if changed {
                    let _ = tx.send(InputEvent::MouseMove { x, y });
                }
            }
            Event::PointerButton { pos, button, pressed, .. } => {
                if rect.contains(pos) || !pressed {
                    let (x, y) = norm(pos);
                    let b = match button {
                        PointerButton::Primary => 0,
                        PointerButton::Secondary => 1,
                        PointerButton::Middle => 2,
                        _ => continue,
                    };
                    let _ = tx.send(InputEvent::MouseButton { button: b, press: pressed, x, y });
                }
            }
            Event::MouseWheel { unit, delta, .. } => {
                let scale = match unit {
                    MouseWheelUnit::Line => 1.0,
                    MouseWheelUnit::Point => 1.0 / 50.0,
                    MouseWheelUnit::Page => 3.0,
                };
                let dx = (delta.x * scale).round() as i32;
                let dy = (delta.y * scale).round() as i32;
                if dx != 0 || dy != 0 {
                    let _ = tx.send(InputEvent::Scroll { dx, dy });
                }
            }
            Event::Key { key, pressed, modifiers, .. } => {
                if control::is_navigation_key(key) || control::shortcut_active(modifiers) {
                    if let Some(kid) = control::egui_key_to_id(key) {
                        let _ = tx.send(InputEvent::Key { id: kid, press: pressed });
                    }
                }
            }
            Event::Text(t) if !t.is_empty() => {
                let _ = tx.send(InputEvent::Text(t));
            }
            _ => {}
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }

        if self.sender.is_some() && !net::lock(&self.shared).outgoing.running {
            self.sender = None;
        }

        // Окна-дети объявляются только из прохода главного окна: пока оно скрыто,
        // просим выполнить проход, если есть что показывать.
        let hidden = ctx.input(|i| i.viewport().visible() == Some(false));
        if hidden {
            let sh = net::lock(&self.shared);
            let needs_pass = !sh.controls.is_empty()
                || sh
                    .streams
                    .iter()
                    .any(|s| s.finished || (!s.hidden && s.texture.is_none()));
            drop(sh);
            if needs_pass {
                ctx.send_viewport_cmd(ViewportCommand::RequestPaintWhileHidden);
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| self.main_ui(ui));
        let ctx = ui.ctx().clone();
        self.show_stream_windows(&ctx);
        self.show_indicator(&ctx);

        let busy = {
            let sh = net::lock(&self.shared);
            !sh.controls.is_empty() || sh.outgoing.running
        };
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}
