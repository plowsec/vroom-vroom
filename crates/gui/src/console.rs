//! A VM window: starts `vroom run --ui-fd`, shows its screen, sends input.
//!
//! ```text
//! vroom-gui (this: window, host clipboard; never touches guest memory)
//!    └── vroom run --ui-fd N   (sandboxed VMM, owns the guest)
//! ```
//! We get pixels (shared memory) and small messages from the VMM and send
//! input back. The VMM is treated as untrusted: its messages are size
//! checked by the decoder, its strings are cleaned before display, and the
//! clipboard rules from the VM file are enforced here too.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command as Proc, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use vroom_config::{GuestOs, VmConfig};
use vroom_display::{recv_fd, Command, Decoder, SharedFb, ToViewer, ToVmm, MAX_HEIGHT, MAX_WIDTH, POINTER_MAX};

use crate::msg::{self, Msg};
use crate::{ConsoleWindow, VmState};

const CLIP_POLL: Duration = Duration::from_millis(500);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(300);
const THUMB_EVERY: Duration = Duration::from_secs(2);
const THUMB_SAVE_EVERY: Duration = Duration::from_secs(60);
/// Scroll distance (logical px) per wheel click sent to the guest.
const WHEEL_STEP: f32 = 30.0;

/// Status lines waiting for the UI thread; more are dropped.
const MAX_STATUS: usize = 16;
/// Status lines are short; snapshot ones hold a path.
const MAX_STATUS_LEN: usize = 4096;
/// How much of the end of a VMM log we read for an error message.
const LOG_TAIL: u64 = 16 << 10;

/// What the socket reader thread hands the UI thread. Bounded whatever the
/// VMM sends: frames and the clipboard keep only the latest, status lines
/// past a few are dropped, and only one wakeup is ever in flight.
#[derive(Default)]
struct Shared {
    mode: (u32, u32),
    frame: bool,
    /// The whole picture changed (new mode).
    full: bool,
    /// What changed since the UI last looked: x0, y0, x1, y1.
    damage: Option<[u32; 4]>,
    status: VecDeque<String>,
    clipboard: Option<String>,
    /// The guest records from the microphone (latest word from the VMM).
    mic: Option<bool>,
    posted: bool,
}

impl Shared {
    /// Take one message in. True if the UI thread needs waking up.
    fn push(&mut self, m: ToViewer) -> bool {
        match m {
            ToViewer::Mode { width, height } => {
                if width != 0 && !SharedFb::mode_ok(width, height) {
                    return false;
                }
                self.mode = (width, height);
                self.frame = true;
                self.full = true;
            }
            ToViewer::Damage { x, y, w, h } => {
                // Clipped to the mode when used.
                let r = [x, y, x.saturating_add(w), y.saturating_add(h)];
                self.damage = Some(self.damage.map_or(r, |d| [d[0].min(r[0]), d[1].min(r[1]), d[2].max(r[2]), d[3].max(r[3])]));
                self.frame = true;
            }
            ToViewer::ClipboardSet(t) => self.clipboard = Some(t),
            ToViewer::Status(mut t) => {
                if self.status.len() >= MAX_STATUS {
                    return false;
                }
                if t.len() > MAX_STATUS_LEN {
                    let mut end = MAX_STATUS_LEN;
                    while !t.is_char_boundary(end) {
                        end -= 1;
                    }
                    t.truncate(end);
                }
                self.status.push_back(t);
            }
            ToViewer::Mic(on) => self.mic = Some(on),
            ToViewer::Closed => return false,
        }
        !std::mem::replace(&mut self.posted, true)
    }
}

/// Input side: everything the window callbacks need, without the rest.
struct Io {
    tx: Option<UnixStream>,
    mods: ModifiersState,
    /// Keys and mouse buttons the guest thinks are down (released if we
    /// lose focus).
    down: BTreeSet<u16>,
    buttons: BTreeSet<u16>,
    wheel: (f32, f32),
    focused: bool,
}

impl Io {
    fn send(&mut self, m: ToVmm) {
        if let Some(t) = &mut self.tx {
            let _ = t.write_all(&m.encode());
        }
    }

    fn key(&mut self, code: u16, down: bool) {
        if down {
            self.down.insert(code);
        } else {
            self.down.remove(&code);
        }
        self.send(ToVmm::Key { code, down });
    }

    fn button(&mut self, code: u16, down: bool) {
        if down {
            self.buttons.insert(code);
        } else {
            self.buttons.remove(&code);
        }
        self.send(ToVmm::Button { code, down });
    }

    fn release_all(&mut self) {
        for code in std::mem::take(&mut self.down) {
            self.send(ToVmm::Key { code, down: false });
        }
        for code in std::mem::take(&mut self.buttons) {
            self.send(ToVmm::Button { code, down: false });
        }
    }
}

