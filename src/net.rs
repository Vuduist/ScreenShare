//! Сетевая часть: показ экрана и удалённое управление по локальной сети.
//!
//! Протокол поверх TCP (порт [`DEFAULT_PORT`]), все числа big-endian.
//!
//! Рукопожатие от инициатора: `b"LSS3"`, `u8` режим, `u16`+имя инициатора.
//! * Режим `1` (показ): инициатор отдаёт свой экран принимающей стороне (только картинка).
//! * Режим `2` (доступ): инициатор ОТДАЁТ управление своим рабочим столом принимающей
//!   стороне. То есть за компьютером сидит тот, кто хочет, чтобы им управляли: он сам
//!   вписывает IP управляющего и жмёт «Дать доступ». Пароль и подтверждения не нужны —
//!   само это действие и есть разрешение. После рукопожатия инициатор отдаёт свой экран
//!   и выполняет приходящие события ввода.
//!
//! Поток сообщений (`u8` тип):
//! * `1` кадр (`u32` ширина, `u32` высота, `u32` длина, JPEG) — от источника экрана;
//! * `2` пинг (без тела);
//! * `3` событие ввода (см. [`crate::control`]) — от управляющего к управляемому.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, TextureHandle, ViewportId};
use xcap::Monitor;

use crate::capture;
use crate::control::{InputEvent, Injector};

pub const DEFAULT_PORT: u16 = 7878;

const MAGIC: &[u8; 4] = b"LSS3";
const MODE_SHARE: u8 = 1;
const MODE_GRANT: u8 = 2;

const KIND_FRAME: u8 = 1;
const KIND_PING: u8 = 2;
const KIND_INPUT: u8 = 3;

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 256;

const CONTROL_FPS: u32 = 15;
const CONTROL_QUALITY: u8 = 70;

// ---------------------------------------------------------------------------
// Общее состояние, доступное из GUI и сетевых потоков.
// ---------------------------------------------------------------------------

pub struct Shared {
    pub ctx: egui::Context,
    pub streams: Vec<IncomingStream>,
    pub outgoing: Outgoing,
    pub grant: GrantStatus,
    pub controls: Vec<ControlSession>,
    pub listen_error: Option<String>,
    next_id: u64,
}

impl Shared {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            ctx,
            streams: Vec::new(),
            outgoing: Outgoing::default(),
            grant: GrantStatus::default(),
            controls: Vec::new(),
            listen_error: None,
            next_id: 1,
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
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

/// Состояние нашего исходящего показа экрана (режим показа).
#[derive(Clone, Debug, Default)]
pub struct Outgoing {
    pub running: bool,
    pub status: String,
    pub error: Option<String>,
    pub fps: f32,
    pub kbps: f32,
}

/// Состояние нашей раздачи доступа: мы отдаём управление своим рабочим столом.
#[derive(Clone, Debug, Default)]
pub struct GrantStatus {
    pub active: bool,
    pub status: String,
    pub error: Option<String>,
}

/// Окно, показывающее чужой экран. Если `controllable` — мы ещё и управляем им
/// (нам отдали доступ), и тогда есть канал `input_tx` для отправки ввода.
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
    pub controllable: bool,
    pub input_tx: Option<Sender<InputEvent>>,
    /// Состояние захвата ввода (сторона управляющего).
    pub prev_mods: egui::Modifiers,
    pub last_sent_pos: Option<(f32, f32)>,
}

impl IncomingStream {
    pub fn viewport_id(&self) -> ViewportId {
        ViewportId::from_hash_of(("screenshare-stream", self.id))
    }
}

/// Активная сессия, в которой МЫ отдаём управление своим столом (для индикатора).
pub struct ControlSession {
    pub id: u64,
    pub controller: String,
    pub peer: SocketAddr,
    pub started: Instant,
    stop: Arc<AtomicBool>,
}

