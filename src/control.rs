//! Удалённое управление: события ввода и их инжекция на управляемой машине.
//!
//! События идут от контроллера к агенту одним потоком поверх того же TCP-соединения,
//! по которому агент отдаёт картинку. Координаты мыши нормализованы (0..1) относительно
//! удалённого экрана, чтобы не зависеть от разрешения сторон.

use std::io::{self, Read, Write};

use eframe::egui;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};

// Идентификаторы специальных клавиш в протоколе (устойчивые, не зависят от версий egui/enigo).
pub mod keyid {
    pub const ENTER: u32 = 1;
    pub const TAB: u32 = 2;
    pub const SPACE: u32 = 3;
    pub const BACKSPACE: u32 = 4;
    pub const DELETE: u32 = 5;
    pub const ESCAPE: u32 = 6;
    pub const INSERT: u32 = 7;
    pub const HOME: u32 = 8;
    pub const END: u32 = 9;
    pub const PAGE_UP: u32 = 10;
    pub const PAGE_DOWN: u32 = 11;
    pub const ARROW_UP: u32 = 12;
    pub const ARROW_DOWN: u32 = 13;
    pub const ARROW_LEFT: u32 = 14;
    pub const ARROW_RIGHT: u32 = 15;

    pub const SHIFT: u32 = 20;
    pub const CONTROL: u32 = 21;
    pub const ALT: u32 = 22;
    pub const META: u32 = 23;

    pub const F1: u32 = 100; // F1..F12 = 100..=111

    /// Клавиши-символы (буквы, цифры): id = BASE + код символа.
    pub const UNICODE_BASE: u32 = 0x10_0000;
}

/// Состояние модификаторов, которые мы удерживаем на агенте по `egui::Modifiers`.
///
/// Shift сюда НЕ входит специально: ввод символов идёт событием текста (в котором
/// заглавные буквы уже учтены), а удержание Shift на агенте исказило бы такой ввод.
/// Поэтому Shift+сочетания и выделение Shift+стрелка в этой версии не передаются.
pub fn modifier_ids(m: egui::Modifiers) -> [(u32, bool); 3] {
    [
        (keyid::CONTROL, m.ctrl || m.command),
        (keyid::ALT, m.alt),
        (keyid::META, m.mac_cmd),
    ]
}

/// Активно ли сочетание (есть управляющий модификатор) — тогда буквенные клавиши
/// передаём как клавиши, а не как текст.
pub fn shortcut_active(m: egui::Modifiers) -> bool {
    m.ctrl || m.command || m.alt || m.mac_cmd
}

/// Навигационные/управляющие клавиши, которые не порождают ввод текста и должны
/// передаваться всегда (Enter, Tab, стрелки, F-клавиши и т. п.). Space исключён —
/// он приходит текстом.
pub fn is_navigation_key(key: egui::Key) -> bool {
    use egui::Key::*;
    matches!(
        key,
        Enter | Tab | Escape | Backspace | Delete | Insert | Home | End | PageUp | PageDown
            | ArrowUp | ArrowDown | ArrowLeft | ArrowRight
            | F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12
    )
}

/// Преобразует клавишу egui в id протокола. Буквы/цифры — как Unicode-символы,
/// чтобы на агенте сработали сочетания вроде Ctrl+C.
pub fn egui_key_to_id(key: egui::Key) -> Option<u32> {
    use egui::Key::*;
    Some(match key {
        Enter => keyid::ENTER,
        Tab => keyid::TAB,
        Space => keyid::SPACE,
        Backspace => keyid::BACKSPACE,
        Delete => keyid::DELETE,
        Escape => keyid::ESCAPE,
        Insert => keyid::INSERT,
        Home => keyid::HOME,
        End => keyid::END,
        PageUp => keyid::PAGE_UP,
        PageDown => keyid::PAGE_DOWN,
        ArrowUp => keyid::ARROW_UP,
        ArrowDown => keyid::ARROW_DOWN,
        ArrowLeft => keyid::ARROW_LEFT,
        ArrowRight => keyid::ARROW_RIGHT,
        F1 => keyid::F1,
        F2 => keyid::F1 + 1,
        F3 => keyid::F1 + 2,
        F4 => keyid::F1 + 3,
        F5 => keyid::F1 + 4,
        F6 => keyid::F1 + 5,
        F7 => keyid::F1 + 6,
        F8 => keyid::F1 + 7,
        F9 => keyid::F1 + 8,
        F10 => keyid::F1 + 9,
        F11 => keyid::F1 + 10,
        F12 => keyid::F1 + 11,
        other => {
            // Буквы и цифры передаём как символы (для сочетаний с модификаторами).
            let name = other.name();
            let bytes = name.as_bytes();
            if bytes.len() == 1 {
                let c = bytes[0].to_ascii_lowercase();
                if c.is_ascii_alphanumeric() {
                    return Some(keyid::UNICODE_BASE + c as u32);
                }
            }
            return None;
        }
    })
}