pub struct Console {
    pub key: String,
    pub state: VmState,
    name: String,
    win: ConsoleWindow,
    io: Rc<RefCell<Io>>,
    fb: Option<SharedFb>,
    shared: Arc<Mutex<Shared>>,
    child: Child,
    /// The guest screen as shown (RGBA), updated in place where it changed.
    img: Option<SharedPixelBuffer<Rgba8Pixel>>,
    mode: (u32, u32),
    last_thumb: Instant,
    last_save: Instant,
    clip_policy: vroom_config::Clipboard,
    clipboard: Option<arboard::Clipboard>,
    clip_seen: Option<String>,
    pending_resize: Option<Instant>,
    /// `[display] hidpi`: give the guest physical pixels.
    hidpi: bool,
    toast_id: u64,
    ever_ran: bool,
    gone: Arc<AtomicBool>,
}

thread_local! {
    static CONSOLES: RefCell<HashMap<String, Console>> = RefCell::new(HashMap::new());
    static TIMER: slint::Timer = slint::Timer::default();
}

fn with<R>(key: &str, f: impl FnOnce(&mut Console) -> R) -> Option<R> {
    CONSOLES.with(|c| c.borrow_mut().get_mut(key).map(f))
}

pub fn is_running(key: &str) -> bool {
    CONSOLES.with(|c| c.borrow().contains_key(key))
}

pub fn count() -> usize {
    CONSOLES.with(|c| c.borrow().len())
}

pub fn state(key: &str) -> Option<VmState> {
    with(key, |c| c.state)
}

/// Bring a VM's window to the front.
pub fn show(key: &str) {
    with(key, |c| {
        let _ = c.win.show();
        c.win.window().with_winit_window(|w| w.focus_window());
    });
}

/// The VM file's clipboard rules changed: apply them now (we enforce them).
pub fn set_clip_policy(key: &str, p: vroom_config::Clipboard) {
    with(key, |c| c.clip_policy = p);
}

/// `[display] hidpi` changed: ask the guest for the new size now.
pub fn set_hidpi(key: &str, on: bool) {
    with(key, |c| {
        if c.hidpi != on {
            c.hidpi = on;
            c.pending_resize = Some(Instant::now());
        }
    });
}

/// What to ask the guest for, for a window of `w` x `h` physical pixels
/// at scale factor `sf`: (width, height, scale percent), see
/// `ToVmm::Resize`. HiDPI: physical pixels, shrunk to fit the shared
/// framebuffer if need be (the scale shrinks with them so the guest's UI
/// keeps its size). Off: logical pixels, the guest's scale left alone.
fn guest_size(w: u32, h: u32, sf: f32, hidpi: bool) -> (u32, u32, u32) {
    let sf = if sf.is_finite() { sf.clamp(0.5, 5.0) } else { 1.0 };
    let (w, h) = (w.max(1), h.max(1));
    if !hidpi {
        return (((w as f32 / sf) as u32).max(1), ((h as f32 / sf) as u32).max(1), 0);
    }
    let k = (w as f32 / MAX_WIDTH as f32).max(h as f32 / MAX_HEIGHT as f32).max(1.0);
    let (gw, gh) = (((w as f32 / k) as u32).clamp(1, MAX_WIDTH), ((h as f32 / k) as u32).clamp(1, MAX_HEIGHT));
    let scale = ((sf / k * 100.0).round() as u32).clamp(vroom_display::SCALE_MIN, vroom_display::SCALE_MAX);
    (gw, gh, scale)
}

pub fn command(key: &str, cmd: &str) {
    with(key, |c| c.command(cmd));
}

/// A picture of some VM window (for `VROOM_SHOT`).
pub fn snapshot_any() -> Option<Result<SharedPixelBuffer<Rgba8Pixel>, slint::PlatformError>> {
    CONSOLES.with(|c| c.borrow().values().next().map(|c| c.win.window().take_snapshot()))
}

/// Ask every running VM to suspend (app quitting).
pub fn suspend_all() {
    CONSOLES.with(|c| {
        for con in c.borrow_mut().values_mut() {
            con.command("suspend");
        }
    });
}

/// VMM text for display: at most 160 chars, no control characters and no
/// invisible or direction-changing ones (they can make it read as
/// something else).
fn clean(s: &str) -> String {
    let hidden = |c: char| matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}');
    s.chars().filter(|c| !c.is_control() && !hidden(*c)).take(160).collect()
}

/// The last `lines` non-empty lines of a log. Only its end is read: the
/// VMM writes it and can make it any size.
fn tail(p: &Path, lines: usize) -> String {
    let mut b = Vec::new();
    if let Ok(mut f) = std::fs::File::open(p) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL))).is_ok() {
            let _ = f.take(LOG_TAIL).read_to_end(&mut b);
        }
    }
    let s = String::from_utf8_lossy(&b);
    let v: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

