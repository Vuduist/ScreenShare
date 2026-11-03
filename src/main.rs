#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod capture;
mod control;
mod icon;
mod net;

use std::sync::Arc;

use eframe::egui;

/// Параметры командной строки.
///
/// * `screenshare --share 192.168.1.42 [--screen 2]` — сразу начать показ своего экрана;
/// * `screenshare --grant 192.168.1.42` — сразу дать управляющему (по его IP) доступ к
///   управлению этим рабочим столом (удобно для ярлыка «пустить Петю к себе»).
#[derive(Clone, Debug, Default)]
pub struct CliArgs {
    pub share: Option<String>,
    pub grant: Option<String>,
    pub screen: Option<usize>,
}

fn parse_args() -> CliArgs {
    let mut args = CliArgs::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--share" => args.share = it.next(),
            "--grant" => args.grant = it.next(),
            "--screen" => args.screen = it.next().and_then(|s| s.parse().ok()),
            other => log::warn!("неизвестный аргумент: {other}"),
        }
    }
    args
}

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = parse_args();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ScreenShare")
            .with_inner_size([440.0, 460.0])
            .with_min_inner_size([380.0, 360.0])
            .with_icon(Arc::new(icon::icon_data(64))),
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "ScreenShare",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, cli)))),
    )
}