impl ControlSession {
    /// Прекратить отдачу управления.
    pub fn revoke(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Приём входящих подключений (сторона управляющего и сторона зрителя).
// ---------------------------------------------------------------------------

pub fn start_listener(shared: SharedState, port: u16) {
    thread::Builder::new()
        .name("listener".into())
        .spawn(move || match TcpListener::bind(("0.0.0.0", port)) {
            Err(e) => {
                log::error!("bind 0.0.0.0:{port}: {e}");
                let mut sh = lock(&shared);
                sh.listen_error = Some(format!(
                    "Не удалось открыть порт {port}: {e}. Входящие подключения недоступны."
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
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));

    let (mode, name) = match read_handshake(&mut stream) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("{peer}: bad handshake: {e}");
            return;
        }
    };

    match mode {
        MODE_SHARE => {
            log::info!("{peer}: показ экрана от {name:?}");
            let id = register_stream(&shared, name, peer, false, None);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            if let Err(e) = recv_frame_loop(&shared, id, &mut stream) {
                log::info!("{peer}: показ завершён: {e}");
            }
            finish_stream(&shared, id);
        }
        MODE_GRANT => {
            log::info!("{peer}: {name:?} даёт нам управление своим столом");
            if let Err(e) = receive_control(&shared, &mut stream, name, peer) {
                log::info!("{peer}: управление завершено: {e}");
            }
        }
        other => log::warn!("{peer}: неизвестный режим {other}"),
    }
}

fn read_handshake(stream: &mut TcpStream) -> io::Result<(u8, String)> {
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
    }
    let mode = read_u8(stream)?;
    let name = read_string(stream, MAX_NAME_BYTES)?;
    let name = if name.trim().is_empty() {
        "Без имени".to_string()
    } else {
        name.trim().to_string()
    };
    Ok((mode, name))
}

/// Сторона управляющего: нам отдали доступ — показываем окно и шлём ввод.
fn receive_control(
    shared: &SharedState,
    stream: &mut TcpStream,
    name: String,
    peer: SocketAddr,
) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<InputEvent>();
    let id = register_stream(shared, name, peer, true, Some(tx));

    // Поток записи шлёт ввод, этот поток читает кадры.
    let mut write_stream = stream.try_clone()?;
    let writer = thread::Builder::new()
        .name("control-input".into())
        .spawn(move || input_writer_loop(&mut write_stream, rx));

    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let result = recv_frame_loop(shared, id, stream);

    let _ = stream.shutdown(Shutdown::Both);
    finish_stream(shared, id);
    if let Ok(handle) = writer {
        let _ = handle.join();
    }
    result
}

// ---------------------------------------------------------------------------
// Показ своего экрана (режим показа, инициатор-отправитель).
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

    write_hello(&mut stream, MODE_SHARE, &cfg.name).map_err(broken)?;

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
            write_frame_header(&mut header, w, h, jpeg.len());
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
// Отдача доступа: мы даём управляющему управлять нашим рабочим столом.
// Инициатор здесь — сам управляемый (за ним сидит человек, который делится).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GrantConfig {
    pub target: SocketAddr,
    pub name: String,
}

/// Подключается к управляющему и начинает отдавать ему управление нашим столом.
pub fn start_grant(shared: SharedState, cfg: GrantConfig) {
    {
        let mut sh = lock(&shared);
        sh.grant = GrantStatus {
            active: true,
            status: format!("Подключение к {}…", cfg.target),
            error: None,
        };
        sh.ctx.request_repaint();
    }
    thread::Builder::new()
        .name("grant".into())
        .spawn(move || {
            let result = grant_session(&shared, &cfg);
            let mut sh = lock(&shared);
            sh.grant.active = false;
            match result {
                Ok(()) => sh.grant.status = "Доступ закрыт".into(),
                Err(e) => {
                    log::warn!("grant: {e}");
                    sh.grant.status.clear();
                    sh.grant.error = Some(e);
                }
            }
            sh.ctx.request_repaint();
        })
        .expect("spawn grant thread");
}

fn grant_session(shared: &SharedState, cfg: &GrantConfig) -> Result<(), String> {
    let mut stream = TcpStream::connect_timeout(&cfg.target, Duration::from_secs(5)).map_err(|e| {
        format!(
            "Не удалось подключиться к {}: {}. Проверьте, что у управляющего запущен ScreenShare.",
            cfg.target, e
        )
    })?;
    let _ = stream.set_nodelay(true);
    write_hello(&mut stream, MODE_GRANT, &cfg.name).map_err(|e| format!("Соединение разорвано: {e}"))?;

    let monitor = primary_monitor().ok_or_else(|| "Нет доступного экрана.".to_string())?;
    let (mw, mh) = (
        monitor.width().unwrap_or(1) as i32,
        monitor.height().unwrap_or(1) as i32,
    );
    let (ox, oy) = (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0));

    let stop = Arc::new(AtomicBool::new(false));
    let session_id = {
        let mut sh = lock(shared);
        let id = sh.next_id();
        sh.controls.push(ControlSession {
            id,
            controller: cfg.target.to_string(),
            peer: cfg.target,
            started: Instant::now(),
            stop: stop.clone(),
        });
        sh.grant.status = format!("Доступ открыт → {}", cfg.target);
        sh.ctx.request_repaint();
        id
    };

    // Поток отдачи кадров пишет в клон сокета, этот поток читает и выполняет ввод.
    // Монитор xcap не `Send`, поэтому поток берёт его сам.
    let mut frame_stream = stream.try_clone().map_err(|e| format!("Внутренняя ошибка сокета: {e}"))?;
    let cap_stop = stop.clone();
    let capture_thread = thread::Builder::new().name("grant-frames".into()).spawn(move || {
        if let Err(e) = control_capture_loop(&mut frame_stream, &cap_stop) {
            log::info!("grant frames stopped: {e}");
        }
        cap_stop.store(true, Ordering::Relaxed);
    });

    let mut injector = match Injector::new(mw, mh, ox, oy) {
        Ok(i) => Some(i),
        Err(e) => {
            log::error!("{}: {e}", cfg.target);
            None
        }
    };

    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match read_u8(&mut stream) {
            Ok(KIND_PING) => {}
            Ok(KIND_INPUT) => match InputEvent::read_from(&mut stream) {
                Ok(ev) => {
                    if let Some(inj) = injector.as_mut() {
                        inj.apply(&ev);
                    }
                }
                Err(e) => break Err(format!("Соединение разорвано: {e}")),
            },
            Ok(other) => break Err(format!("Неизвестное сообщение {other}")),
            Err(e) if is_timeout(&e) => continue,
            Err(e) => break Err(format!("Соединение разорвано: {e}")),
        }
    };

    stop.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(Shutdown::Both);
    if let Ok(handle) = capture_thread {
        let _ = handle.join();
    }
    {
        let mut sh = lock(shared);
        sh.controls.retain(|c| c.id != session_id);
        sh.ctx.request_repaint();
    }
    result
}