/// The state a status line from the VMM puts the VM in.
fn status_state(t: &str) -> Option<VmState> {
    if t == "running" {
        Some(VmState::Running)
    } else if t == "paused" || t.starts_with("host disk almost full") {
        Some(VmState::Paused)
    } else if t.starts_with("suspend failed") {
        // The VMM paused the guest to save it and stays up.
        Some(VmState::Paused)
    } else {
        None
    }
}

/// The snapshot a "snapshot: <dir>" status reports, if its name is one we
/// can use in a file name.
fn snapshot_name(raw: &str) -> Option<String> {
    let dir = raw.strip_prefix("snapshot: ")?;
    let n = Path::new(dir.trim_end()).file_name()?.to_str()?;
    vroom_config::valid_name(n).then(|| n.to_string())
}

/// A VMM whose window couldn't be set up: stop it, reap it elsewhere.
fn abandon(mut child: Child, e: String) -> String {
    let _ = child.kill();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    e
}

/// Start a VM in a new window. `extra` goes to `vroom run` (`--fresh`,
/// `--restore DIR`).
pub fn start(path: &Path, cfg: &VmConfig, extra: &[String]) -> Result<String, String> {
    let key = crate::paths::key(path);
    if is_running(&key) {
        show(&key);
        return Ok(key);
    }
    let log = crate::paths::log_file(&key);
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    // Keep the last run only: logs are for "why did it stop".
    let logf = std::fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    let os = cfg.guest_os();
    let win = ConsoleWindow::new().map_err(|e| e.to_string())?;
    win.set_vm_name(cfg.name.clone().into());
    win.set_os(crate::os_id(os).into());
    win.set_state(VmState::Starting);
    win.set_has_display(cfg.display.gpu != vroom_config::Gpu::None);
    win.set_windows_guest(os == GuestOs::Windows);
    win.set_bar_pinned(true);
    let io = Rc::new(RefCell::new(Io {
        tx: ours.try_clone().ok(),
        mods: ModifiersState::empty(),
        down: BTreeSet::new(),
        buttons: BTreeSet::new(),
        wheel: (0.0, 0.0),
        focused: false,
    }));
    wire(&win, &key, &io);

    // The child must inherit its end: clear close-on-exec.
    // SAFETY: fcntl on an fd we own.
    #[allow(unsafe_code)]
    unsafe {
        libc::fcntl(theirs.as_raw_fd(), libc::F_SETFD, 0)
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let vmm = exe.with_file_name("vroom");
    let child = Proc::new(&vmm)
        .arg("run")
        .arg(path)
        .arg("--ui-fd")
        .arg(theirs.as_raw_fd().to_string())
        .args(extra)
        .stdin(Stdio::null())
        .stdout(logf.try_clone().map_err(|e| e.to_string())?)
        .stderr(logf)
        .spawn()
        .map_err(|e| format!("start {}: {e}", vmm.display()))?;
    drop(theirs);

    if let Err(e) = win.show() {
        return Err(abandon(child, e.to_string()));
    }
    // Keep the toolbar out for a few seconds so people see it exists.
    {
        let k = key.clone();
        slint::Timer::single_shot(Duration::from_secs(4), move || {
            with(&k, |c| c.win.set_bar_pinned(false));
        });
    }

    let shared = Arc::new(Mutex::new(Shared::default()));
    let gone = Arc::new(AtomicBool::new(false));
    {
        let (key, shared, gone) = (key.clone(), shared.clone(), gone.clone());
        let r = std::thread::Builder::new().name("vmm-reader".into()).spawn(move || reader(ours, key, shared, gone));
        if let Err(e) = r {
            let _ = win.hide();
            return Err(abandon(child, e.to_string()));
        }
    }
    let c = Console {
        key: key.clone(),
        state: VmState::Starting,
        name: cfg.name.clone(),
        win,
        io,
        fb: None,
        shared,
        child,
        img: None,
        mode: (0, 0),
        last_thumb: Instant::now(),
        last_save: Instant::now(),
        clip_policy: cfg.clipboard.clone(),
        clipboard: arboard::Clipboard::new().ok(),
        clip_seen: None,
        pending_resize: None,
        hidpi: cfg.display.hidpi,
        toast_id: 0,
        ever_ran: false,
        gone,
    };
    CONSOLES.with(|m| m.borrow_mut().insert(key.clone(), c));
    ensure_timer();
    msg::send(Msg::ConsoleState { key: key.clone(), state: VmState::Starting });
    Ok(key)
}

fn wire(win: &ConsoleWindow, key: &str, io: &Rc<RefCell<Io>>) {
    {
        let io = io.clone();
        win.on_mouse_move(move |fx, fy| {
            let x = (fx.clamp(0.0, 1.0) * POINTER_MAX as f32) as u16;
            let y = (fy.clamp(0.0, 1.0) * POINTER_MAX as f32) as u16;
            io.borrow_mut().send(ToVmm::Pointer { x, y });
        });
    }
    {
        let io = io.clone();
        win.on_button(move |code, down| io.borrow_mut().button(code as u16, down));
    }
    {
        let io = io.clone();
        win.on_wheel(move |dx, dy| {
            let mut io = io.borrow_mut();
            io.wheel.0 += dx;
            io.wheel.1 += dy;
            let sx = (io.wheel.0 / WHEEL_STEP).trunc();
            let sy = (io.wheel.1 / WHEEL_STEP).trunc();
            if sx != 0.0 || sy != 0.0 {
                io.wheel.0 -= sx * WHEEL_STEP;
                io.wheel.1 -= sy * WHEEL_STEP;
                io.send(ToVmm::Wheel { dx: sx as i16, dy: sy as i16 });
            }
        });
    }
    {
        let k = key.to_string();
        win.on_act(move |a| {
            let (k, a) = (k.clone(), a.to_string());
            // Not from inside the callback: the command may hide this window.
            slint::Timer::single_shot(Duration::ZERO, move || command(&k, &a));
        });
    }
    {
        let k = key.to_string();
        win.window().on_close_requested(move || {
            let k = k.clone();
            slint::Timer::single_shot(Duration::ZERO, move || command(&k, "close"));
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    let (k, io) = (key.to_string(), io.clone());
    win.window().on_winit_window_event(move |_w, ev| winit_event(&k, &io, ev));
}

/// Keys as physical key codes (what the guest's keyboard sends), focus,
/// resizes. Text input never goes through here.
fn winit_event(key: &str, io: &Rc<RefCell<Io>>, ev: &WindowEvent) -> EventResult {
    match ev {
        WindowEvent::ModifiersChanged(m) => {
            io.borrow_mut().mods = m.state();
            EventResult::Propagate
        }
        WindowEvent::Focused(f) => {
            let mut i = io.borrow_mut();
            i.focused = *f;
            if !*f {
                i.release_all();
            }
            i.send(ToVmm::Focus(*f));
            drop(i);
            if *f {
                // Can arrive from inside show(): don't touch the registry now.
                let key = key.to_string();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    with(&key, |c| c.clip_seen = None);
                });
            }
            EventResult::Propagate
        }
        WindowEvent::KeyboardInput { event, .. } => {
            let PhysicalKey::Code(k) = event.physical_key else { return EventResult::PreventDefault };
            let down = event.state == ElementState::Pressed;
            let m = io.borrow().mods;
            if down && m.control_key() && m.alt_key() && m.shift_key() {
                if let Some(cmd) = host_shortcut(k) {
                    let key = key.to_string();
                    slint::Timer::single_shot(Duration::ZERO, move || command(&key, cmd));
                    return EventResult::PreventDefault;
                }
            }
            if event.repeat {
                return EventResult::PreventDefault; // the guest repeats on its own
            }
            if let Some(code) = crate::keymap::evdev(k) {
                io.borrow_mut().key(code, down);
            }
            EventResult::PreventDefault
        }
        // Moving to a screen with another scale also resizes.
        WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
            let key = key.to_string();
            slint::Timer::single_shot(Duration::ZERO, move || {
                with(&key, |c| c.pending_resize = Some(Instant::now() + RESIZE_DEBOUNCE));
            });
            EventResult::Propagate
        }
        _ => EventResult::Propagate,
    }
}

/// Ctrl+Alt+Shift+<key>: never sent to the guest.
fn host_shortcut(k: KeyCode) -> Option<&'static str> {
    Some(match k {
        KeyCode::KeyS => "snapshot",
        KeyCode::KeyP => "toggle-pause",
        KeyCode::KeyR => "reset",
        KeyCode::KeyD => "shutdown",
        KeyCode::KeyQ => "poweroff",
        KeyCode::KeyZ => "suspend",
        KeyCode::KeyF => "fullscreen",
        KeyCode::KeyL => "library",
        _ => return None,
    })
}

