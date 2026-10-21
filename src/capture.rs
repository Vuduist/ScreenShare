//! Захват экрана и кодирование/декодирование кадров (JPEG).

use xcap::Monitor;

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub id: u32,
    pub label: String,
}

/// Список мониторов для выпадающего списка.
pub fn list_monitors() -> Vec<MonitorInfo> {
    let Ok(monitors) = Monitor::all() else {
        return Vec::new();
    };
    monitors
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let id = m.id().ok()?;
            let w = m.width().unwrap_or(0);
            let h = m.height().unwrap_or(0);
            let primary = m.is_primary().unwrap_or(false);
            let name = m
                .friendly_name()
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| m.name().ok())
                .unwrap_or_default();
            let label = format!(
                "{}: {} ({}×{}){}",
                i + 1,
                name,
                w,
                h,
                if primary { " — основной" } else { "" }
            );
            Some(MonitorInfo { id, label })
        })
        .collect()
}

pub fn find_monitor(id: u32) -> Option<Monitor> {
    Monitor::all()
        .ok()?
        .into_iter()
        .find(|m| m.id().ok() == Some(id))
}

/// Кодирует RGBA-буфер в JPEG.
pub fn encode_jpeg(rgba: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>, String> {
    if width > u16::MAX as u32 || height > u16::MAX as u32 {
        return Err(format!("Слишком большое разрешение: {width}×{height}"));
    }
    let mut out = Vec::with_capacity(rgba.len() / 8);
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .encode(rgba, width as u16, height as u16, jpeg_encoder::ColorType::Rgba)
        .map_err(|e| format!("Ошибка кодирования JPEG: {e}"))?;
    Ok(out)
}

/// Декодирует JPEG в RGBA. Возвращает (пиксели, ширина, высота).
pub fn decode_jpeg(data: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    use zune_jpeg::zune_core::{colorspace::ColorSpace, options::DecoderOptions};

    let options = DecoderOptions::new_fast().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(data), options);
    let pixels = decoder
        .decode()
        .map_err(|e| format!("Ошибка декодирования JPEG: {e:?}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| "JPEG без заголовка".to_string())?;
    let (w, h) = (info.width as usize, info.height as usize);
    if pixels.len() != w * h * 4 {
        return Err(format!(
            "Неожиданный размер буфера: {} вместо {}",
            pixels.len(),
            w * h * 4
        ));
    }
    Ok((pixels, w, h))
}
