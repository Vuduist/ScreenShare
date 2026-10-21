//! Сетевая часть: приём входящих трансляций и отправка своего экрана.
//!
//! Протокол поверх TCP (порт [`DEFAULT_PORT`]):
//! * рукопожатие: `b"LSS1"`, `u16` длина имени, имя отправителя (UTF-8);
//! * затем поток сообщений: `u8` тип —
//!   `1` кадр (`u32` ширина, `u32` высота, `u32` длина, JPEG),
//!   `2` пинг (без тела).
//!
//! Все числа — big-endian. Получатель ничего не отправляет.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, TextureHandle, ViewportId};

use crate::capture;

pub const DEFAULT_PORT: u16 = 7878;

const MAGIC: &[u8; 4] = b"LSS1";
const KIND_FRAME: u8 = 1;
const KIND_PING: u8 = 2;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 256;

// ---------------------------------------------------------------------------
// Общее состояние, доступное из GUI и сетевых потоков.
// ---------------------------------------------------------------------------

pub struct Shared {
    pub ctx: egui::Context,
    pub streams: Vec<IncomingStream>,
    pub outgoing: Outgoing,
    pub listen_error: Option<String>,
    next_id: u64,
}

impl Shared {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            ctx,
            streams: Vec::new(),
            outgoing: Outgoing::default(),
            listen_error: None,
            next_id: 1,
        }
    }

    pub fn stream_mut(&mut self, id: u64) -> Option<&mut IncomingStream> {
        self.streams.iter_mut().find(|s| s.id == id)
    }
}

pub type SharedState = Arc<Mutex<Shared>>;

/// Берёт блокировку, не падая при «отравленном» мьютексе.
pub fn lock(shared: &SharedState) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Состояние исходящей трансляции (мы показываем свой экран).
#[derive(Clone, Debug, Default)]
pub struct Outgoing {
    pub running: bool,
    pub status: String,
    pub error: Option<String>,
    pub fps: f32,
    pub kbps: f32,
}

/// Входящая трансляция (нам показывают чужой экран).
pub struct IncomingStream {
    pub id: u64,
    pub name: String,
    pub peer: SocketAddr,
    pub pending: Option<ColorImage>,
    pub texture: Option<TextureHandle>,
    pub size: [usize; 2],
    pub hidden: bool,
    pub finished: bool,
    pub frames: u64,
    pub started: Instant,
}

impl IncomingStream {
    pub fn viewport_id(&self) -> ViewportId {
        ViewportId::from_hash_of(("screenshare-stream", self.id))
    }
}

// ---------------------------------------------------------------------------
// Приём.
// ---------------------------------------------------------------------------

/// Запускает фоновый поток, принимающий входящие подключения.
pub fn start_listener(shared: SharedState, port: u16) {
    thread::Builder::new()
        .name("listener".into())
        .spawn(move || match TcpListener::bind(("0.0.0.0", port)) {
            Err(e) => {
                log::error!("bind 0.0.0.0:{port}: {e}");
                let mut sh = lock(&shared);
                sh.listen_error = Some(format!(
                    "Не удалось открыть порт {port}: {e}. Входящие трансляции недоступны."
                ));
                sh.ctx.request_repaint();
            }
            Ok(listener) => {
                log::info!("listening on 0.0.0.0:{port}");
                for conn in listener.incoming() {
                    match conn {
                        Ok(stream) => {
                            let shared = shared.clone();
                            let _ = thread::Builder::new()
                                .name("incoming".into())
                                .spawn(move || handle_incoming(shared, stream));
                        }
                        Err(e) => log::warn!("accept: {e}"),
                    }
                }
            }
        })
        .expect("spawn listener thread");
}

