//! VMM side of the display channel.
//!
//! Owns the shared framebuffer and the socket to the viewer. Guest frames
//! come in through [`FbSink`] (virtio-gpu), viewer input goes out to the
//! virtio-input devices, resize, focus + clipboard to the agent (which
//! also drives the GPU's size), lifecycle commands to the main loop.

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vroom_display::{send_fd, Decoder, GuestCursor, GuestWindow, SharedFb, ToViewer, ToVmm};
use vroom_devices::usb::hid::HidEvent;
use vroom_devices::usb::xhci::Xhci;
use vroom_virtio::devices::gpu::{Cursor, DisplaySink};
use vroom_virtio::devices::input::{self as ev, event};

use crate::agent::{Agent, AgentCmd, ClipboardProvider};
use crate::usb::UsbPorts;
use crate::vcpu::VmEvent;

/// Sends to the viewer without ever blocking the caller.
///
/// A dedicated thread does the (blocking) socket writes. Damage is
/// coalesced into one rect (the union of everything since the viewer was
/// last told), so a viewer that stops reading costs us at most a handful
/// of queued messages, never a stall, and one that keeps up only converts
/// what changed.
///
/// The viewer can go (the app crashed) and another come (the app again):
/// what a newcomer needs to catch up is kept (`Sticky`).
#[derive(Clone)]
pub(crate) struct ViewerTx {
    q: std::sync::mpsc::Sender<ToViewer>,
    /// x0, y0, x1, y1 not yet sent; Some means a marker is queued.
    damage: Arc<Mutex<Option<[u32; 4]>>>,
    /// The guest's latest window list not yet sent (Coherence): like
    /// damage, a guest that keeps changing it costs one queued message.
    windows: Arc<Mutex<Option<Vec<GuestWindow>>>>,
    /// Window icons not yet sent, by window: the latest of each, at most
    /// `ICONS_PENDING` windows.
    icons: Arc<Mutex<Icons>>,
    /// The viewer now, if any.
    sock: Arc<Mutex<Option<UnixStream>>>,
    sticky: Arc<Mutex<Sticky>>,
}

type Icons = std::collections::BTreeMap<u32, (u32, u32, Vec<u8>)>;

/// Most window icons waiting for a slow viewer (16 KiB each at most).
const ICONS_PENDING: usize = 2 * vroom_proto::WINDOWS_MAX;

/// The last word on each thing a viewer shows: a new one gets these first.
#[derive(Default)]
struct Sticky {
    mode: Option<(u32, u32)>,
    cursor: Option<GuestCursor>,
    mic: Option<bool>,
    camera: Option<bool>,
    /// Running, paused, or why a suspend failed.
    state: Option<String>,
    /// "guest agent connected (...)".
    agent: Option<String>,
    /// Host USB devices plugged in (once there is a USB controller).
    usb: Option<Vec<vroom_display::UsbPort>>,
    /// What the guest agent can do.
    features: Option<u64>,
}

impl std::fmt::Debug for ViewerTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerTx").field("attached", &self.attached()).finish()
    }
}

