#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod capture;
mod control;
mod icon;
mod net;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Файл журнала рядом с exe (или во временной папке, если каталог недоступен).
fn log_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("screenshare-error.log")))
        .unwrap_or_else(|| std::env::temp_dir().join("screenshare-error.log"))
}

/// Дописывает строку в журнал. Молча игнорирует ошибки записи.
fn log_line(msg: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

fn main() {
    // Если рядом с exe лежит программный OpenGL от Mesa (сборка screenshare-full для
    // серверов/ВМ без видеодрайвера), просим Mesa использовать llvmpipe — чистый
    // софт-рендер на CPU. На обычном ПК (portable-сборка) загружается системный
    // opengl32.dll, который эту переменную игнорирует, так что вреда нет.
    // SAFETY: вызывается в самом начале main, других потоков ещё нет.
    unsafe {
        std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = parse_args();

    // Паники пишем в файл: в релизе консоли нет, иначе падение было бы «немым».
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_line(&format!("PANIC: {info}"));
        default_hook(info);
    }));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ScreenShare")
            .with_inner_size([440.0, 460.0])
            .with_min_inner_size([380.0, 360.0])
            .with_icon(Arc::new(icon::icon_data(64))),
        centered: true,
        ..Default::default()
    };

    let result = eframe::run_native(
        "ScreenShare",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, cli)))),
    );

    if let Err(e) = result {
        // Самая частая причина на сервере/в ВМ без видеодрайвера — не создаётся
        // контекст OpenGL 3.3. Подсказываем решение прямо в журнале.
        log_line(&format!(
            "Не удалось запустить окно: {e}. Вероятно, на этом ПК нет современного OpenGL \
             (частый случай на Windows Server / в виртуалке без видеодрайвера). \
             Используйте сборку screenshare-full с вложенным opengl32.dll (Mesa)."
        ));
        eprintln!("ScreenShare: ошибка запуска: {e}");
        std::process::exit(1);
    }
}
