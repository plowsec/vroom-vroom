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
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Proc, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use slint::winit_030::winit::window::{CursorIcon, CustomCursor};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use vroom_config::{GuestOs, VmConfig};
use vroom_display::{recv_fd, Command, Decoder, GuestCursor, SharedFb, ToViewer, ToVmm, MAX_HEIGHT, MAX_WIDTH, POINTER_MAX};

use crate::msg::{self, Msg};
use crate::{ConsoleWindow, VmState};

mod usbmenu;

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
    /// The guest has the camera on (latest word).
    camera: Option<bool>,
    /// The guest's pointer, the latest.
    cursor: Option<GuestCursor>,
    /// Answers about files dragged over the window, in order.
    drag: VecDeque<ToViewer>,
    /// The host USB devices the VM has, the latest list.
    usb: Option<Vec<vroom_display::UsbPort>>,
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
            ToViewer::Camera(on) => self.camera = Some(on),
            ToViewer::Cursor(c) => self.cursor = Some(c),
            m @ (ToViewer::DragReady { .. } | ToViewer::DropDone { .. }) => {
                if self.drag.len() >= MAX_STATUS {
                    return false;
                }
                self.drag.push_back(m);
            }
            ToViewer::Usb(l) => self.usb = Some(l),
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
    /// The VM file.
    path: PathBuf,
    win: ConsoleWindow,
    io: Rc<RefCell<Io>>,
    fb: Option<SharedFb>,
    shared: Arc<Mutex<Shared>>,
    /// The VMM, if this app started it (not when reattached to one).
    child: Option<Child>,
    pid: u32,
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
    /// The guest's pointer, as it last said.
    cursor: GuestCursor,
    /// Bumped with each new picture of it.
    cursor_gen: u64,
    /// The guest draws its own pointer into the frame when it gives us
    /// none (Windows: its virtio-gpu drivers have no hardware pointer).
    draws_pointer: bool,
    /// The mouse is over the guest screen.
    over_screen: bool,
    /// The host pointer last set: what it is, and the one made from the
    /// guest's picture (picture, size).
    pointer: Option<Pointer>,
    host_cursor: Option<(u64, u32, CustomCursor)>,
    /// The VM's CPUs, and its CPU time when last looked at.
    cpus: u32,
    cpu_seen: Option<(Instant, Duration)>,
    /// Files dragged over the window, or being dropped.
    drag: Option<Drag>,
    drag_seq: u32,
    drag_timer: slint::Timer,
    /// Host USB devices: the menu, and what the VM has.
    usb: usbmenu::UsbMenu,
}

/// Files from the Mac over the window (see dnd.rs).
struct Drag {
    id: u32,
    paths: Vec<PathBuf>,
    /// The guest's answer: whether it runs a live drag, or why it can't
    /// take the files.
    ready: Option<Result<bool, String>>,
    /// We hold the guest's left button for its live drag.
    pressed: bool,
    /// Dropped: the copy underway.
    copy: Option<Arc<crate::dnd::Progress>>,
    pointer: Option<(u16, u16)>,
}

/// The guest's left button, and Escape (evdev).
const BTN_LEFT: u16 = 0x110;
const KEY_ESC: u16 = 1;
/// How often the pointer follows the mouse during a drag (the window gets
/// no mouse events then).
const DRAG_POLL: Duration = Duration::from_millis(16);

/// What the host pointer is over the guest screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pointer {
    Arrow,
    Hidden,
    /// The guest's picture (its number), at this size in pixels.
    Guest(u64, u32),
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

/// `cmd` to every VM window (for `VROOM_SHOT`).
pub fn command_any(cmd: &str) {
    CONSOLES.with(|c| {
        for con in c.borrow_mut().values_mut() {
            con.command(cmd);
        }
    });
}

/// A key press to every VM's guest (for `VROOM_SHOT`).
pub fn key_any(code: u16) {
    CONSOLES.with(|c| {
        for con in c.borrow().values() {
            let mut io = con.io.borrow_mut();
            io.key(code, true);
            io.key(code, false);
        }
    });
}