impl ViewerTx {
    fn spawn(sock: Option<UnixStream>) -> std::io::Result<Self> {
        let (q, rx) = std::sync::mpsc::channel::<ToViewer>();
        let damage = Arc::new(Mutex::new(None::<[u32; 4]>));
        let pending = damage.clone();
        let windows = Arc::new(Mutex::new(None::<Vec<GuestWindow>>));
        let pending_windows = windows.clone();
        let icons = Arc::new(Mutex::new(Icons::new()));
        let pending_icons = icons.clone();
        let sock = Arc::new(Mutex::new(sock));
        let out = sock.clone();
        std::thread::Builder::new().name("viewer-tx".into()).spawn(move || {
            while let Ok(mut m) = rx.recv() {
                if matches!(m, ToViewer::Damage { .. }) {
                    let Some([x0, y0, x1, y1]) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() else { continue };
                    m = ToViewer::Damage { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
                }
                if matches!(m, ToViewer::Windows(_)) {
                    let Some(list) = pending_windows.lock().unwrap_or_else(|e| e.into_inner()).take() else { continue };
                    m = ToViewer::Windows(list);
                }
                let mut batch = vec![m];
                if matches!(batch[0], ToViewer::WindowIcon { .. }) {
                    let all = std::mem::take(&mut *pending_icons.lock().unwrap_or_else(|e| e.into_inner()));
                    batch = all.into_iter().map(|(id, (w, h, rgba))| ToViewer::WindowIcon { id, w, h, rgba }).collect();
                }
                let mut s = out.lock().unwrap_or_else(|e| e.into_inner());
                for m in batch {
                    if let Some(v) = s.as_mut() {
                        if v.write_all(&m.encode()).is_err() {
                            *s = None;
                        }
                    }
                }
            }
        })?;
        Ok(ViewerTx { q, damage, windows, icons, sock, sticky: Arc::default() })
    }

    /// A viewer is connected.
    pub fn attached(&self) -> bool {
        self.sock.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn detach(&self) {
        *self.sock.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// A new viewer: the framebuffer first (as at start), then what it
    /// missed, then everything from here on.
    fn attach(&self, mut v: UnixStream, fb: RawFd) -> std::io::Result<()> {
        let mut s = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        send_fd(&v, fb).map_err(|e| std::io::Error::other(e.to_string()))?;
        let st = self.sticky.lock().unwrap_or_else(|e| e.into_inner());
        let mut first = Vec::new();
        if let Some((width, height)) = st.mode {
            first.push(ToViewer::Mode { width, height });
            first.push(ToViewer::Damage { x: 0, y: 0, w: width, h: height });
        }
        first.extend(st.cursor.clone().map(ToViewer::Cursor));
        first.extend(st.mic.map(ToViewer::Mic));
        first.extend(st.camera.map(ToViewer::Camera));
        first.extend(st.agent.clone().map(ToViewer::Status));
        first.extend(st.features.map(|features| ToViewer::Agent { features }));
        first.extend(st.state.clone().map(ToViewer::Status));
        first.extend(st.usb.clone().map(ToViewer::Usb));
        drop(st);
        for m in first {
            v.write_all(&m.encode())?;
        }
        *s = Some(v);
        Ok(())
    }

    fn remember(&self, m: &ToViewer) {
        let mut st = self.sticky.lock().unwrap_or_else(|e| e.into_inner());
        match m {
            ToViewer::Mode { width, height } => st.mode = Some((*width, *height)),
            ToViewer::Cursor(c) => st.cursor = Some(c.clone()),
            ToViewer::Mic(on) => st.mic = Some(*on),
            ToViewer::Camera(on) => st.camera = Some(*on),
            ToViewer::Status(t) if t == "running" || t == "paused" || t.starts_with("suspend failed") || t.starts_with("host disk almost full") => {
                st.state = Some(t.clone())
            }
            ToViewer::Status(t) if t.starts_with("guest agent connected") => st.agent = Some(t.clone()),
            ToViewer::Usb(l) => st.usb = Some(l.clone()),
            ToViewer::Agent { features } => st.features = Some(*features),
            _ => {}
        }
    }

    pub fn send(&self, m: &ToViewer) {
        if let ToViewer::Damage { x, y, w, h } = *m {
            let r = [x, y, x.saturating_add(w), y.saturating_add(h)];
            let mut d = self.damage.lock().unwrap_or_else(|e| e.into_inner());
            match d.as_mut() {
                Some(u) => *u = union(*u, r),
                None => {
                    *d = Some(r);
                    // The rect is filled in when it goes out.
                    let _ = self.q.send(ToViewer::Damage { x: 0, y: 0, w: 0, h: 0 });
                }
            }
            return;
        }
        if let ToViewer::Windows(list) = m {
            if self.windows.lock().unwrap_or_else(|e| e.into_inner()).replace(list.clone()).is_none() {
                // Filled in when it goes out.
                let _ = self.q.send(ToViewer::Windows(Vec::new()));
            }
            return;
        }
        if let ToViewer::WindowIcon { id, w, h, rgba } = m {
            if hold(&mut self.icons.lock().unwrap_or_else(|e| e.into_inner()), *id, (*w, *h, rgba.clone())) {
                // All waiting icons go out with it.
                let _ = self.q.send(ToViewer::WindowIcon { id: 0, w: 0, h: 0, rgba: Vec::new() });
            }
            return;
        }
        self.remember(m);
        let _ = self.q.send(m.clone());
    }
}

/// Keep a window's icon until it goes out (the latest per window, up to
/// `ICONS_PENDING` windows). True if it is the first one waiting.
fn hold(icons: &mut Icons, id: u32, icon: (u32, u32, Vec<u8>)) -> bool {
    let first = icons.is_empty();
    if icons.len() < ICONS_PENDING || icons.contains_key(&id) {
        icons.insert(id, icon);
    }
    first
}

fn union(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// Two sources can feed the screen: ramfb (firmware, boot, Windows before
/// its virtio-gpu driver) and virtio-gpu. Windows with both drivers shows
/// two monitors and may well keep its desktop on the ramfb one, so neither
/// source simply wins: the one that changed most recently does, once the
/// one on screen has been quiet for `HOLD`. GPU updates are partial, so we
/// keep a copy of its picture to redraw from when it takes over again.
#[derive(Default)]
struct Arbiter {
    gpu_on_screen: bool,
    gpu_mode: (u32, u32),
    gpu_shadow: Vec<u8>,
    last_gpu: Option<Instant>,
    last_ramfb: Option<Instant>,
    /// The GPU's pointer; ramfb has none.
    gpu_cursor: Option<GuestCursor>,
}

const HOLD: Duration = Duration::from_secs(3);

fn quiet(t: Option<Instant>, now: Instant) -> bool {
    t.is_none_or(|t| now.duration_since(t) >= HOLD)
}

pub(crate) struct FbSink {
    fb: SharedFb,
    mode: Mutex<(u32, u32)>,
    arb: Mutex<Arbiter>,
    /// None when headless: frames still land in the buffer (screendumps).
    tx: Option<ViewerTx>,
}

impl FbSink {
    pub fn headless() -> std::io::Result<Arc<Self>> {
        let fb = SharedFb::create().map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Arc::new(FbSink { fb, mode: Mutex::new((0, 0)), arb: Mutex::default(), tx: None }))
    }

    /// A viewer window is attached.
    pub fn has_viewer(&self) -> bool {
        self.tx.is_some()
    }

    /// A full frame from ramfb (sent only when it changed); rows
    /// `changed.0..changed.1` differ from the last one.
    pub fn ramfb_frame(&self, w: u32, h: u32, rows: &[u8], stride: usize, changed: (u32, u32)) {
        self.ramfb_frame_at(Instant::now(), w, h, rows, stride, changed)
    }

    fn ramfb_frame_at(&self, now: Instant, w: u32, h: u32, rows: &[u8], stride: usize, changed: (u32, u32)) {
        let mut a = self.arb.lock().unwrap_or_else(|e| e.into_inner());
        a.last_ramfb = Some(now);
        if a.gpu_on_screen {
            if !quiet(a.last_gpu, now) {
                return;
            }
            log::info!("display: showing the firmware framebuffer (virtio-gpu is idle)");
            a.gpu_on_screen = false;
            if a.gpu_cursor.is_some() {
                self.send(ToViewer::Cursor(GuestCursor::Unset));
            }
            // Taking over the screen: all of it.
            self.apply_mode(w, h);
            self.draw(0, 0, w, h, rows, stride);
            return;
        }
        let new_mode = *self.mode.lock().unwrap_or_else(|e| e.into_inner()) != (w, h);
        self.apply_mode(w, h);
        let (y0, y1) = if new_mode { (0, h) } else { (changed.0.min(h), changed.1.min(h)) };
        if y1 > y0 {
            let start = y0 as usize * stride;
            self.draw(0, y0, w, y1 - y0, rows.get(start..).unwrap_or(&[]), stride);
        }
    }

    fn apply_mode(&self, width: u32, height: u32) {
        let mut m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if *m == (width, height) {
            return;
        }
        *m = (width, height);
        drop(m);
        log::debug!("display: mode {width}x{height}");
        if let Some(tx) = &self.tx {
            tx.send(&ToViewer::Mode { width, height });
        }
    }

    fn send(&self, m: ToViewer) {
        if let Some(tx) = &self.tx {
            tx.send(&m);
        }
    }

    fn draw(&self, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
        let (mw, mh) = *self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if x.saturating_add(w) > mw || y.saturating_add(h) > mh {
            return;
        }
        self.fb.write_rect(mw, x, y, w, h, rows, stride);
        if let Some(tx) = &self.tx {
            tx.send(&ToViewer::Damage { x, y, w, h });
        }
    }

    /// Write the current screen as PNG.
    pub fn screendump(&self, path: &std::path::Path) -> std::io::Result<(u32, u32)> {
        let (w, h) = *self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if w == 0 || h == 0 {
            return Err(std::io::Error::other("display is off"));
        }
        let mut px = vec![0u32; w as usize * h as usize];
        self.fb.read_into(w, h, &mut px);
        let rgb: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
        let f = std::fs::File::create(path)?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(std::io::Error::other)?;
        wr.write_image_data(&rgb).map_err(std::io::Error::other)?;
        Ok((w, h))
    }
}

impl DisplaySink for FbSink {
    fn set_mode(&self, width: u32, height: u32) {
        if width != 0 && !SharedFb::mode_ok(width, height) {
            log::warn!("display: mode {width}x{height} too large, ignored");
            return;
        }
        let mut a = self.arb.lock().unwrap_or_else(|e| e.into_inner());
        a.gpu_mode = (width, height);
        a.gpu_shadow = vec![0; width as usize * height as usize * 4];
        a.gpu_on_screen = width != 0;
        a.last_gpu = Some(Instant::now());
        self.apply_mode(width, height);
    }

    fn flush(&self, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
        self.flush_at(Instant::now(), x, y, w, h, rows, stride)
    }

    fn cursor(&self, c: &Cursor) {
        let c = match c {
            Cursor::Unset => GuestCursor::Unset,
            Cursor::Hidden => GuestCursor::Hidden,
            Cursor::Image { w, h, hot_x, hot_y, rgba } => {
                GuestCursor::Image { w: *w, h: *h, hot_x: *hot_x, hot_y: *hot_y, rgba: rgba.clone() }
            }
        };
        if let GuestCursor::Image { w, h, hot_x, hot_y, .. } = &c {
            log::debug!("display: guest pointer {w}x{h}, hot spot {hot_x},{hot_y}");
        } else {
            log::debug!("display: guest pointer {c:?}");
        }
        let mut a = self.arb.lock().unwrap_or_else(|e| e.into_inner());
        if a.gpu_on_screen || c == GuestCursor::Unset {
            self.send(ToViewer::Cursor(c.clone()));
        }
        a.gpu_cursor = (c != GuestCursor::Unset).then_some(c);
    }
}

impl FbSink {
    #[allow(clippy::too_many_arguments)]
    fn flush_at(&self, now: Instant, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
        let mut a = self.arb.lock().unwrap_or_else(|e| e.into_inner());
        let (mw, mh) = a.gpu_mode;
        if x.saturating_add(w) > mw || y.saturating_add(h) > mh {
            return;
        }
        // Keep the GPU picture whether or not it is on screen.
        let row = w as usize * 4;
        for i in 0..h as usize {
            let (Some(src), d) = (rows.get(i * stride..i * stride + row), ((y as usize + i) * mw as usize + x as usize) * 4) else { return };
            a.gpu_shadow[d..d + row].copy_from_slice(src);
        }
        a.last_gpu = Some(now);
        if a.gpu_on_screen {
            self.draw(x, y, w, h, rows, stride);
        } else if quiet(a.last_ramfb, now) {
            log::info!("display: showing virtio-gpu again");
            a.gpu_on_screen = true;
            if let Some(c) = a.gpu_cursor.clone() {
                self.send(ToViewer::Cursor(c));
            }
            self.apply_mode(mw, mh);
            self.draw(0, 0, mw, mh, &a.gpu_shadow, mw as usize * 4);
        }
    }
}

/// Host clipboard that lives in the viewer process.
struct RemoteClipboard {
    latest: Arc<Mutex<Option<String>>>,
    tx: ViewerTx,
}

impl ClipboardProvider for RemoteClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set_text(&mut self, text: &str) {
        self.tx.send(&ToViewer::ClipboardSet(text.to_string()));
    }
}

/// Built before the machine; finished once devices exist.
pub(crate) struct DisplayServer {
    stream: UnixStream,
    tx: ViewerTx,
    clip_latest: Arc<Mutex<Option<String>>>,
}

pub(crate) struct InputPipes {
    pub kbd: Option<UnixDatagram>,
    pub tablet: Option<UnixDatagram>,
    /// USB HID (xHCI) instead of virtio-input. Exactly one path is used
    /// so guests never see doubled keystrokes.
    pub usb: Option<Arc<Mutex<Xhci>>>,
}

impl InputPipes {
    fn none() -> Self {
        InputPipes { kbd: None, tablet: None, usb: None }
    }
}

fn to_hid(m: &ToVmm) -> Option<HidEvent> {
    Some(match *m {
        ToVmm::Key { code, down } => HidEvent::Key { code, down },
        ToVmm::Pointer { x, y } => HidEvent::Pointer { x, y },
        ToVmm::Button { code, down } => HidEvent::Button { code, down },
        ToVmm::Wheel { dy, .. } => HidEvent::Wheel { dy: dy.clamp(-127, 127) as i8 },
        _ => return None,
    })
}

impl DisplayServer {
    pub fn new(stream: UnixStream) -> std::io::Result<Self> {
        let tx = ViewerTx::spawn(Some(stream.try_clone()?))?;
        Ok(DisplayServer { stream, tx, clip_latest: Arc::new(Mutex::new(None)) })
    }

    /// Create the framebuffer and hand it to the viewer.
    pub fn sink(&self) -> std::io::Result<Arc<FbSink>> {
        let fb = SharedFb::create().map_err(|e| std::io::Error::other(e.to_string()))?;
        send_fd(&self.stream, fb.fd().as_raw_fd()).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Arc::new(FbSink {
            fb,
            mode: Mutex::new((0, 0)),
            arb: Mutex::default(),
            tx: Some(self.tx.clone()),
        }))
    }

    pub fn clipboard(&self) -> Box<dyn ClipboardProvider> {
        Box::new(RemoteClipboard { latest: self.clip_latest.clone(), tx: self.tx.clone() })
    }

    pub fn tx(&self) -> ViewerTx {
        self.tx.clone()
    }

    /// Start reading viewer input. `window(open)`: the microphone gate,
    /// open while a viewer is there. With `listener`, a later viewer
    /// (the app restarted after a crash) can take over: one at a time.
    pub fn run(
        self,
        input: Option<InputPipes>,
        agent: Agent,
        events: Sender<VmEvent>,
        window: Option<Box<dyn Fn(bool) + Send + Sync>>,
        listener: Option<(UnixListener, Arc<FbSink>)>,
        usb: Option<Arc<UsbPorts>>,
    ) -> std::io::Result<()> {
        let ctx = Arc::new(Serve { input, agent, events, window, latest: self.clip_latest, tx: self.tx, usb });
        let c = ctx.clone();
        let first = self.stream;
        std::thread::Builder::new().name("display".into()).spawn(move || serve(first, &c))?;
        if let Some((l, sink)) = listener {
            std::thread::Builder::new().name("viewer-listen".into()).spawn(move || {
                for v in l.incoming() {
                    let Ok(v) = v else { continue };
                    if ctx.tx.attached() {
                        // Someone is watching already.
                        let _ = v.shutdown(std::net::Shutdown::Both);
                        continue;
                    }
                    let Ok(r) = v.try_clone() else { continue };
                    if let Err(e) = ctx.tx.attach(v, sink.fb.fd().as_raw_fd()) {
                        log::warn!("display: viewer: {e}");
                        continue;
                    }
                    log::info!("viewer attached");
                    if let Some(w) = &ctx.window {
                        w(true);
                    }
                    let _ = ctx.events.send(VmEvent::ViewerAttached);
                    let c = ctx.clone();
                    let _ = std::thread::Builder::new().name("display".into()).spawn(move || serve(r, &c));
                }
            })?;
        }
        Ok(())
    }
}

/// What every viewer connection shares.
struct Serve {
    input: Option<InputPipes>,
    agent: Agent,
    events: Sender<VmEvent>,
    window: Option<Box<dyn Fn(bool) + Send + Sync>>,
    latest: Arc<Mutex<Option<String>>>,
    tx: ViewerTx,
    /// Host USB devices plugged in while the VM runs (if it has a USB
    /// controller).
    usb: Option<Arc<UsbPorts>>,
}

/// Read one viewer's input until it goes away.
fn serve(stream: UnixStream, c: &Serve) {
    let mut dec = Decoder::default();
    let mut buf = vec![0u8; 64 << 10];
    // Fds that came with the bytes (a drop's pipe, a USB helper's
    // socket), in order.
    let mut fds = Vec::new();
    loop {
        let n = match vroom_ipc::read_with_fds(&stream, &mut buf, &mut fds) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        dec.push(&buf[..n]);
        loop {
            match dec.next_to_vmm() {
                Ok(Some(ToVmm::Drop { id })) => match (!fds.is_empty()).then(|| fds.remove(0)) {
                    Some(from) => c.agent.send(AgentCmd::Drop { id, from }),
                    None => log::warn!("display: a drop without its pipe"),
                },
                Ok(Some(ToVmm::UsbAttach(dev))) => {
                    let Some(fd) = (!fds.is_empty()).then(|| fds.remove(0)) else {
                        log::warn!("display: a USB device without its helper");
                        continue;
                    };
                    let r = match &c.usb {
                        Some(u) => u.attach(dev, UnixStream::from(fd)),
                        None => Err("this VM has no USB controller".into()),
                    };
                    if let Err(e) = r {
                        log::warn!("usb: {e}");
                        c.tx.send(&ToViewer::Status(format!("USB: {e}")));
                    }
                }
                Ok(Some(ToVmm::UsbDetach { port })) => {
                    if let Some(u) = &c.usb {
                        u.detach(port);
                    }
                }
                Ok(Some(m)) => handle(m, c.input.as_ref(), &c.agent, &c.events, &c.latest),
                Ok(None) => break,
                Err(e) => {
                    log::warn!("display: {e}");
                    break;
                }
            }
        }
    }
    log::info!("viewer disconnected");
    c.tx.detach();
    // Its Coherence windows are gone: the guest gets its taskbar back.
    c.agent.send(AgentCmd::Coherence(false));
    if let Some(w) = &c.window {
        w(false);
    }
    let _ = c.events.send(VmEvent::ViewerClosed);
}

fn send_events(sock: &Option<UnixDatagram>, evs: &[[u8; 8]]) {
    let Some(sock) = sock else { return };
    let mut b = Vec::with_capacity(evs.len() * 8);
    for e in evs {
        b.extend_from_slice(e);
    }
    let _ = sock.send(&b);
}

fn handle(
    m: ToVmm,
    input: Option<&InputPipes>,
    agent: &Agent,
    events: &Sender<VmEvent>,
    latest: &Arc<Mutex<Option<String>>>,
) {
    let syn = event(ev::EV_SYN, 0, 0);
    let none = InputPipes::none();
    let input = input.unwrap_or(&none);
    if let (Some(usb), Some(ev)) = (&input.usb, to_hid(&m)) {
        usb.lock().unwrap_or_else(|e| e.into_inner()).input(ev);
        return;
    }
    match m {
        ToVmm::Key { code, down } => {
            if code < 249 {
                send_events(&input.kbd, &[event(ev::EV_KEY, code, down as i32), syn]);
            }
        }
        ToVmm::Pointer { x, y } => send_events(
            &input.tablet,
            &[event(ev::EV_ABS, ev::ABS_X, x as i32), event(ev::EV_ABS, ev::ABS_Y, y as i32), syn],
        ),
        ToVmm::Button { code, down } => {
            if matches!(code, ev::BTN_LEFT | ev::BTN_RIGHT | ev::BTN_MIDDLE) {
                send_events(&input.tablet, &[event(ev::EV_KEY, code, down as i32), syn]);
            }
        }
        ToVmm::Wheel { dx, dy } => {
            let mut e = Vec::new();
            if dy != 0 {
                e.push(event(ev::EV_REL, ev::REL_WHEEL, dy as i32));
            }
            if dx != 0 {
                e.push(event(ev::EV_REL, ev::REL_HWHEEL, dx as i32));
            }
            e.push(syn);
            send_events(&input.tablet, &e);
        }
        ToVmm::Resize { width, height, scale } => {
            log::debug!("display: viewer asks for {width}x{height} at {scale}%");
            agent.send(AgentCmd::Display { width, height, scale });
        }
        ToVmm::Focus(f) => agent.send(AgentCmd::Focus(f)),
        ToVmm::Clipboard(t) => *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(t),
        ToVmm::Command(c) => {
            let _ = events.send(VmEvent::Ui(c));
        }
        ToVmm::DragEnter { id, items } => agent.send(AgentCmd::DragEnter { id, items }),
        ToVmm::DragCancel { id } => agent.send(AgentCmd::DragCancel { id }),
        // serve() pairs these with their fds.
        ToVmm::Drop { .. } | ToVmm::UsbAttach(_) | ToVmm::UsbDetach { .. } => {}
        ToVmm::Coherence(on) => agent.send(AgentCmd::Coherence(on)),
        ToVmm::Window { id, op } => agent.send(AgentCmd::Window { id, op }),
        ToVmm::StartApp(name) => agent.send(AgentCmd::StartApp(name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn px(sink: &FbSink) -> u32 {
        let (w, h) = *sink.mode.lock().unwrap();
        let mut v = vec![0u32; (w * h) as usize];
        sink.fb.read_into(w, h, &mut v);
        v[0]
    }

    #[test]
    fn a_viewer_that_comes_later_catches_up() {
        let tx = ViewerTx::spawn(None).unwrap();
        tx.send(&ToViewer::Mode { width: 64, height: 32 });
        tx.send(&ToViewer::Status("guest agent connected (t)".into()));
        tx.send(&ToViewer::Status("running".into()));
        tx.send(&ToViewer::Status("paused".into()));
        tx.send(&ToViewer::Status("snapshot: /x".into()));
        tx.send(&ToViewer::Mic(true));
        let (a, b) = UnixStream::pair().unwrap();
        let (pa, _pb) = UnixStream::pair().unwrap();
        assert!(!tx.attached());
        tx.attach(a, pa.as_raw_fd()).unwrap();
        assert!(tx.attached());
        // The screen first, as at start.
        vroom_display::recv_fd(&b).unwrap();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut d = vroom_display::Decoder::default();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while got.len() < 5 {
            let n = (&b).read(&mut buf).unwrap();
            d.push(&buf[..n]);
            while let Ok(Some(m)) = d.next_to_viewer() {
                got.push(m);
            }
        }
        assert_eq!(
            got,
            vec![
                ToViewer::Mode { width: 64, height: 32 },
                ToViewer::Damage { x: 0, y: 0, w: 64, h: 32 },
                ToViewer::Mic(true),
                ToViewer::Status("guest agent connected (t)".into()),
                ToViewer::Status("paused".into()),
            ]
        );
        tx.detach();
        assert!(!tx.attached());
    }

    #[test]
    fn damage_is_merged_not_lost() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let tx = ViewerTx::spawn(Some(a)).unwrap();
        for (x, y) in [(10, 20), (300, 5), (40, 400)] {
            tx.send(&ToViewer::Damage { x, y, w: 8, h: 8 });
        }
        tx.send(&ToViewer::Closed);
        let mut d = vroom_display::Decoder::default();
        let mut got: Option<[u32; 4]> = None;
        let mut buf = [0u8; 256];
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        'read: loop {
            let n = b.read(&mut buf).unwrap();
            d.push(&buf[..n]);
            while let Some(m) = d.next_to_viewer().unwrap() {
                match m {
                    ToViewer::Damage { x, y, w, h } => {
                        let r = [x, y, x + w, y + h];
                        got = Some(got.map_or(r, |u| union(u, r)));
                    }
                    ToViewer::Closed => break 'read,
                    _ => {}
                }
            }
        }
        assert_eq!(got, Some([10, 5, 308, 408]));
    }

    #[test]
    fn icons_wait_one_per_window() {
        // A viewer that doesn't keep up: icons pile up, one per window, capped.
        let mut icons = Icons::new();
        let firsts = (0..ICONS_PENDING as u32 + 10).filter(|i| hold(&mut icons, *i, (1, 1, vec![0; 4]))).count();
        assert_eq!(firsts, 1, "one message queued for them all");
        hold(&mut icons, 3, (1, 1, vec![9; 4]));
        assert_eq!(icons.len(), ICONS_PENDING);
        assert_eq!(icons[&3].2, vec![9; 4], "the latest wins");
    }

    #[test]
    fn the_guest_pointer_goes_with_the_gpu_picture() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let fb = SharedFb::create().unwrap();
        let s = FbSink { fb, mode: Mutex::new((0, 0)), arb: Mutex::default(), tx: Some(ViewerTx::spawn(Some(a)).unwrap()) };
        let t0 = Instant::now();
        s.set_mode(4, 2);
        let img = Cursor::Image { w: 1, h: 1, hot_x: 0, hot_y: 0, rgba: vec![1, 2, 3, 4] };
        s.cursor(&img);
        // The firmware screen takes over: it has no pointer of its own.
        s.ramfb_frame_at(t0 + Duration::from_secs(5), 4, 2, &[0x11; 32], 16, (0, 2));
        // The GPU again: its pointer comes back.
        s.flush_at(t0 + Duration::from_secs(10), 0, 0, 1, 1, &[0x33; 4], 4);
        s.tx.as_ref().unwrap().send(&ToViewer::Closed);
        let mut d = vroom_display::Decoder::default();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        'read: loop {
            let n = b.read(&mut buf).unwrap();
            d.push(&buf[..n]);
            while let Some(m) = d.next_to_viewer().unwrap() {
                match m {
                    ToViewer::Cursor(c) => got.push(c),
                    ToViewer::Closed => break 'read,
                    _ => {}
                }
            }
        }
        let gimg = GuestCursor::Image { w: 1, h: 1, hot_x: 0, hot_y: 0, rgba: vec![1, 2, 3, 4] };
        assert_eq!(got, vec![gimg.clone(), GuestCursor::Unset, gimg]);
    }

