//! Иконка приложения, рисуется программно: синий монитор на подставке.

use eframe::egui;

/// RGBA-пиксели иконки размером `size`×`size`.
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let fx = (x as f32 + 0.5) / s;
            let fy = (y as f32 + 0.5) / s;
            out.extend_from_slice(&pixel(fx, fy));
        }
    }
    out
}

pub fn icon_data(size: u32) -> egui::IconData {
    egui::IconData {
        rgba: rgba(size),
        width: size,
        height: size,
    }
}

fn in_rounded_rect(fx: f32, fy: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if fx < x0 || fx > x1 || fy < y0 || fy > y1 {
        return false;
    }
    let cx = fx.clamp(x0 + r, x1 - r);
    let cy = fy.clamp(y0 + r, y1 - r);
    let dx = fx - cx;
    let dy = fy - cy;
    dx * dx + dy * dy <= r * r
}

fn pixel(fx: f32, fy: f32) -> [u8; 4] {
    // Экран (светлый, с лёгким градиентом).
    if in_rounded_rect(fx, fy, 0.14, 0.18, 0.86, 0.62, 0.03) {
        let t = ((fx + fy) * 0.5).clamp(0.0, 1.0);
        let r = (200.0 + 40.0 * t) as u8;
        let g = (225.0 + 25.0 * t) as u8;
        return [r, g, 255, 255];
    }
    // Корпус монитора.
    if in_rounded_rect(fx, fy, 0.06, 0.10, 0.94, 0.70, 0.08) {
        return [37, 99, 235, 255];
    }
    // Ножка.
    if in_rounded_rect(fx, fy, 0.43, 0.70, 0.57, 0.84, 0.02) {
        return [71, 85, 105, 255];
    }
    // Подставка.
    if in_rounded_rect(fx, fy, 0.26, 0.82, 0.74, 0.92, 0.04) {
        return [71, 85, 105, 255];
    }
    [0, 0, 0, 0]
}