/// The mouse to a point of every VM window, as if moved there (for
/// `VROOM_SHOT`): goes through the window like a real one.
pub fn move_any(x: f32, y: f32) {
    let wins: Vec<ConsoleWindow> = CONSOLES.with(|c| c.borrow().values().map(|c| c.win.clone_strong()).collect());
    for w in wins {
        let position = slint::LogicalPosition::new(x, y);
        w.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
    }
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
/// Why a suspend failed, for people: the VMM's line names its layers
/// ("snapshot: encode: device: the GPU can't be saved: <why>"); the last
/// part is the reason, in words.
fn suspend_why(raw: &str) -> String {
    let why = raw.trim().trim_start_matches("suspend failed").trim_start_matches([':', ' ']);
    let why = why.rsplit(": ").next().unwrap_or(why);
    let mut s = clean(why);
    if let Some(c) = s.get(..1) {
        s.replace_range(..1, &c.to_uppercase());
    }
    if !s.ends_with('.') && !s.ends_with(')') {
        s.push('.');
    }
    s
}

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
    let pid = child.id();
    open(key, path, cfg, ours, Some(child), pid, VmState::Starting)
}

/// How long a VMM we reattach to gets to send its screen.
const ATTACH_WAIT: Duration = Duration::from_secs(5);

/// Where a VM running without a window (its app crashed) can be shown
/// again: the VMM says in its state dir.
pub fn viewer_socket(state_dir: &Path) -> Option<PathBuf> {
    let p = std::fs::read_to_string(state_dir.join("viewer")).ok()?;
    let p = PathBuf::from(p.trim());
    std::os::unix::fs::FileTypeExt::is_socket(&std::fs::symlink_metadata(&p).ok()?.file_type()).then_some(p)
}

/// Show a VM that runs without a window again (its app crashed, or was
/// quit while it couldn't be saved).
pub fn attach(path: &Path, cfg: &VmConfig, sock: &Path, pid: u32) -> Result<String, String> {
    let key = crate::paths::key(path);
    if is_running(&key) {
        show(&key);
        return Ok(key);
    }
    let s = UnixStream::connect(sock).map_err(|e| format!("it doesn't answer ({e})"))?;
    // A VMM that hangs never sends the screen: give up then (see reader).
    s.set_read_timeout(Some(ATTACH_WAIT)).map_err(|e| e.to_string())?;
    open(key, path, cfg, s, None, pid, VmState::Running)
}