    #[test]
    fn the_source_that_changes_wins_after_a_quiet_spell() {
        let s = FbSink::headless().unwrap();
        let t0 = Instant::now();
        let (ramfb, gpu) = ([0x11u8; 4 * 4 * 2], [0x22u8; 4 * 4 * 2]);
        s.ramfb_frame_at(t0, 4, 2, &ramfb, 16, (0, 2));
        assert_eq!(px(&s), 0x11111111);
        // Driver comes up: GPU takes over at once.
        s.set_mode(4, 2);
        s.flush_at(t0, 0, 0, 4, 2, &gpu, 16);
        assert_eq!(px(&s), 0x22222222);
        // Firmware screen changes while the GPU is busy: stays on GPU.
        s.ramfb_frame_at(t0 + Duration::from_secs(1), 4, 2, &ramfb, 16, (0, 2));
        assert_eq!(px(&s), 0x22222222);
        // GPU quiet for a while, ramfb changes: show ramfb.
        s.ramfb_frame_at(t0 + Duration::from_secs(5), 4, 2, &ramfb, 16, (0, 2));
        assert_eq!(px(&s), 0x11111111);
        // A partial GPU update right after: ramfb is not quiet yet.
        s.flush_at(t0 + Duration::from_secs(6), 0, 0, 1, 1, &[0x33; 4], 4);
        assert_eq!(px(&s), 0x11111111);
        // Later: GPU back, redrawn from its copy including that update.
        s.flush_at(t0 + Duration::from_secs(10), 1, 0, 1, 1, &[0x44; 4], 4);
        assert_eq!(px(&s), 0x33333333);
    }
}