fn control_capture_loop(stream: &mut TcpStream, stop: &AtomicBool) -> io::Result<()> {
    let monitor = primary_monitor()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "нет доступного экрана"))?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let interval = Duration::from_secs_f32(1.0 / CONTROL_FPS as f32);
    let mut prev: Option<Vec<u8>> = None;
    let mut last = Instant::now();
    let mut header = [0u8; 13];

    while !stop.load(Ordering::Relaxed) {
        let tick = Instant::now();
        let image = monitor
            .capture_image()
            .map_err(|e| io::Error::other(format!("capture: {e}")))?;
        let (w, h) = (image.width(), image.height());
        let raw = image.into_raw();

        let changed = prev.as_ref().is_none_or(|p| *p != raw);
        if changed {
            let jpeg = capture::encode_jpeg(&raw, w, h, CONTROL_QUALITY).map_err(io::Error::other)?;
            write_frame_header(&mut header, w, h, jpeg.len());
            stream.write_all(&header)?;
            stream.write_all(&jpeg)?;
            prev = Some(raw);
            last = Instant::now();
        } else if last.elapsed() >= Duration::from_secs(1) {
            stream.write_all(&[KIND_PING])?;
            last = Instant::now();
        }

        let spent = tick.elapsed();
        if spent < interval {
            thread::sleep(interval - spent);
        }
    }
    Ok(())
}

fn input_writer_loop(stream: &mut TcpStream, rx: Receiver<InputEvent>) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ev) => {
                if stream.write_all(&[KIND_INPUT]).is_err() || ev.write_to(stream).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stream.write_all(&[KIND_PING]).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

// ---------------------------------------------------------------------------
// Общий приём кадров (и для показа, и для управляемого потока).
// ---------------------------------------------------------------------------

fn recv_frame_loop(shared: &SharedState, id: u64, stream: &mut TcpStream) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let kind = match read_u8(stream) {
            Ok(k) => k,
            Err(e) if is_timeout(&e) => {
                if lock(shared).streams.iter().all(|s| s.id != id || s.finished) {
                    return Ok(());
                }
                continue;
            }
            Err(e) => return Err(e),
        };
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

fn register_stream(
    shared: &SharedState,
    name: String,
    peer: SocketAddr,
    controllable: bool,
    input_tx: Option<Sender<InputEvent>>,
) -> u64 {
    let mut sh = lock(shared);
    let id = sh.next_id();
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
        controllable,
        input_tx,
        prev_mods: egui::Modifiers::default(),
        last_sent_pos: None,
    });
    sh.ctx.request_repaint();
    id
}

fn finish_stream(shared: &SharedState, id: u64) {
    let mut sh = lock(shared);
    if let Some(s) = sh.stream_mut(id) {
        s.finished = true;
        s.input_tx = None;
    }
    sh.ctx.request_repaint();
}

// ---------------------------------------------------------------------------
// Протокольные помощники.
// ---------------------------------------------------------------------------

fn write_hello(stream: &mut TcpStream, mode: u8, name: &str) -> io::Result<()> {
    let mut hello = Vec::with_capacity(64);
    hello.extend_from_slice(MAGIC);
    hello.push(mode);
    write_string(&mut hello, name);
    stream.write_all(&hello)
}

fn write_frame_header(header: &mut [u8; 13], w: u32, h: u32, len: usize) {
    header[0] = KIND_FRAME;
    header[1..5].copy_from_slice(&w.to_be_bytes());
    header[5..9].copy_from_slice(&h.to_be_bytes());
    header[9..13].copy_from_slice(&(len as u32).to_be_bytes());
}

fn primary_monitor() -> Option<Monitor> {
    let monitors = Monitor::all().ok()?;
    monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .cloned()
        .or_else(|| monitors.into_iter().next())
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

// ---------------------------------------------------------------------------
// Утилиты.
// ---------------------------------------------------------------------------

fn write_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(MAX_NAME_BYTES);
    buf.extend_from_slice(&(len as u16).to_be_bytes());
    buf.extend_from_slice(&bytes[..len]);
}

fn read_string(r: &mut impl Read, max: usize) -> io::Result<String> {
    let len = read_u16(r)? as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "string too long"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

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