fn reader(mut s: UnixStream, key: String, shared: Arc<Mutex<Shared>>, gone: Arc<AtomicBool>) {
    // The VMM sends the framebuffer first.
    match recv_fd(&s) {
        Ok(fd) => {
            let k = key.clone();
            let _ = slint::invoke_from_event_loop(move || ready(&k, fd));
        }
        Err(_) => {
            gone.store(true, Ordering::Release);
            let _ = slint::invoke_from_event_loop(move || closed(&key));
            return;
        }
    }
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
                Ok(Some(ToViewer::Closed)) => {
                    gone.store(true, Ordering::Release);
                    let _ = slint::invoke_from_event_loop(move || closed(&key));
                    return;
                }
                Ok(Some(m)) => {
                    if shared.lock().unwrap_or_else(|e| e.into_inner()).push(m) {
                        let k = key.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            with(&k, |c| c.inbox());
                        });
                    }
                }
                Ok(None) => break,
                Err(e) => log::warn!("from vmm: {e}"),
            }
        }
    }
    gone.store(true, Ordering::Release);
    let _ = slint::invoke_from_event_loop(move || closed(&key));
}

fn ready(key: &str, fd: OwnedFd) {
    with(key, |c| match SharedFb::from_fd(fd) {
        Ok(fb) => c.fb = Some(fb),
        Err(e) => {
            c.toast(&format!("display: {e}"));
        }
    });
}