fn handle_incoming(shared: SharedState, mut stream: TcpStream) {
    let peer = match stream.peer_addr() {
        Ok(p) => p,
        Err(_) => return,
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));

    let name = match read_handshake(&mut stream) {
        Ok(name) => name,
        Err(e) => {
            log::warn!("{peer}: bad handshake: {e}");
            return;
        }
    };
    log::info!("{peer}: incoming stream from {name:?}");

    let id = {
        let mut sh = lock(&shared);
        let id = sh.next_id;
        sh.next_id += 1;
        sh.streams.push(IncomingStream {
            id,
            name,
            peer,
            pending: None,
            texture: None,
            size: [0, 0],
            hidden: false,
            finished: false,
            frames: 0,
            started: Instant::now(),
        });
        sh.ctx.request_repaint();
        id
    };

    if let Err(e) = stream_loop(&shared, id, &mut stream) {
        log::info!("{peer}: stream ended: {e}");
    }

    let mut sh = lock(&shared);
    if let Some(s) = sh.stream_mut(id) {
        s.finished = true;
    }
    sh.ctx.request_repaint();
}

fn read_handshake(stream: &mut TcpStream) -> io::Result<String> {
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
    }
    let len = read_u16(stream)? as usize;
    if len > MAX_NAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "name too long"));
    }
    let mut name = vec![0u8; len];
    stream.read_exact(&mut name)?;
    let name = String::from_utf8_lossy(&name).trim().to_string();
    Ok(if name.is_empty() { "Без имени".into() } else { name })
}

fn stream_loop(shared: &SharedState, id: u64, stream: &mut TcpStream) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let kind = read_u8(stream)?;
        match kind {
            KIND_PING => {}
            KIND_FRAME => {
                let _w = read_u32(stream)?;
                let _h = read_u32(stream)?;
                let len = read_u32(stream)? as usize;
                if len > MAX_FRAME_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
                }
                buf.resize(len, 0);
                stream.read_exact(&mut buf)?;

                match capture::decode_jpeg(&buf) {
                    Ok((pixels, w, h)) => {
                        let image = ColorImage::from_rgba_unmultiplied([w, h], &pixels);
                        let mut sh = lock(shared);
                        let ctx = sh.ctx.clone();
                        let Some(s) = sh.stream_mut(id) else {
                            return Ok(());
                        };
                        s.pending = Some(image);
                        s.size = [w, h];
                        s.frames += 1;
                        let viewport = s.viewport_id();
                        let window_exists = s.texture.is_some();
                        let hidden = s.hidden;
                        drop(sh);
                        if !hidden {
                            if window_exists {
                                ctx.request_repaint_of(viewport);
                            } else {
                                // Окно ещё не создано — его создаст главное окно.
                                ctx.request_repaint();
                            }
                        }
                    }
                    Err(e) => log::warn!("frame decode failed: {e}"),
                }
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown message kind {other}"),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Отправка.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SendConfig {
    pub target: SocketAddr,
    pub monitor_id: u32,
    pub fps: u32,
    pub quality: u8,
    pub name: String,
}

pub struct SenderHandle {
    stop: Arc<AtomicBool>,
}

impl SenderHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for SenderHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Запускает поток, который захватывает экран и шлёт его на `cfg.target`.
pub fn start_sender(shared: SharedState, cfg: SendConfig) -> SenderHandle {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut sh = lock(&shared);
        sh.outgoing = Outgoing {
            running: true,
            status: format!("Подключение к {}…", cfg.target),
            ..Default::default()
        };
    }
    let stop_flag = stop.clone();
    thread::Builder::new()
        .name("sender".into())
        .spawn(move || {
            let result = sender_loop(&shared, &cfg, &stop_flag);
            let mut sh = lock(&shared);
            sh.outgoing.running = false;
            sh.outgoing.fps = 0.0;
            sh.outgoing.kbps = 0.0;
            match result {
                Ok(()) => sh.outgoing.status = "Показ остановлен".into(),
                Err(e) => {
                    log::warn!("sender: {e}");
                    sh.outgoing.status.clear();
                    sh.outgoing.error = Some(e);
                }
            }
            sh.ctx.request_repaint();
        })
        .expect("spawn sender thread");
    SenderHandle { stop }
}