/// The window for a VMM on the other end of `ours`.
fn open(key: String, path: &Path, cfg: &VmConfig, ours: UnixStream, child: Option<Child>, pid: u32, state: VmState) -> Result<String, String> {
    let fail = |child: Option<Child>, e: String| match child {
        Some(c) => abandon(c, e),
        None => e,
    };
    let os = cfg.guest_os();
    let win = match ConsoleWindow::new() {
        Ok(w) => w,
        Err(e) => return Err(fail(child, e.to_string())),
    };
    win.set_vm_name(cfg.name.clone().into());
    win.set_os(crate::os_id(os).into());
    win.set_state(state);
    win.set_has_display(cfg.display.gpu != vroom_config::Gpu::None);
    win.set_windows_guest(os == GuestOs::Windows);
    if cfg.display.gpu == vroom_config::Gpu::Virtio3d {
        win.set_gpu_text(if os == GuestOs::Windows { "Direct3D 11" } else { "3D" }.into());
    }
    let io = Rc::new(RefCell::new(Io {
        tx: ours.try_clone().ok(),
        mods: ModifiersState::empty(),
        down: BTreeSet::new(),
        buttons: BTreeSet::new(),
        wheel: (0.0, 0.0),
        focused: false,
    }));
    wire(&win, &key, &io);

    if let Err(e) = win.show() {
        return Err(fail(child, e.to_string()));
    }

    let shared = Arc::new(Mutex::new(Shared::default()));
    let gone = Arc::new(AtomicBool::new(false));
    {
        let (key, shared, gone) = (key.clone(), shared.clone(), gone.clone());
        let r = std::thread::Builder::new().name("vmm-reader".into()).spawn(move || reader(ours, key, shared, gone));
        if let Err(e) = r {
            let _ = win.hide();
            return Err(fail(child, e.to_string()));
        }
    }
    let c = Console {
        key: key.clone(),
        state,
        name: cfg.name.clone(),
        path: path.to_path_buf(),
        win,
        io,
        fb: None,
        shared,
        child,
        pid,
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
        cursor: GuestCursor::Unset,
        cursor_gen: 0,
        draws_pointer: os == GuestOs::Windows,
        over_screen: false,
        pointer: None,
        host_cursor: None,
        cpus: cfg.cpus.max(1),
        cpu_seen: None,
        drag: None,
        drag_seq: 0,
        drag_timer: slint::Timer::default(),
        usb: usbmenu::UsbMenu::default(),
    };
    CONSOLES.with(|m| m.borrow_mut().insert(key.clone(), c));
    ensure_timer();
    msg::send(Msg::ConsoleState { key: key.clone(), state });
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
        win.on_pointer_over(move |over| {
            let k = k.clone();
            // After Slint is done with the event: it sets its own pointer
            // as the mouse moves from one item to another.
            slint::Timer::single_shot(Duration::ZERO, move || {
                with(&k, |c| {
                    c.over_screen = over;
                    c.pointer = None;
                    c.apply_pointer();
                });
            });
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
        // Files from the Mac: one event per file, all at once.
        WindowEvent::HoveredFile(p) => {
            let (key, p) = (key.to_string(), p.clone());
            slint::Timer::single_shot(Duration::ZERO, move || {
                with(&key, |c| c.drag_hover(p));
            });
            EventResult::Propagate
        }
        WindowEvent::HoveredFileCancelled => {
            let key = key.to_string();
            slint::Timer::single_shot(Duration::ZERO, move || {
                with(&key, |c| c.drag_left());
            });
            EventResult::Propagate
        }
        WindowEvent::DroppedFile(p) => {
            let (key, p) = (key.to_string(), p.clone());
            slint::Timer::single_shot(Duration::ZERO, move || {
                with(&key, |c| c.drag_dropped(p));
            });
            EventResult::Propagate
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
            let _ = s.set_read_timeout(None);
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
        Ok(fb) => {
            c.fb = Some(fb);
            // Damage that came before it (a VMM we reattached to replays
            // its screen at once) was dropped: draw it all now.
            c.frame(true, None);
        }
        Err(e) => {
            c.toast(&format!("display: {e}"));
        }
    });
}

fn closed(key: &str) {
    usbmenu::forget(key);
    let Some(mut c) = CONSOLES.with(|m| m.borrow_mut().remove(key)) else { return };
    c.save_thumb(None);
    let _ = c.win.hide();
    let Some(mut child) = c.child.take() else {
        // Reattached: not ours to reap. Never got going: it hangs.
        let error = (!c.ever_ran).then(|| format!("{} isn't answering. Force power off (power menu) stops it.", c.name));
        msg::send(Msg::ConsoleGone { key: key.to_string(), error });
        return;
    };
    // The VMM exits right after saying goodbye; don't block the UI on it.
    let status = match child.try_wait() {
        Ok(Some(s)) => Some(s),
        _ => {
            std::thread::sleep(Duration::from_millis(50));
            child.try_wait().ok().flatten()
        }
    };
    let failed = status.is_some_and(|s| !s.success());
    if status.is_none() {
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
            "resume" => {
                self.win.set_suspend_error("".into());
                send(self, Command::Resume);
            }
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
                self.win.set_suspend_error("".into());
                // A paused guest can't hear the request.
                if self.state == VmState::Paused {
                    send(self, Command::Resume);
                }
                send(self, Command::Shutdown);
                self.toast("Asked the guest to shut down");
            }
            "poweroff" => {
                self.win.set_suspend_error("".into());
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
                // The bar comes and goes: the guest gets another size.
                self.pending_resize = Some(Instant::now() + RESIZE_DEBOUNCE);
                if fs {
                    // Show the floating bar for a moment so people see where it went.
                    self.win.set_bar_pinned(true);
                    let k = self.key.clone();
                    slint::Timer::single_shot(Duration::from_secs(3), move || {
                        with(&k, |c| c.win.set_bar_pinned(false));
                    });
                }
            }
            c if c.starts_with("usb") => {
                self.usb_command(c);
            }
            "library" => msg::send(Msg::ShowLibrary(None)),
            "settings" => msg::send(Msg::ShowLibrary(Some(self.key.clone()))),
            _ => {}
        }
    }

    /// What the reader thread collected since the last wakeup.
    fn inbox(&mut self) {
        let (frame, full, damage, status, clip, mic, cursor, drag, camera, usb) = {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.posted = false;
            let f = (std::mem::take(&mut s.frame), std::mem::take(&mut s.full), s.damage.take());
            let t = (std::mem::take(&mut s.status), s.clipboard.take(), s.mic.take(), s.cursor.take(), std::mem::take(&mut s.drag));
            (f.0, f.1, f.2, t.0, t.1, t.2, t.3, t.4, s.camera.take(), s.usb.take())
        };
        if let Some(on) = camera {
            self.camera(on);
        }
        if let Some(l) = usb {
            self.usb_list(l);
        }
        for m in drag {
            match m {
                ToViewer::DragReady { id, live } => self.drag_ready(id, live),
                ToViewer::DropDone { id, ok, place } => self.drop_done(id, ok, &clean(&place)),
                _ => {}
            }
        }
        for t in status {
            self.status(&t);
        }
        if let Some(on) = mic {
            self.mic(on);
        }
        if let Some(c) = cursor {
            if matches!(c, GuestCursor::Image { .. }) {
                self.cursor_gen += 1;
            }
            self.cursor = c;
            self.apply_pointer();
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
            _ if t.starts_with("USB: ") => {
                self.usb_refused();
                self.toast(&t);
            }
            _ if t.starts_with("suspend failed") => {
                // Usually 3D in use. Say why and offer the ways out,
                // rather than leaving a paused window that won't close.
                self.win.set_suspend_error(suspend_why(raw).into());
            }
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

    /// Camera indicator; like the microphone's.
    fn camera(&mut self, on: bool) {
        if on == self.win.get_camera_on() {
            return;
        }
        self.win.set_camera_on(on);
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
            // The guest gets the window below the bar.
            let (sz, sf) = (self.win.window().size(), self.win.window().scale_factor());
            let bar = (self.win.get_bar_height() * sf).round() as u32;
            let (width, height, scale) = guest_size(sz.width, sz.height.saturating_sub(bar), sf, self.hidpi);
            self.io.borrow_mut().send(ToVmm::Resize { width, height, scale });
        }
        self.cpu_meter(now);
        self.usb_tick(now);
        // State, size or the screen's place changed: the pointer may too.
        self.apply_pointer();
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
        if let Some(Ok(Some(_))) = self.child.as_mut().map(|c| c.try_wait()) {
            if !self.gone.swap(true, Ordering::AcqRel) {
                let k = self.key.clone();
                slint::Timer::single_shot(Duration::ZERO, move || closed(&k));
            }
        }
        self.win.set_captured(self.io.borrow().focused && self.state == VmState::Running);
    }

    /// A file is dragged over the window (several come at once).
    fn drag_hover(&mut self, p: PathBuf) {
        if let Some(d) = self.drag.as_mut().filter(|d| d.copy.is_none()) {
            if d.ready.is_none() && !d.paths.contains(&p) {
                d.paths.push(p);
            }
            return;
        }
        if self.drag.is_some() {
            // Still copying the last drop.
            return;
        }
        self.drag_seq = self.drag_seq.wrapping_add(1);
        self.drag = Some(Drag { id: self.drag_seq, paths: vec![p], ready: None, pressed: false, copy: None, pointer: None });
        // After this burst of events: then all the names are in.
        let k = self.key.clone();
        slint::Timer::single_shot(Duration::ZERO, move || {
            with(&k, |c| c.drag_enter());
        });
    }

    fn drag_enter(&mut self) {
        let Some(d) = &self.drag else { return };
        let items = crate::dnd::items(&d.paths);
        let id = d.id;
        if items.is_empty() {
            self.drag = None;
            return;
        }
        self.io.borrow_mut().send(ToVmm::DragEnter { id, items });
        self.win.set_drop_over(true);
        self.win.set_drop_text(format!("Copy to {}", self.name).into());
        let k = self.key.clone();
        self.drag_timer.start(slint::TimerMode::Repeated, DRAG_POLL, move || {
            with(&k, |c| c.drag_poll());
        });
    }

    /// Where the mouse is over the guest screen (0..1 each way), if it is.
    fn mouse_on_screen(&self) -> Option<(f32, f32)> {
        let (mx, my) = crate::dnd::mouse()?;
        let sf = self.win.window().scale_factor() as f64;
        let inner = self.win.window().with_winit_window(|w| w.inner_position().ok()).flatten()?;
        let (lx, ly) = (mx - inner.x as f64 / sf, my - inner.y as f64 / sf);
        let fx = (lx - self.win.get_screen_x() as f64) / self.win.get_screen_width() as f64;
        let fy = (ly - self.win.get_screen_y() as f64) / self.win.get_screen_height() as f64;
        ((0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy)).then_some((fx as f32, fy as f32))
    }

    /// The guest's pointer follows the mouse; a copy shows how far it is.
    fn drag_poll(&mut self) {
        let Some(d) = &self.drag else {
            self.drag_timer.stop();
            return;
        };
        if let Some(p) = &d.copy {
            let (sent, total) = (p.sent.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
            let f = if total == 0 { 1.0 } else { sent as f32 / total as f32 };
            self.win.set_drop_progress(f.min(1.0));
            let what = match d.paths.len() {
                1 => d.paths[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                n => format!("{n} items"),
            };
            let text = if sent >= total { format!("Copying {what} to {}...", self.name) } else { format!("Copying {what} to {}: {}%", self.name, (f * 100.0) as u32) };
            self.win.set_drop_text(text.into());
        }
        let Some((fx, fy)) = self.mouse_on_screen() else { return };
        let ptr = ((fx * POINTER_MAX as f32) as u16, (fy * POINTER_MAX as f32) as u16);
        if self.drag.as_ref().is_some_and(|d| d.pointer != Some(ptr)) {
            self.io.borrow_mut().send(ToVmm::Pointer { x: ptr.0, y: ptr.1 });
            if let Some(d) = self.drag.as_mut() {
                d.pointer = Some(ptr);
            }
        }
    }

    fn drag_ready(&mut self, id: u32, live: bool) {
        let Some(d) = self.drag.as_mut().filter(|d| d.id == id) else { return };
        d.ready = Some(Ok(live));
        if live && !d.pressed {
            // The guest's drag window is under its pointer: press on it.
            // The guest then shows where a drop would go.
            d.pressed = true;
            self.io.borrow_mut().send(ToVmm::Button { code: BTN_LEFT, down: true });
        }
        if d.copy.is_none() {
            let t = if live { format!("Copy to {}: drop where you want the files", self.name) } else { format!("Copy to the Desktop of {}", self.name) };
            self.win.set_drop_text(t.into());
        }
    }

    /// Let go of the guest's drag: drop (`drop`), or give up (Escape).
    fn release(&mut self, drop: bool) {
        let Some(d) = self.drag.as_mut() else { return };
        if !std::mem::replace(&mut d.pressed, false) {
            return;
        }
        let mut io = self.io.borrow_mut();
        if !drop {
            io.send(ToVmm::Key { code: KEY_ESC, down: true });
            io.send(ToVmm::Key { code: KEY_ESC, down: false });
        }
        io.send(ToVmm::Button { code: BTN_LEFT, down: false });
    }

    fn drag_end(&mut self) {
        self.drag = None;
        self.drag_timer.stop();
        self.win.set_drop_over(false);
        self.win.set_drop_text("".into());
        self.win.set_drop_progress(-1.0);
    }

    fn drag_left(&mut self) {
        let Some(d) = &self.drag else { return };
        if d.copy.is_some() {
            return;
        }
        let id = d.id;
        self.release(false);
        self.io.borrow_mut().send(ToVmm::DragCancel { id });
        self.drag_end();
    }

    /// Dropped (one event per file, all at once).
    fn drag_dropped(&mut self, p: PathBuf) {
        if self.drag.is_none() {
            self.drag_hover(p);
        }
        let Some(d) = self.drag.as_mut() else { return };
        if d.copy.is_some() {
            return;
        }
        if let Some(Err(why)) = &d.ready {
            let t = format!("Can't copy to {}: {why}", self.name);
            self.drag_end();
            self.toast(&t);
            return;
        }
        let progress = Arc::new(crate::dnd::Progress::default());
        d.copy = Some(progress.clone());
        let (id, paths) = (d.id, d.paths.clone());
        self.win.set_drop_over(false);
        self.win.set_drop_progress(0.0);
        // A pipe for the files: the VMM passes what we write to the guest.
        let sent = UnixStream::pair().and_then(|(ours, theirs)| {
            let io = self.io.borrow();
            let tx = io.tx.as_ref().ok_or_else(|| std::io::Error::other("the VM is gone"))?;
            vroom_display::send_fd_with(tx, &ToVmm::Drop { id }.encode(), theirs.as_raw_fd()).map_err(|e| std::io::Error::other(e.to_string()))?;
            // Not closed at once: macOS can hand over a socket already
            // disconnected when the sender's copy closes while it is in
            // flight (about 1 in 10 here). The VMM has it within moments.
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                drop(theirs);
            });
            Ok(ours)
        });
        match sent.and_then(|ours| std::thread::Builder::new().name("drop".into()).spawn(move || crate::dnd::send(id, paths, ours, progress))) {
            Ok(_) => {}
            Err(e) => {
                self.release(false);
                self.drag_end();
                self.toast(&format!("Can't copy to {}: {e}", self.name));
            }
        }
    }

    fn drop_done(&mut self, id: u32, ok: bool, place: &str) {
        let Some(d) = self.drag.as_ref().filter(|d| d.id == id) else { return };
        if d.copy.is_none() {
            // Before any drop: the guest can't take files. Say so while
            // they hover; a drop then only shows it.
            if !ok {
                let why = if place.is_empty() { "the guest refused".to_string() } else { place.to_string() };
                self.win.set_drop_text(format!("Can't copy to {}: {why}", self.name).into());
                self.win.set_drop_over(false);
                if let Some(d) = self.drag.as_mut() {
                    d.ready = Some(Err(why));
                }
            }
            return;
        }
        let n = d.copy.as_ref().map_or(0, |p| p.files.load(Ordering::Relaxed));
        let live = matches!(d.ready, Some(Ok(true)));
        if ok && live && !d.pressed {
            // The guest was ready only after the drop: drop where it is.
            self.io.borrow_mut().send(ToVmm::Button { code: BTN_LEFT, down: true });
            if let Some(d) = self.drag.as_mut() {
                d.pressed = true;
            }
        }
        self.release(ok);
        self.drag_end();
        let what = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
        let t = match (ok, live) {
            (true, true) => format!("Copied {what} to {}", self.name),
            (true, false) => format!("Copied {what} to {} ({})", self.name, if place.is_empty() { "Desktop" } else { place }),
            (false, _) => format!("Copy to {} failed: {place}", self.name),
        };
        self.toast(&t);
    }

    /// "12%": the host CPU time the VM took over the last second or so,
    /// out of what its CPUs could have taken.
    fn cpu_meter(&mut self, now: Instant) {
        if self.state != VmState::Running {
            self.cpu_seen = None;
            self.win.set_cpu_text("".into());
            return;
        }
        let Some(t) = crate::host::cpu_time(self.pid) else { return };
        match self.cpu_seen {
            Some((at, before)) if now.duration_since(at) >= Duration::from_secs(1) => {
                let used = t.saturating_sub(before).as_secs_f64() / now.duration_since(at).as_secs_f64();
                let pct = (used / self.cpus as f64 * 100.0).round().clamp(0.0, 100.0);
                self.win.set_cpu_text(format!("{pct}%").into());
                self.cpu_seen = Some((now, t));
            }
            Some(_) => {}
            None => self.cpu_seen = Some((now, t)),
        }
    }

    /// Set the host pointer over the guest screen: the guest's own picture
    /// when it gives one (VMware does the same), none when it hid it or
    /// draws its own into the frame, else the usual arrow. Off the screen,
    /// Slint picks the pointer. Does nothing if that's already so.
    fn apply_pointer(&mut self) {
        if !self.over_screen {
            self.pointer = None;
            return;
        }
        let live = self.state == VmState::Running && self.mode != (0, 0) && self.win.get_suspend_error().is_empty();
        let sf = self.win.window().scale_factor();
        let want = match &self.cursor {
            _ if !live => Pointer::Arrow,
            GuestCursor::Image { w, .. } => Pointer::Guest(self.cursor_gen, cursor_px(*w, self.mode.0, self.win.get_screen_width(), sf)),
            GuestCursor::Hidden => Pointer::Hidden,
            GuestCursor::Unset if self.draws_pointer => Pointer::Hidden,
            GuestCursor::Unset => Pointer::Arrow,
        };
        if self.pointer == Some(want) {
            return;
        }
        log::debug!("{}: pointer over the guest screen: {want:?}", self.name);
        let set =|c: Option<&CustomCursor>, visible: bool| {
            self.win.window().with_winit_window(|w| {
                match c {
                    Some(c) => w.set_cursor(c.clone()),
                    None => w.set_cursor(CursorIcon::Default),
                }
                w.set_cursor_visible(visible);
            });
        };
        match want {
            Pointer::Arrow => set(None, true),
            Pointer::Hidden => set(None, false),
            Pointer::Guest(id, px) => match &self.host_cursor {
                Some((i, p, c)) if (*i, *p) == (id, px) => set(Some(c), true),
                _ => {
                    // Made on the event loop (winit wants it); applied then.
                    let GuestCursor::Image { w, h, hot_x, hot_y, rgba } = &self.cursor else { return };
                    let (tw, th, px_rgba, (hx, hy)) = scale_cursor(*w, *h, rgba, (*hot_x, *hot_y), px);
                    let Ok(src) = CustomCursor::from_rgba(px_rgba, tw as u16, th as u16, hx as u16, hy as u16) else { return };
                    let k = self.key.clone();
                    self.pointer = Some(want);
                    let _ = slint::winit_030::invoke_from_active_event_loop(move |el| {
                        let c = el.create_custom_cursor(src);
                        with(&k, |con| {
                            con.host_cursor = Some((id, px, c));
                            con.pointer = None;
                            con.apply_pointer();
                        });
                    });
                    return;
                }
            },
        }
        self.pointer = Some(want);
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

/// How wide, in the host cursor's pixels, a guest pointer `w` pixels wide
/// looks on a screen of `mode_w` guest pixels shown `screen_w` wide
/// (logical). macOS takes cursor pictures in points, others in pixels.
fn cursor_px(w: u32, mode_w: u32, screen_w: f32, sf: f32) -> u32 {
    let k = if mode_w == 0 || !screen_w.is_finite() { 1.0 } else { screen_w / mode_w as f32 };
    let k = if cfg!(target_os = "macos") || !sf.is_finite() { k } else { k * sf };
    ((w as f32 * k).round() as u32).clamp(1, 2 * vroom_display::CURSOR_MAX)
}

/// A guest pointer picture (RGBA) resized to `tw` wide, and its hot spot.
/// Each pixel averages the pixels it covers, weighted by alpha (shrinking
/// a HiDPI guest's pointer); growing repeats them.
fn scale_cursor(w: u32, h: u32, rgba: &[u8], hot: (u32, u32), tw: u32) -> (u32, u32, Vec<u8>, (u32, u32)) {
    let (w, h, tw) = (w.max(1), h.max(1), tw.max(1));
    let th = ((h as u64 * tw as u64 + w as u64 / 2) / w as u64).max(1) as u32;
    let px = |x: u32, y: u32| rgba.get((y * w + x) as usize * 4..(y * w + x) as usize * 4 + 4).unwrap_or(&[0; 4]);
    let mut out = vec![0u8; tw as usize * th as usize * 4];
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h).div_ceil(th).max(ty * h / th + 1).min(h));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w).div_ceil(tw).max(tx * w / tw + 1).min(w));
            let (mut c, mut a, mut n) = ([0u32; 3], 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = px(x, y);
                    for i in 0..3 {
                        c[i] += p[i] as u32 * p[3] as u32;
                    }
                    a += p[3] as u32;
                    n += 1;
                }
            }
            let o = (ty * tw + tx) as usize * 4;
            for i in 0..3 {
                out[o + i] = c[i].checked_div(a).unwrap_or(0) as u8;
            }
            out[o + 3] = (a / n.max(1)) as u8;
        }
    }
    let hx = (hot.0 as u64 * tw as u64 / w as u64).min(tw as u64 - 1) as u32;
    let hy = (hot.1 as u64 * th as u64 / h as u64).min(th as u64 - 1) as u32;
    (tw, th, out, (hx, hy))
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
    fn suspend_reasons() {
        let vk = "suspend failed: snapshot: encode: device: the GPU can't be saved: a Vulkan app is running, and Vulkan state can't be saved";
        assert_eq!(suspend_why(vk), "A Vulkan app is running, and Vulkan state can't be saved.");
        assert_eq!(suspend_why("suspend failed: snapshot: write: No space left on device"), "No space left on device.");
        assert_eq!(suspend_why("suspend failed: disk busy"), "Disk busy.");
    }

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
    fn guest_pointer_sizes() {
        // HiDPI guest (2x pixels) on macOS: half as many points.
        let pts = cursor_px(64, 2560, 1280.0, 2.0);
        assert_eq!(pts, if cfg!(target_os = "macos") { 32 } else { 64 });
        assert_eq!(cursor_px(32, 1280, 1280.0, 1.0), 32);
        assert_eq!(cursor_px(32, 0, f32::NAN, f32::NAN), 32);
        // 2x2 -> 1x1: alpha-weighted color, mean alpha.
        let rgba = [255, 0, 0, 255, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0];
        let (w, h, px, hot) = scale_cursor(2, 2, &rgba, (1, 1), 1);
        assert_eq!((w, h, hot), (1, 1, (0, 0)));
        assert_eq!(px, [255, 0, 0, 127]);
        // Growing repeats pixels; the hot spot moves with them.
        let (w, h, px, hot) = scale_cursor(1, 2, &[1, 2, 3, 4, 5, 6, 7, 8], (0, 1), 2);
        assert_eq!((w, h, hot), (2, 4, (0, 2)));
        assert_eq!(&px[..8], &[1, 2, 3, 4, 1, 2, 3, 4]);
        assert_eq!(&px[24..], &[5, 6, 7, 8, 5, 6, 7, 8]);
        // Short pixel data doesn't panic.
        let _ = scale_cursor(4, 4, &[0; 3], (9, 9), 3);
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