fn closed(key: &str) {
    let Some(mut c) = CONSOLES.with(|m| m.borrow_mut().remove(key)) else { return };
    c.save_thumb(None);
    let _ = c.win.hide();
    // The VMM exits right after saying goodbye; don't block the UI on it.
    let status = match c.child.try_wait() {
        Ok(Some(s)) => Some(s),
        _ => {
            std::thread::sleep(Duration::from_millis(50));
            c.child.try_wait().ok().flatten()
        }
    };
    let failed = status.is_some_and(|s| !s.success());
    if status.is_none() {
        let mut child = c.child;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    let error = if failed || !c.ever_ran {
        let t = tail(&crate::paths::log_file(key), 4);
        let t = t.lines().rev().find(|l| l.contains("vroom:") || l.contains("error")).unwrap_or(t.lines().last().unwrap_or("")).to_string();
        Some(if t.is_empty() { format!("{} stopped", c.name) } else { clean(&t) })
    } else {
        None
    };
    msg::send(Msg::ConsoleGone { key: key.to_string(), error });
}

fn ensure_timer() {
    TIMER.with(|t| {
        if !t.running() {
            t.start(slint::TimerMode::Repeated, CLIP_POLL / 2, tick);
        }
    });
}

fn tick() {
    let keys: Vec<String> = CONSOLES.with(|m| m.borrow().keys().cloned().collect());
    for k in keys {
        with(&k, |c| c.tick());
    }
}

impl Console {
    fn set_state(&mut self, s: VmState) {
        if self.state != s {
            self.state = s;
            self.win.set_state(s);
            msg::send(Msg::ConsoleState { key: self.key.clone(), state: s });
        }
    }

    fn toast(&mut self, text: &str) {
        self.toast_id += 1;
        let id = self.toast_id;
        self.win.set_toast(text.into());
        let k = self.key.clone();
        slint::Timer::single_shot(Duration::from_millis(3500), move || {
            with(&k, |c| {
                if c.toast_id == id {
                    c.win.set_toast("".into());
                }
            });
        });
    }

    pub fn command(&mut self, cmd: &str) {
        let send = |c: &mut Console, m: Command| c.io.borrow_mut().send(ToVmm::Command(m));
        match cmd {
            "pause" => send(self, Command::Pause),
            "resume" => send(self, Command::Resume),
            "toggle-pause" => {
                let m = if self.state == VmState::Paused { Command::Resume } else { Command::Pause };
                send(self, m);
            }
            "snapshot" => {
                send(self, Command::Snapshot);
                self.toast("Taking a snapshot...");
            }
            "reset" => send(self, Command::Reset),
            "shutdown" => {
                send(self, Command::Shutdown);
                self.toast("Asked the guest to shut down");
            }
            "poweroff" => {
                self.set_state(VmState::Stopping);
                send(self, Command::PowerOff);
            }
            "suspend" | "close" => {
                if self.gone.load(Ordering::Acquire) {
                    let _ = self.win.hide();
                    return;
                }
                if self.state != VmState::Suspending {
                    self.save_thumb(None);
                    self.set_state(VmState::Suspending);
                    send(self, Command::Suspend);
                }
            }
            "cad" => {
                let mut io = self.io.borrow_mut();
                for code in [29u16, 56, 111] {
                    io.send(ToVmm::Key { code, down: true });
                }
                for code in [111u16, 56, 29] {
                    io.send(ToVmm::Key { code, down: false });
                }
            }
            "fullscreen" => {
                let w = self.win.window();
                let fs = !w.is_fullscreen();
                w.set_fullscreen(fs);
                self.win.set_fullscreen(fs);
            }
            "library" => msg::send(Msg::ShowLibrary),
            _ => {}
        }
    }

    /// What the reader thread collected since the last wakeup.
    fn inbox(&mut self) {
        let (frame, full, damage, status, clip, mic) = {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.posted = false;
            let f = (std::mem::take(&mut s.frame), std::mem::take(&mut s.full), s.damage.take());
            (f.0, f.1, f.2, std::mem::take(&mut s.status), s.clipboard.take(), s.mic.take())
        };
        for t in status {
            self.status(&t);
        }
        if let Some(on) = mic {
            self.mic(on);
        }
        if let Some(t) = clip {
            self.guest_clipboard(t);
        }
        if frame {
            self.frame(full, damage);
        }
    }

    fn status(&mut self, raw: &str) {
        let t = clean(raw);
        if let Some(s) = status_state(&t) {
            self.ever_ran |= s == VmState::Running;
            self.set_state(s);
        }
        match t.as_str() {
            "running" | "paused" => {}
            _ if t.starts_with("snapshot: ") => {
                // From the raw line: the cleaned one may be cut short.
                if let Some(n) = snapshot_name(raw) {
                    self.save_thumb(Some(&n));
                }
                self.toast("Snapshot saved");
                msg::send(Msg::SnapshotTaken { key: self.key.clone() });
            }
            _ if t.starts_with("guest agent connected") => self.toast("Guest tools connected: clipboard and resizing work"),
            _ if t.starts_with("host disk almost full") => {
                self.toast(&t);
                msg::send(Msg::Toast { text: format!("{}: {t}", self.name), kind: 2, action: String::new() });
            }
            _ => self.toast(&t),
        }
    }

    /// Microphone indicator. Turning on also shows the toolbar for a
    /// moment so it gets noticed.
    fn mic(&mut self, on: bool) {
        if on == self.win.get_mic_on() {
            return;
        }
        self.win.set_mic_on(on);
        if on {
            self.win.set_bar_pinned(true);
            let k = self.key.clone();
            slint::Timer::single_shot(Duration::from_secs(3), move || {
                with(&k, |c| c.win.set_bar_pinned(false));
            });
        }
    }

    fn guest_clipboard(&mut self, t: String) {
        let focused = self.io.borrow().focused;
        let ok = focused && self.clip_policy.guest_to_host && t.len() <= self.clip_policy.max_bytes as usize;
        if let (true, Some(cb)) = (ok, self.clipboard.as_mut()) {
            let _ = cb.set_text(t.clone());
            self.clip_seen = Some(t);
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.pending_resize.is_some_and(|at| now >= at) {
            self.pending_resize = None;
            let sz = self.win.window().size();
            let (width, height, scale) = guest_size(sz.width, sz.height, self.win.window().scale_factor(), self.hidpi);
            self.io.borrow_mut().send(ToVmm::Resize { width, height, scale });
        }
        // Host clipboard -> guest, only while this window has focus.
        if self.io.borrow().focused && self.clip_policy.host_to_guest {
            if let Some(Ok(t)) = self.clipboard.as_mut().map(|c| c.get_text()) {
                if self.clip_seen.as_deref() != Some(t.as_str()) {
                    self.clip_seen = Some(t.clone());
                    if t.len() <= self.clip_policy.max_bytes as usize {
                        self.io.borrow_mut().send(ToVmm::Clipboard(t));
                    }
                }
            }
        }
        if self.state == VmState::Running && now.duration_since(self.last_save) > THUMB_SAVE_EVERY {
            self.last_save = now;
            self.save_thumb(None);
        }
        // A VMM that died without a goodbye.
        if let Ok(Some(_)) = self.child.try_wait() {
            if !self.gone.swap(true, Ordering::AcqRel) {
                let k = self.key.clone();
                slint::Timer::single_shot(Duration::ZERO, move || closed(&k));
            }
        }
        self.win.set_captured(self.io.borrow().focused && self.state == VmState::Running);
    }

    fn frame(&mut self, full: bool, damage: Option<[u32; 4]>) {
        let mode = self.shared.lock().unwrap_or_else(|e| e.into_inner()).mode;
        let Some(fb) = &self.fb else { return };
        let (w, h) = mode;
        if w == 0 || h == 0 {
            self.mode = (0, 0);
            self.win.set_mode_w(0);
            self.win.set_mode_h(0);
            return;
        }
        let fresh = full || self.mode != mode || self.img.as_ref().is_none_or(|b| (b.width(), b.height()) != mode);
        let [x0, y0, x1, y1] = match damage {
            Some(d) if !fresh => [d[0].min(w), d[1].min(h), d[2].min(w), d[3].min(h)],
            _ => [0, 0, w, h],
        };
        if x1 > x0 && y1 > y0 {
            // Let go of the picture on screen, so ours is the only
            // reference and only the changed rows get written.
            let mut buf = match self.img.take() {
                Some(b) if !fresh => {
                    self.win.set_frame(slint::Image::default());
                    b
                }
                _ => SharedPixelBuffer::new(w, h),
            };
            to_rgba(fb, w, [x0, y0, x1, y1], buf.make_mut_bytes());
            self.img = Some(buf.clone());
            self.win.set_frame(slint::Image::from_rgba8(buf));
        }
        if self.mode != mode {
            // Ask the guest to match the window on the first frame.
            if self.mode == (0, 0) {
                self.pending_resize = Some(Instant::now());
            }
            self.mode = mode;
            self.win.set_mode_w(w as i32);
            self.win.set_mode_h(h as i32);
        }
        if self.state == VmState::Starting {
            self.ever_ran = true;
            self.set_state(VmState::Running);
        }
        let now = Instant::now();
        if now.duration_since(self.last_thumb) > THUMB_EVERY {
            self.last_thumb = now;
            if let Some(img) = self.scaled(640).map(|(b, _, _)| slint::Image::from_rgba8(b)) {
                msg::send(Msg::Thumb { key: self.key.clone(), image: img });
            }
        }
    }

    /// The screen, at most `max_w` wide.
    fn scaled(&self, max_w: u32) -> Option<(SharedPixelBuffer<Rgba8Pixel>, u32, u32)> {
        let img = self.img.as_ref()?;
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return None;
        }
        let px = img.as_slice();
        let tw = w.min(max_w);
        let th = (h as u64 * tw as u64 / w as u64).max(1) as u32;
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(tw, th);
        let out = buf.make_mut_slice();
        for y in 0..th as usize {
            let sy = y * h as usize / th as usize;
            for x in 0..tw as usize {
                let sx = x * w as usize / tw as usize;
                out[y * tw as usize + x] = px[sy * w as usize + sx];
            }
        }
        Some((buf, tw, th))
    }

    /// Save the screen as the VM's picture, or as a snapshot's.
    fn save_thumb(&self, snapshot: Option<&str>) {
        let Some((buf, w, h)) = self.scaled(960) else { return };
        let dir = crate::paths::thumbs_dir(&self.key);
        let p = match snapshot {
            Some(n) => dir.join(format!("snap-{n}.png")),
            None => dir.join("screen.png"),
        };
        if let Err(e) = write_png(&p, &buf, w, h) {
            log::warn!("thumbnail {}: {e}", p.display());
        }
    }
}

/// Copy the `x0..x1` x `y0..y1` part of the guest screen (`fb_w` wide)
/// into the same place of `out` (RGBA, as wide), converting from XRGB.
/// The changed part only: at 5K a whole frame is 59 MB.
fn to_rgba(fb: &SharedFb, fb_w: u32, [x0, y0, x1, y1]: [u32; 4], out: &mut [u8]) {
    let stride = fb_w as usize * 4;
    let Some(dst) = out.get_mut(y0 as usize * stride + x0 as usize * 4..) else { return };
    fb.read_rect(fb_w, x0, y0, x1 - x0, y1 - y0, dst, stride);
    let row = (x1 - x0) as usize * 4;
    for r in 0..(y1 - y0) as usize {
        if let Some(px) = dst.get_mut(r * stride..r * stride + row) {
            xrgb_to_rgba(px);
        }
    }
}

/// In place: little-endian XRGB (bytes B, G, R, X) to RGBA.
fn xrgb_to_rgba(px: &mut [u8]) {
    for p in px.as_chunks_mut::<4>().0 {
        let v = u32::from_le_bytes(*p);
        *p = (((v >> 16) & 0xff) | (v & 0xff00) | ((v & 0xff) << 16) | 0xff00_0000).to_le_bytes();
    }
}

pub fn write_png(p: &Path, buf: &SharedPixelBuffer<Rgba8Pixel>, w: u32, h: u32) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let tmp = p.with_extension("tmp~");
    let f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(buf.as_bytes()).map_err(|e| e.to_string())?;
    wr.finish().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flooding_vmm_stays_bounded() {
        let mut s = Shared::default();
        assert!(s.push(ToViewer::Damage { x: 0, y: 0, w: 1, h: 1 }), "first one wakes the UI");
        for i in 0..100_000u32 {
            assert!(!s.push(ToViewer::Mode { width: 640 + i % 8, height: 480 }));
            assert!(!s.push(ToViewer::Status("x".repeat(100_000))));
            assert!(!s.push(ToViewer::ClipboardSet(i.to_string())));
        }
        assert_eq!(s.status.len(), MAX_STATUS);
        assert!(s.status.iter().all(|t| t.len() <= MAX_STATUS_LEN));
        assert_eq!(s.clipboard.as_deref(), Some("99999"));
        assert_eq!(s.mode, (640 + 99_999 % 8, 480));
        // Bad modes are dropped.
        assert!(!s.push(ToViewer::Mode { width: 1 << 20, height: 1 }));
        assert_eq!(s.mode.1, 480);
        // The UI took it all: the next message wakes it again.
        s.posted = false;
        s.status.clear();
        assert!(s.push(ToViewer::Status("é".repeat(MAX_STATUS_LEN))));
        assert!(s.status[0].len() <= MAX_STATUS_LEN);
    }

    #[test]
    fn guest_sizes() {
        // Retina, HiDPI: every pixel, 2x.
        assert_eq!(guest_size(3456, 2234, 2.0, true), (3456, 2234, 200));
        assert_eq!(guest_size(3456, 2234, 2.0, false), (1728, 1117, 0));
        assert_eq!(guest_size(1920, 1080, 1.0, true), (1920, 1080, 100));
        // Bigger than the framebuffer (16K wide): shrunk to fit, scale too.
        let (w, h, s) = guest_size(15360, 8640, 2.0, true);
        assert!(w <= MAX_WIDTH && h <= MAX_HEIGHT && s == 100, "{w}x{h} {s}");
        // Junk scale factors stay sane.
        assert_eq!(guest_size(800, 600, f32::NAN, true), (800, 600, 100));
        assert_eq!(guest_size(0, 0, 0.0, false), (2, 2, 0));
    }

    #[test]
    fn damage_converts_only_its_rect() {
        let fb = SharedFb::create().unwrap();
        // 4x2 screen, every pixel XRGB 0x00112233.
        fb.write_rect(4, 0, 0, 4, 2, &[0x33, 0x22, 0x11, 0x00].repeat(8), 16);
        let mut out = vec![0u8; 4 * 2 * 4];
        to_rgba(&fb, 4, [1, 1, 3, 2], &mut out);
        let px = |i: usize| &out[i * 4..i * 4 + 4];
        assert_eq!(px(5), &[0x11, 0x22, 0x33, 0xff]);
        assert_eq!(px(6), &[0x11, 0x22, 0x33, 0xff]);
        assert!([0, 1, 2, 3, 4, 7].iter().all(|&i| px(i) == [0; 4]), "outside the rect: untouched");
        // A rect past the buffer: nothing written, no panic.
        to_rgba(&fb, 4, [0, 5, 4, 9], &mut out);
    }

    #[test]
    fn damage_adds_up() {
        let mut s = Shared::default();
        s.push(ToViewer::Damage { x: 10, y: 10, w: 5, h: 5 });
        s.push(ToViewer::Damage { x: 2, y: 30, w: 1, h: 1 });
        s.push(ToViewer::Damage { x: u32::MAX, y: 0, w: 9, h: 1 });
        assert_eq!(s.damage, Some([2, 0, u32::MAX, 31]));
        assert!(!s.full);
        s.push(ToViewer::Mode { width: 8, height: 8 });
        assert!(s.full);
    }

    #[test]
    fn status_lines() {
        assert_eq!(status_state("running"), Some(VmState::Running));
        assert_eq!(status_state("paused"), Some(VmState::Paused));
        // A suspend that failed leaves the VM paused, not "suspending" forever.
        assert_eq!(status_state("suspend failed: disk full"), Some(VmState::Paused));
        assert_eq!(status_state("snapshot: /x"), None);
        assert_eq!(snapshot_name("snapshot: /a/b/vm.state/snapshots/snap-20261015-1200"), Some("snap-20261015-1200".into()));
        let long = format!("snapshot: /{}/snap-1", "d".repeat(300));
        assert_eq!(snapshot_name(&long), Some("snap-1".into()), "names come from the whole line");
        for bad in ["snapshot: /x/..", "snapshot: /x/.hidden", "snapshot: /x/a b", "snapshot: ", "snapshot: /x/a\u{202e}gnp"] {
            assert_eq!(snapshot_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn cleaning() {
        assert_eq!(clean("a\x1b[31mb\nc\u{202e}d\u{200b}e"), "a[31mbcde");
        assert_eq!(clean(&"x".repeat(1000)).len(), 160);
    }

    #[test]
    fn log_tail_reads_only_the_end() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("vm.log");
        let mut f = std::fs::File::create(&p).unwrap();
        // Sparse: 8 GB that would take as much memory to read whole.
        f.set_len(8 << 30).unwrap();
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(b"\nvroom: one\n\nvroom: two\n").unwrap();
        assert_eq!(tail(&p, 2), "vroom: one\nvroom: two");
        assert_eq!(tail(&t.path().join("nope"), 2), "");
    }

    #[test]
    fn focus_loss_releases_keys_and_buttons() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut io = Io {
            tx: Some(a),
            mods: ModifiersState::empty(),
            down: BTreeSet::new(),
            buttons: BTreeSet::new(),
            wheel: (0.0, 0.0),
            focused: true,
        };
        io.key(30, true);
        io.button(272, true);
        io.button(273, true);
        io.button(273, false);
        io.release_all();
        io.tx = None;
        let mut buf = Vec::new();
        b.set_nonblocking(true).unwrap();
        let _ = b.read_to_end(&mut buf);
        let mut d = Decoder::default();
        d.push(&buf);
        let mut got = Vec::new();
        while let Some(m) = d.next_to_vmm().unwrap() {
            got.push(m);
        }
        assert_eq!(&got[4..], &[ToVmm::Key { code: 30, down: false }, ToVmm::Button { code: 272, down: false }]);
    }
}