fn sender_loop(shared: &SharedState, cfg: &SendConfig, stop: &AtomicBool) -> Result<(), String> {
    let monitor = capture::find_monitor(cfg.monitor_id)
        .ok_or_else(|| "Выбранный экран не найден. Обновите список экранов.".to_string())?;

    let mut stream = TcpStream::connect_timeout(&cfg.target, Duration::from_secs(5)).map_err(|e| {
        format!(
            "Не удалось подключиться к {}: {}. Проверьте, что у коллеги запущен ScreenShare.",
            cfg.target, e
        )
    })?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let broken = |e: io::Error| format!("Соединение с {} разорвано: {}", cfg.target, e);

    // Рукопожатие.
    let name = cfg.name.as_bytes();
    let name = &name[..name.len().min(MAX_NAME_BYTES)];
    let mut hello = Vec::with_capacity(6 + name.len());
    hello.extend_from_slice(MAGIC);
    hello.extend_from_slice(&(name.len() as u16).to_be_bytes());
    hello.extend_from_slice(name);
    stream.write_all(&hello).map_err(broken)?;

    {
        let mut sh = lock(shared);
        sh.outgoing.status = format!("Идёт показ → {}", cfg.target);
        sh.ctx.request_repaint();
    }

    let interval = Duration::from_secs_f32(1.0 / cfg.fps.clamp(1, 60) as f32);
    let mut prev_raw: Option<Vec<u8>> = None;
    let mut last_write = Instant::now();
    let mut stats_since = Instant::now();
    let mut stats_frames = 0u32;
    let mut stats_bytes = 0u64;
    let mut header = [0u8; 13];

    while !stop.load(Ordering::Relaxed) {
        let tick = Instant::now();

        let image = monitor
            .capture_image()
            .map_err(|e| format!("Ошибка захвата экрана: {e}"))?;
        let (w, h) = (image.width(), image.height());
        let raw = image.into_raw();

        let changed = prev_raw.as_ref().is_none_or(|p| *p != raw);
        if changed {
            let jpeg = capture::encode_jpeg(&raw, w, h, cfg.quality)?;
            header[0] = KIND_FRAME;
            header[1..5].copy_from_slice(&w.to_be_bytes());
            header[5..9].copy_from_slice(&h.to_be_bytes());
            header[9..13].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
            stream.write_all(&header).map_err(broken)?;
            stream.write_all(&jpeg).map_err(broken)?;
            prev_raw = Some(raw);
            last_write = Instant::now();
            stats_frames += 1;
            stats_bytes += jpeg.len() as u64 + header.len() as u64;
        } else if last_write.elapsed() >= Duration::from_secs(1) {
            stream.write_all(&[KIND_PING]).map_err(broken)?;
            last_write = Instant::now();
        }

        let since = stats_since.elapsed();
        if since >= Duration::from_secs(1) {
            let secs = since.as_secs_f32();
            let mut sh = lock(shared);
            sh.outgoing.fps = stats_frames as f32 / secs;
            sh.outgoing.kbps = stats_bytes as f32 / secs / 1024.0;
            sh.ctx.request_repaint();
            stats_since = Instant::now();
            stats_frames = 0;
            stats_bytes = 0;
        }

        let spent = tick.elapsed();
        if spent < interval {
            thread::sleep(interval - spent);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Утилиты.
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

/// IPv4-адреса этого компьютера (без loopback и link-local), частные сети первыми.
pub fn local_ips() -> Vec<String> {
    let mut ips: Vec<std::net::Ipv4Addr> = local_ip_address::list_afinet_netifas()
        .map(|list| {
            list.into_iter()
                .filter_map(|(_, ip)| match ip {
                    std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => Some(v4),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    ips.sort_by_key(|ip| (!ip.is_private(), *ip));
    ips.dedup();
    ips.iter().map(ToString::to_string).collect()
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "PC".to_string())
}
