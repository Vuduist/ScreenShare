//! Графический интерфейс: главное окно, окна входящих трансляций, трей.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{
    self, Color32, ComboBox, Frame, Image, RichText, TextureOptions, Ui, ViewportBuilder,
    ViewportClass, ViewportCommand, ViewportId, load::SizedTexture,
};
use serde::{Deserialize, Serialize};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::capture::{self, MonitorInfo};
use crate::icon;
use crate::net::{self, DEFAULT_PORT, SendConfig, SenderHandle, SharedState};

const SETTINGS_KEY: &str = "settings";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    target: String,
    monitor_id: Option<u32>,
    fps: u32,
    quality: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target: String::new(),
            monitor_id: None,
            fps: 15,
            quality: 70,
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

    fn show_main_window(&mut self, ctx: &egui::Context) {
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

    fn main_ui(&mut self, ui: &mut Ui) {
        let outgoing = net::lock(&self.shared).outgoing.clone();
        let running = outgoing.running || self.sender.is_some();

        ui.heading("ScreenShare");
        ui.label(
            RichText::new(format!("Мой IP: {}  ·  порт {}", self.local_ip, DEFAULT_PORT)).weak(),
        );
        ui.add_space(6.0);

        ui.add_enabled_ui(!running, |ui| {
            ui.label("IP-адрес коллеги:");
            let edit = egui::TextEdit::singleline(&mut self.settings.target)
                .hint_text("например 192.168.1.42")
                .desired_width(f32::INFINITY);
            let response = ui.add(edit);
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter && !running {
                self.start_sharing();
            }

            ui.add_space(4.0);
            ui.label("Экран:");
            ui.horizontal(|ui| {
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

            ui.add_space(4.0);
            ui.collapsing("Настройки", |ui| {
                ui.add(egui::Slider::new(&mut self.settings.fps, 1..=30).text("кадров/с"));
                ui.add(egui::Slider::new(&mut self.settings.quality, 30..=95).text("качество JPEG"));
            });
        });

        ui.add_space(10.0);
        let (label, color) = if running {
            ("Stop", Color32::from_rgb(190, 50, 50))
        } else {
            ("Start", Color32::from_rgb(40, 140, 70))
        };
        let button = egui::Button::new(RichText::new(label).size(20.0).color(Color32::WHITE))
            .fill(color)
            .min_size(egui::vec2(ui.available_width(), 40.0));
        if ui.add(button).clicked() {
            if running {
                self.stop_sharing();
            } else {
                self.start_sharing();
            }
        }

        ui.add_space(6.0);
        if let Some(err) = &outgoing.error {
            ui.colored_label(Color32::from_rgb(220, 80, 80), err);
        } else if !outgoing.status.is_empty() {
            let mut text = outgoing.status.clone();
            if outgoing.running && outgoing.fps > 0.0 {
                text.push_str(&format!(
                    "   ({:.0} кадр/с, {:.0} КБ/с)",
                    outgoing.fps, outgoing.kbps
                ));
            }
            ui.label(text);
        } else {
            ui.label(RichText::new("Готов к показу.").weak());
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        ui.label(RichText::new("Входящие трансляции").strong());

        let mut to_show: Option<u64> = None;
        let mut to_hide: Option<u64> = None;
        {
            let sh = net::lock(&self.shared);
            if let Some(err) = &sh.listen_error {
                ui.colored_label(Color32::from_rgb(220, 80, 80), err);
            }
            let active: Vec<&net::IncomingStream> =
                sh.streams.iter().filter(|s| !s.finished).collect();
            if active.is_empty() {
                ui.label(RichText::new("Сейчас никто не показывает вам экран.").weak());
            }
            for s in active {
                ui.horizontal(|ui| {
                    let secs = s.started.elapsed().as_secs();
                    ui.label(format!(
                        "{} ({})  {}×{}  {:02}:{:02}",
                        s.name,
                        s.peer.ip(),
                        s.size[0],
                        s.size[1],
                        secs / 60,
                        secs % 60
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if s.hidden {
                            if ui.button("Показать окно").clicked() {
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

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.label(
                RichText::new("Закрытие окна сворачивает программу в трей. Выход — через меню трея.")
                    .weak()
                    .small(),
            );
        });
    }

    /// Объявляет окна для всех активных входящих трансляций.
    fn show_stream_windows(&mut self, ctx: &egui::Context) {
        struct Pending {
            id: u64,
            viewport: ViewportId,
            title: String,
            size: [f32; 2],
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
                    Pending {
                        id: s.id,
                        viewport: s.viewport_id(),
                        title: format!("Экран: {} ({})", s.name, s.peer.ip()),
                        size,
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
            ctx.show_viewport_deferred(p.viewport, builder, move |ui, class| {
                viewer_ui(ui, class, &shared, id);
            });
        }
    }
}

fn viewer_ui(ui: &mut Ui, _class: ViewportClass, shared: &SharedState, id: u64) {
    let ctx = ui.ctx().clone();

    if ctx.input(|i| i.viewport().close_requested()) {
        // Окно «закрывают» — прячем его; трансляция продолжается,
        // окно можно вернуть из главного окна.
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
            ui.centered_and_justified(|ui| match texture {
                Some(t) => {
                    ui.add(Image::from_texture(t).shrink_to_fit());
                }
                None => {
                    ui.label(RichText::new("Ожидание изображения…").color(Color32::GRAY));
                }
            });
        });
}

impl eframe::App for App {
    /// Вызывается перед каждым кадром, а также когда главное окно скрыто (в трее)
    /// и кто-то запросил перерисовку — например, пришла новая трансляция.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }

        // Поток отправки завершился сам (ошибка/обрыв) — убираем хэндл.
        if self.sender.is_some() && !net::lock(&self.shared).outgoing.running {
            self.sender = None;
        }

        // Главное окно скрыто, а окна трансляций объявляются только из его UI-прохода:
        // просим выполнить проход, если появилась новая трансляция или закончилась старая.
        let hidden = ctx.input(|i| i.viewport().visible() == Some(false));
        if hidden {
            let needs_pass = net::lock(&self.shared)
                .streams
                .iter()
                .any(|s| s.finished || (!s.hidden && s.texture.is_none()));
            if needs_pass {
                ctx.send_viewport_cmd(ViewportCommand::RequestPaintWhileHidden);
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| self.main_ui(ui));
        let ctx = ui.ctx().clone();
        self.show_stream_windows(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}
