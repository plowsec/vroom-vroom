//! bulwark-gui: the VM window.
//!
//! Process model:
//! ```text
//! bulwark-gui (this, unsandboxed: window, host clipboard)
//!    └── bulwark run --ui-fd N   (sandboxed VMM, owns the guest)
//! ```
//! We only get pixels (shared memory) and small messages from the VMM, and
//! send input back. The VMM is treated as untrusted.

mod keymap;

use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command as Proc};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bulwark_display::{recv_fd, Command, Decoder, SharedFb, ToViewer, ToVmm, POINTER_MAX};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(300);
const CLIP_POLL: Duration = Duration::from_millis(500);

#[derive(Debug)]
enum UserEvent {
    Frame,
    Clipboard(String),
    Status(String),
    Closed,
}

/// State shared with the socket reader thread.
#[derive(Default)]
struct Shared {
    mode: (u32, u32),
    frame_pending: bool,
}

struct Tx(UnixStream);

impl Tx {
    fn send(&mut self, m: ToVmm) {
        let _ = self.0.write_all(&m.encode());
    }
}

struct App {
    name: String,
    fb: Arc<SharedFb>,
    shared: Arc<Mutex<Shared>>,
    tx: Tx,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    pixels: Vec<u32>,
    /// Where the guest image sits in the window: (x, y, scale).
    view: (f64, f64, f64),
    mods: ModifiersState,
    focused: bool,
    pending_resize: Option<(Instant, u32, u32)>,
    clipboard: Option<arboard::Clipboard>,
    clip_seen: Option<String>,
    next_clip_poll: Instant,
    status: String,
    child: Child,
}

impl App {
    fn title(&self) -> String {
        format!("{} — Bulwark{}", self.name, if self.status.is_empty() { String::new() } else { format!(" ({})", self.status) })
    }

    fn redraw(&mut self) {
        let (Some(win), Some(surf)) = (self.window.clone(), self.surface.as_mut()) else { return };
        let size = win.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if surf.resize(w, h).is_err() {
            return;
        }
        let (mw, mh) = {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.frame_pending = false;
            s.mode
        };
        let Ok(mut buf) = surf.buffer_mut() else { return };
        buf.fill(0x0020_2020);
        if mw > 0 && mh > 0 {
            let n = mw as usize * mh as usize;
            if self.pixels.len() != n {
                self.pixels.resize(n, 0);
            }
            self.fb.read_into(mw, mh, &mut self.pixels);
            // Fit, keep aspect, center.
            let (ww, wh) = (size.width as f64, size.height as f64);
            let scale = (ww / mw as f64).min(wh / mh as f64);
            let dw = (mw as f64 * scale) as usize;
            let dh = (mh as f64 * scale) as usize;
            let ox = ((ww as usize).saturating_sub(dw)) / 2;
            let oy = ((wh as usize).saturating_sub(dh)) / 2;
            self.view = (ox as f64, oy as f64, scale);
            let stride = size.width as usize;
            if dw == mw as usize && dh == mh as usize {
                for y in 0..dh {
                    let d = (oy + y) * stride + ox;
                    buf[d..d + dw].copy_from_slice(&self.pixels[y * mw as usize..(y + 1) * mw as usize]);
                }
            } else {
                let xs: Vec<usize> = (0..dw).map(|x| ((x as f64 / scale) as usize).min(mw as usize - 1)).collect();
                for y in 0..dh {
                    let sy = ((y as f64 / scale) as usize).min(mh as usize - 1);
                    let src = &self.pixels[sy * mw as usize..(sy + 1) * mw as usize];
                    let d = (oy + y) * stride + ox;
                    for (o, &sx) in buf[d..d + dw].iter_mut().zip(&xs) {
                        *o = src[sx];
                    }
                }
            }
        }
        let _ = buf.present();
    }

    fn pointer(&mut self, p: PhysicalPosition<f64>) {
        let (mw, mh) = self.shared.lock().unwrap_or_else(|e| e.into_inner()).mode;
        let (ox, oy, scale) = self.view;
        if mw == 0 || mh == 0 || scale <= 0.0 {
            return;
        }
        let gx = ((p.x - ox) / scale).clamp(0.0, (mw - 1) as f64);
        let gy = ((p.y - oy) / scale).clamp(0.0, (mh - 1) as f64);
        let x = (gx * POINTER_MAX as f64 / (mw.max(2) - 1) as f64) as u16;
        let y = (gy * POINTER_MAX as f64 / (mh.max(2) - 1) as f64) as u16;
        self.tx.send(ToVmm::Pointer { x, y });
    }