fn id_to_enigo_key(id: u32) -> Option<Key> {
    Some(match id {
        keyid::ENTER => Key::Return,
        keyid::TAB => Key::Tab,
        keyid::SPACE => Key::Space,
        keyid::BACKSPACE => Key::Backspace,
        keyid::DELETE => Key::Delete,
        keyid::ESCAPE => Key::Escape,
        keyid::INSERT => Key::Insert,
        keyid::HOME => Key::Home,
        keyid::END => Key::End,
        keyid::PAGE_UP => Key::PageUp,
        keyid::PAGE_DOWN => Key::PageDown,
        keyid::ARROW_UP => Key::UpArrow,
        keyid::ARROW_DOWN => Key::DownArrow,
        keyid::ARROW_LEFT => Key::LeftArrow,
        keyid::ARROW_RIGHT => Key::RightArrow,
        keyid::SHIFT => Key::Shift,
        keyid::CONTROL => Key::Control,
        keyid::ALT => Key::Alt,
        keyid::META => Key::Meta,
        _ if id >= keyid::UNICODE_BASE => {
            Key::Unicode(char::from_u32(id - keyid::UNICODE_BASE)?)
        }
        _ => return None,
    })
}

fn id_to_fkey(id: u32) -> Option<Key> {
    match id {
        100 => Some(Key::F1),
        101 => Some(Key::F2),
        102 => Some(Key::F3),
        103 => Some(Key::F4),
        104 => Some(Key::F5),
        105 => Some(Key::F6),
        106 => Some(Key::F7),
        107 => Some(Key::F8),
        108 => Some(Key::F9),
        109 => Some(Key::F10),
        110 => Some(Key::F11),
        111 => Some(Key::F12),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// События ввода и их сериализация.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum InputEvent {
    /// Движение мыши, координаты 0..1 относительно удалённого экрана.
    MouseMove { x: f32, y: f32 },
    /// Кнопка мыши: 0 — левая, 1 — правая, 2 — средняя.
    MouseButton { button: u8, press: bool, x: f32, y: f32 },
    /// Прокрутка в «щелчках» колеса.
    Scroll { dx: i32, dy: i32 },
    /// Специальная клавиша/модификатор по id из [`keyid`].
    Key { id: u32, press: bool },
    /// Печать текста (обычный ввод символов).
    Text(String),
}

const EV_MOVE: u8 = 1;
const EV_BUTTON: u8 = 2;
const EV_SCROLL: u8 = 3;
const EV_KEY: u8 = 4;
const EV_TEXT: u8 = 5;

const MAX_TEXT_BYTES: usize = 4096;

impl InputEvent {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            InputEvent::MouseMove { x, y } => {
                w.write_all(&[EV_MOVE])?;
                w.write_all(&x.to_be_bytes())?;
                w.write_all(&y.to_be_bytes())?;
            }
            InputEvent::MouseButton { button, press, x, y } => {
                w.write_all(&[EV_BUTTON, *button, u8::from(*press)])?;
                w.write_all(&x.to_be_bytes())?;
                w.write_all(&y.to_be_bytes())?;
            }
            InputEvent::Scroll { dx, dy } => {
                w.write_all(&[EV_SCROLL])?;
                w.write_all(&dx.to_be_bytes())?;
                w.write_all(&dy.to_be_bytes())?;
            }
            InputEvent::Key { id, press } => {
                w.write_all(&[EV_KEY])?;
                w.write_all(&id.to_be_bytes())?;
                w.write_all(&[u8::from(*press)])?;
            }
            InputEvent::Text(s) => {
                let bytes = s.as_bytes();
                let len = bytes.len().min(MAX_TEXT_BYTES) as u16;
                w.write_all(&[EV_TEXT])?;
                w.write_all(&len.to_be_bytes())?;
                w.write_all(&bytes[..len as usize])?;
            }
        }
        Ok(())
    }

    pub fn read_from(r: &mut impl Read) -> io::Result<InputEvent> {
        let ev = read_u8(r)?;
        Ok(match ev {
            EV_MOVE => InputEvent::MouseMove { x: read_f32(r)?, y: read_f32(r)? },
            EV_BUTTON => {
                let button = read_u8(r)?;
                let press = read_u8(r)? != 0;
                InputEvent::MouseButton { button, press, x: read_f32(r)?, y: read_f32(r)? }
            }
            EV_SCROLL => InputEvent::Scroll { dx: read_i32(r)?, dy: read_i32(r)? },
            EV_KEY => {
                let id = read_u32(r)?;
                let press = read_u8(r)? != 0;
                InputEvent::Key { id, press }
            }
            EV_TEXT => {
                let len = read_u16(r)? as usize;
                if len > MAX_TEXT_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "text too long"));
                }
                let mut buf = vec![0u8; len];
                r.read_exact(&mut buf)?;
                InputEvent::Text(String::from_utf8_lossy(&buf).into_owned())
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown input event {other}"),
                ));
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Инжекция на управляемой машине.
// ---------------------------------------------------------------------------