    /// Ctrl+Alt+Shift+<key> are host shortcuts, never sent to the guest.
    fn host_shortcut(&mut self, k: KeyCode, el: &ActiveEventLoop) -> bool {
        if !(self.mods.control_key() && self.mods.alt_key() && self.mods.shift_key()) {
            return false;
        }
        let cmd = match k {
            KeyCode::KeyS => Command::Snapshot,
            KeyCode::KeyP => {
                if self.status == "paused" {
                    Command::Resume
                } else {
                    Command::Pause
                }
            }
            KeyCode::KeyR => Command::Reset,
            KeyCode::KeyD => Command::Shutdown,
            KeyCode::KeyQ => Command::PowerOff,
            KeyCode::KeyZ => Command::Suspend,
            _ => return false,
        };
        self.tx.send(ToVmm::Command(cmd));
        if matches!(cmd, Command::PowerOff | Command::Suspend) {
            el.set_control_flow(ControlFlow::Wait);
        }
        true
    }

    fn poll_clipboard(&mut self) {
        if !self.focused {
            return;
        }
        let Some(cb) = self.clipboard.as_mut() else { return };
        if let Ok(t) = cb.get_text() {
            if self.clip_seen.as_deref() != Some(t.as_str()) {
                self.clip_seen = Some(t.clone());
                self.tx.send(ToVmm::Clipboard(t));
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title(self.title()).with_inner_size(LogicalSize::new(1280.0, 800.0));
        let win = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("bulwark-gui: window: {e}");
                el.exit();
                return;
            }
        };
        let surface = softbuffer::Context::new(win.clone()).and_then(|c| softbuffer::Surface::new(&c, win.clone()));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("bulwark-gui: surface: {e}");
                el.exit();
                return;
            }
        }
        // Tell the guest our size (logical pixels: readable on HiDPI).
        let ls = win.inner_size().to_logical::<u32>(win.scale_factor());
        self.tx.send(ToVmm::Resize { width: ls.width, height: ls.height });
        self.window = Some(win);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => {
                // Closing the window suspends the VM (state kept on disk).
                self.status = "suspending".into();
                self.tx.send(ToVmm::Command(Command::Suspend));
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(sz) => {
                if let Some(w) = &self.window {
                    let ls = sz.to_logical::<u32>(w.scale_factor());
                    self.pending_resize = Some((Instant::now() + RESIZE_DEBOUNCE, ls.width, ls.height));
                    w.request_redraw();
                }
            }
            WindowEvent::Focused(f) => {
                self.focused = f;
                self.tx.send(ToVmm::Focus(f));
                if f {
                    self.clip_seen = None;
                    self.poll_clipboard();
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(k) = event.physical_key else { return };
                let down = event.state == ElementState::Pressed;
                if down && self.host_shortcut(k, el) {
                    return;
                }
                if event.repeat {
                    return; // guest does its own autorepeat
                }
                if let Some(code) = keymap::evdev(k) {
                    self.tx.send(ToVmm::Key { code, down });
                }
            }
            WindowEvent::CursorMoved { position, .. } => self.pointer(position),
            WindowEvent::MouseInput { state, button, .. } => {
                let code = match button {
                    MouseButton::Left => 0x110,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                    _ => return,
                };
                self.tx.send(ToVmm::Button { code, down: state == ElementState::Pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x.round() as i16, y.round() as i16),
                    MouseScrollDelta::PixelDelta(p) => ((p.x / 40.0) as i16, (p.y / 40.0) as i16),
                };
                if dx != 0 || dy != 0 {
                    self.tx.send(ToVmm::Wheel { dx, dy });
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Clipboard(t) => {
                if let Some(cb) = self.clipboard.as_mut() {
                    let _ = cb.set_text(t.clone());
                    self.clip_seen = Some(t);
                }
            }
            UserEvent::Status(s) => {
                self.status = s;
                if let Some(w) = &self.window {
                    w.set_title(&self.title());
                }
            }
            UserEvent::Closed => el.exit(),
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if let Some((at, w, h)) = self.pending_resize {
            if now >= at {
                self.pending_resize = None;
                self.tx.send(ToVmm::Resize { width: w, height: h });
            }
        }
        if now >= self.next_clip_poll {
            self.next_clip_poll = now + CLIP_POLL;
            self.poll_clipboard();
        }
        let mut next = self.next_clip_poll;
        if let Some((at, _, _)) = self.pending_resize {
            next = next.min(at);
        }
        el.set_control_flow(ControlFlow::WaitUntil(next));
        if let Ok(Some(_)) = self.child.try_wait() {
            el.exit();
        }
    }
}

fn reader(mut s: UnixStream, shared: Arc<Mutex<Shared>>, proxy: EventLoopProxy<UserEvent>) {
    let mut dec = Decoder::default();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        dec.push(&buf[..n]);
        loop {
            match dec.next_to_viewer() {
                Ok(Some(ToViewer::Mode { width, height })) => {
                    let ok = width == 0 || SharedFb::mode_ok(width, height);
                    if ok {
                        shared.lock().unwrap_or_else(|e| e.into_inner()).mode = (width, height);
                        let _ = proxy.send_event(UserEvent::Frame);
                    }
                }
                Ok(Some(ToViewer::Damage { .. })) => {
                    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if !sh.frame_pending {
                        sh.frame_pending = true;
                        let _ = proxy.send_event(UserEvent::Frame);
                    }
                }
                Ok(Some(ToViewer::ClipboardSet(t))) => {
                    let _ = proxy.send_event(UserEvent::Clipboard(t));
                }
                Ok(Some(ToViewer::Status(t))) => {
                    let _ = proxy.send_event(UserEvent::Status(t));
                }
                Ok(Some(ToViewer::Closed)) => {
                    let _ = proxy.send_event(UserEvent::Closed);
                    return;
                }
                Ok(None) => break,
                Err(e) => log::warn!("from vmm: {e}"),
            }
        }
    }
    let _ = proxy.send_event(UserEvent::Closed);
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let mut args = std::env::args().skip(1);
    let Some(config) = args.next() else {
        eprintln!("usage: bulwark-gui <vm.toml> [extra args for `bulwark run`]");
        std::process::exit(2);
    };
    let extra: Vec<String> = args.collect();
    if let Err(e) = run(PathBuf::from(config), extra) {
        eprintln!("bulwark-gui: {e}");
        std::process::exit(1);
    }
}

fn run(config: PathBuf, extra: Vec<String>) -> Result<(), String> {
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    // The child must inherit its end: clear close-on-exec.
    // SAFETY: fcntl on an fd we own.
    unsafe { libc::fcntl(theirs.as_raw_fd(), libc::F_SETFD, 0) };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let vmm = exe.with_file_name("bulwark");
    let name = config.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let child = Proc::new(&vmm)
        .arg("run")
        .arg(&config)
        .arg("--ui-fd")
        .arg(theirs.as_raw_fd().to_string())
        .args(&extra)
        .spawn()
        .map_err(|e| format!("start {}: {e}", vmm.display()))?;
    drop(theirs);

    let fd = recv_fd(&ours).map_err(|e| format!("vmm did not start: {e}"))?;
    let fb = Arc::new(SharedFb::from_fd(fd).map_err(|e| e.to_string())?);
    let shared = Arc::new(Mutex::new(Shared::default()));

    let el = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
    let proxy = el.create_proxy();
    let rd = ours.try_clone().map_err(|e| e.to_string())?;
    let sh = shared.clone();
    std::thread::spawn(move || reader(rd, sh, proxy));

    let mut app = App {
        name,
        fb,
        shared,
        tx: Tx(ours),
        window: None,
        surface: None,
        pixels: Vec::new(),
        view: (0.0, 0.0, 1.0),
        mods: ModifiersState::empty(),
        focused: false,
        pending_resize: None,
        clipboard: arboard::Clipboard::new().ok(),
        clip_seen: None,
        next_clip_poll: Instant::now(),
        status: String::new(),
        child,
    };
    el.run_app(&mut app).map_err(|e| e.to_string())?;
    let _ = app.child.wait();
    Ok(())
}