/// Выполняет события ввода на локальной машине (сторона агента).
pub struct Injector {
    enigo: Enigo,
    /// Размер экрана-источника в пикселях (для перевода нормализованных координат).
    screen_w: i32,
    screen_h: i32,
    origin_x: i32,
    origin_y: i32,
}

impl Injector {
    pub fn new(screen_w: i32, screen_h: i32, origin_x: i32, origin_y: i32) -> Result<Self, String> {
        let enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("Не удалось получить доступ к вводу: {e}"))?;
        Ok(Self {
            enigo,
            screen_w: screen_w.max(1),
            screen_h: screen_h.max(1),
            origin_x,
            origin_y,
        })
    }

    fn to_px(&self, x: f32, y: f32) -> (i32, i32) {
        let px = self.origin_x + (x.clamp(0.0, 1.0) * (self.screen_w - 1) as f32).round() as i32;
        let py = self.origin_y + (y.clamp(0.0, 1.0) * (self.screen_h - 1) as f32).round() as i32;
        (px, py)
    }

    pub fn apply(&mut self, ev: &InputEvent) {
        let result = match ev {
            InputEvent::MouseMove { x, y } => {
                let (px, py) = self.to_px(*x, *y);
                self.enigo.move_mouse(px, py, Coordinate::Abs)
            }
            InputEvent::MouseButton { button, press, x, y } => {
                let (px, py) = self.to_px(*x, *y);
                let _ = self.enigo.move_mouse(px, py, Coordinate::Abs);
                let btn = match button {
                    1 => Button::Right,
                    2 => Button::Middle,
                    _ => Button::Left,
                };
                let dir = if *press { Direction::Press } else { Direction::Release };
                self.enigo.button(btn, dir)
            }
            InputEvent::Scroll { dx, dy } => {
                let mut r = Ok(());
                if *dy != 0 {
                    r = self.enigo.scroll(-*dy, Axis::Vertical);
                }
                if *dx != 0 {
                    r = r.and(self.enigo.scroll(*dx, Axis::Horizontal));
                }
                r
            }
            InputEvent::Key { id, press } => {
                let dir = if *press { Direction::Press } else { Direction::Release };
                if let Some(k) = id_to_fkey(*id).or_else(|| id_to_enigo_key(*id)) {
                    self.enigo.key(k, dir)
                } else {
                    Ok(())
                }
            }
            InputEvent::Text(s) => self.enigo.text(s),
        };
        if let Err(e) = result {
            log::warn!("input inject failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Утилиты чтения.
// ---------------------------------------------------------------------------

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}
fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}
fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn read_i32(r: &mut impl Read) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}
fn read_f32(r: &mut impl Read) -> io::Result<f32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(f32::from_be_bytes(b))
}
