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

use vroom_display::{send_fd, Decoder, GuestCursor, GuestWindow, SharedFb, ToViewer, ToVmm, MAX_HEADS, POINTER_MAX};
use vroom_devices::usb::hid::HidEvent;
use vroom_devices::usb::xhci::Xhci;
use vroom_virtio::devices::gpu::{Cursor, DisplaySink};
use vroom_virtio::devices::input::{self as ev, event};

use crate::agent::{Agent, AgentCmd, ClipboardProvider};
use crate::net::NetPorts;
use crate::usb::UsbPorts;
use crate::vcpu::VmEvent;

/// Sends to the viewer without ever blocking the caller.
///
/// A dedicated thread does the (blocking) socket writes. Damage is
/// coalesced into one rect per monitor (the union of everything since the
/// viewer was last told), so a viewer that stops reading costs us at most
/// a handful of queued messages, never a stall, and one that keeps up
/// only converts what changed.
///
/// The viewer can go (the app crashed) and another come (the app again):
/// what a newcomer needs to catch up is kept (`Sticky`).
#[derive(Clone)]
pub(crate) struct ViewerTx {
    /// Messages, with the viewer generation they were sent in.
    q: std::sync::mpsc::Sender<(u64, ToViewer)>,
    /// Goes up with each viewer that attaches: what was queued before it
    /// is stale (the catch-up replay told it the current state), so it is
    /// dropped, not sent after the replay.
    gen: Arc<std::sync::atomic::AtomicU64>,
    /// Per monitor: x0, y0, x1, y1 not yet sent; Some means a marker is
    /// queued.
    damage: Arc<Mutex<[Option<[u32; 4]>; MAX_HEADS]>>,
    /// The guest's latest window list not yet sent (Coherence): like
    /// damage, a guest that keeps changing it costs one queued message.
    windows: Arc<Mutex<Option<Vec<GuestWindow>>>>,
    /// The viewer shows the first monitor's GL frames from IOSurfaces
    /// (`ToVmm::Surfaces`): off again when it goes.
    surfaces: Arc<std::sync::atomic::AtomicBool>,
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
    /// Each monitor's.
    modes: [Option<(u32, u32)>; MAX_HEADS],
    cursor: Option<GuestCursor>,
    mic: Option<bool>,
    camera: Option<bool>,
    /// Running, paused, or why a suspend failed.
    state: Option<String>,
    /// "guest agent connected (...)".
    agent: Option<String>,
    /// Host USB devices plugged in (once there is a USB controller).
    usb: Option<Vec<vroom_display::UsbPort>>,
    /// The network adapters.
    net: Option<(bool, bool, Vec<vroom_display::NetPort>)>,
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
        let (q, rx) = std::sync::mpsc::channel::<(u64, ToViewer)>();
        let gen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let now = gen.clone();
        let damage = Arc::new(Mutex::new([None::<[u32; 4]>; MAX_HEADS]));
        let pending = damage.clone();
        let windows = Arc::new(Mutex::new(None::<Vec<GuestWindow>>));
        let pending_windows = windows.clone();
        let icons = Arc::new(Mutex::new(Icons::new()));
        let pending_icons = icons.clone();
        let sock = Arc::new(Mutex::new(sock));
        let out = sock.clone();
        std::thread::Builder::new().name("viewer-tx".into()).spawn(move || {
            while let Ok((g, mut m)) = rx.recv() {
                // Damage and window markers take the state as it is now:
                // fine for any viewer. Anything else from before the
                // viewer attached is old news.
                let marker = matches!(m, ToViewer::Damage { .. } | ToViewer::HeadDamage { .. } | ToViewer::Windows(_) | ToViewer::WindowIcon { .. });
                if let Some(head) = damage_head(&m) {
                    let Some([x0, y0, x1, y1]) = pending.lock().unwrap_or_else(|e| e.into_inner())[head].take() else { continue };
                    let (x, y, w, h) = (x0, y0, x1 - x0, y1 - y0);
                    m = if head == 0 { ToViewer::Damage { x, y, w, h } } else { ToViewer::HeadDamage { head: head as u32, x, y, w, h } };
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
                // Checked under the lock attach() holds while it replays.
                if !marker && g < now.load(std::sync::atomic::Ordering::Acquire) {
                    continue;
                }
                for m in batch {
                    if let Some(v) = s.as_mut() {
                        if v.write_all(&m.encode()).is_err() {
                            *s = None;
                        }
                    }
                }
            }
        })?;
        Ok(ViewerTx { q, gen, damage, windows, icons, sock, sticky: Arc::default(), surfaces: Arc::default() })
    }

    /// A viewer is connected.
    pub fn attached(&self) -> bool {
        self.sock.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn detach(&self) {
        self.surfaces.store(false, std::sync::atomic::Ordering::Release);
        *self.sock.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The viewer wants IOSurfaces, or pixels again. Only the app that
    /// started this VMM can have them: it left us a mach port for them.
    fn set_surfaces(&self, on: bool) {
        #[cfg(target_os = "macos")]
        let on = on && vroom_gpu3d::remote::surfaces_possible();
        #[cfg(not(target_os = "macos"))]
        let on = {
            let _ = on;
            false
        };
        log::info!("display: {}", if on { "the app shows GL frames from IOSurfaces" } else { "GL frames as pixels" });
        self.surfaces.store(on, std::sync::atomic::Ordering::Release);
    }

    fn surfaces(&self) -> bool {
        self.surfaces.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A new viewer: the framebuffer first (as at start), then what it
    /// missed, then everything from here on.
    fn gen(&self) -> u64 {
        self.gen.load(std::sync::atomic::Ordering::Acquire)
    }

    fn attach(&self, mut v: UnixStream, fb: RawFd) -> std::io::Result<()> {
        let mut s = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        // A new viewer asks for IOSurfaces itself if it can have them.
        self.surfaces.store(false, std::sync::atomic::Ordering::Release);
        // What is queued now is in the replay below (sticky is updated
        // before a message is queued): drop the queued copies.
        self.gen.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        send_fd(&v, fb).map_err(|e| std::io::Error::other(e.to_string()))?;
        let st = self.sticky.lock().unwrap_or_else(|e| e.into_inner());
        let mut first = Vec::new();
        if let Some((width, height)) = st.modes[0] {
            first.push(ToViewer::Mode { width, height });
            first.push(ToViewer::Damage { x: 0, y: 0, w: width, h: height });
        }
        for (head, m) in st.modes.iter().enumerate().skip(1) {
            if let Some((width, height)) = *m {
                let head = head as u32;
                first.push(ToViewer::HeadMode { head, width, height });
                first.push(ToViewer::HeadDamage { head, x: 0, y: 0, w: width, h: height });
            }
        }
        first.extend(st.cursor.clone().map(ToViewer::Cursor));
        first.extend(st.mic.map(ToViewer::Mic));
        first.extend(st.camera.map(ToViewer::Camera));
        first.extend(st.agent.clone().map(ToViewer::Status));
        first.extend(st.features.map(|features| ToViewer::Agent { features }));
        first.extend(st.state.clone().map(ToViewer::Status));
        first.extend(st.usb.clone().map(ToViewer::Usb));
        first.extend(st.net.clone().map(|(nat_ok, hotplug, ports)| ToViewer::Net { nat_ok, hotplug, ports }));
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
            ToViewer::Mode { width, height } => st.modes[0] = Some((*width, *height)),
            ToViewer::HeadMode { head, width, height } => {
                if let Some(m) = st.modes.get_mut(*head as usize) {
                    *m = Some((*width, *height));
                }
            }
            ToViewer::Cursor(c) => st.cursor = Some(c.clone()),
            ToViewer::Mic(on) => st.mic = Some(*on),
            ToViewer::Camera(on) => st.camera = Some(*on),
            ToViewer::Status(t) if t == "running" || t == "paused" || t.starts_with("suspend failed") || t.starts_with("host disk almost full") => {
                st.state = Some(t.clone())
            }
            ToViewer::Status(t) if t.starts_with("guest agent connected") => st.agent = Some(t.clone()),
            ToViewer::Usb(l) => st.usb = Some(l.clone()),
            ToViewer::Net { nat_ok, hotplug, ports } => st.net = Some((*nat_ok, *hotplug, ports.clone())),
            ToViewer::Agent { features } => st.features = Some(*features),
            _ => {}
        }
    }

    pub fn send(&self, m: &ToViewer) {
        if let Some(head) = damage_head(m) {
            let (ToViewer::Damage { x, y, w, h } | ToViewer::HeadDamage { x, y, w, h, .. }) = *m else { return };
            let r = [x, y, x.saturating_add(w), y.saturating_add(h)];
            let mut d = self.damage.lock().unwrap_or_else(|e| e.into_inner());
            match d[head].as_mut() {
                Some(u) => *u = union(*u, r),
                None => {
                    d[head] = Some(r);
                    // The rect is filled in when it goes out.
                    let _ = self.q.send((self.gen(), m.clone()));
                }
            }
            return;
        }
        if let ToViewer::Windows(list) = m {
            if self.windows.lock().unwrap_or_else(|e| e.into_inner()).replace(list.clone()).is_none() {
                // Filled in when it goes out.
                let _ = self.q.send((self.gen(), ToViewer::Windows(Vec::new())));
            }
            return;
        }
        if let ToViewer::WindowIcon { id, w, h, rgba } = m {
            if hold(&mut self.icons.lock().unwrap_or_else(|e| e.into_inner()), *id, (*w, *h, rgba.clone())) {
                // All waiting icons go out with it.
                let _ = self.q.send((self.gen(), ToViewer::WindowIcon { id: 0, w: 0, h: 0, rgba: Vec::new() }));
            }
            return;
        }
        self.remember(m);
        let _ = self.q.send((self.gen(), m.clone()));
    }
}

/// The monitor a damage message is for (checked: a slot we have).
fn damage_head(m: &ToViewer) -> Option<usize> {
    match *m {
        ToViewer::Damage { .. } => Some(0),
        ToViewer::HeadDamage { head, .. } => Some(head as usize).filter(|&h| h < MAX_HEADS),
        _ => None,
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

/// One guest monitor's picture: its slot of the shared framebuffer. The
/// first monitor's also has the firmware screen (ramfb) to share with.
pub(crate) struct FbSink {
    fb: Arc<SharedFb>,
    head: usize,
    mode: Mutex<(u32, u32)>,
    arb: Mutex<Arbiter>,
    /// None when headless: frames still land in the buffer (screendumps).
    tx: Option<ViewerTx>,
}

impl FbSink {
    /// `heads` monitors with nobody watching.
    pub fn headless(heads: usize) -> std::io::Result<Vec<Arc<Self>>> {
        let fb = SharedFb::create_heads(heads).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self::all(Arc::new(fb), None))
    }

    /// A sink per slot of `fb`.
    fn all(fb: Arc<SharedFb>, tx: Option<ViewerTx>) -> Vec<Arc<Self>> {
        (0..fb.heads())
            .map(|head| Arc::new(FbSink { fb: fb.clone(), head, mode: Mutex::new((0, 0)), arb: Mutex::default(), tx: tx.clone() }))
            .collect()
    }

    /// What the guest shows on this monitor now (0x0: off).
    pub fn mode(&self) -> (u32, u32) {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
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
        log::debug!("display: monitor {} mode {width}x{height}", self.head + 1);
        if let Some(tx) = &self.tx {
            tx.send(&if self.head == 0 { ToViewer::Mode { width, height } } else { ToViewer::HeadMode { head: self.head as u32, width, height } });
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
        self.fb.write_rect_on(self.head, mw, x, y, w, h, rows, stride);
        if let Some(tx) = &self.tx {
            tx.send(&if self.head == 0 { ToViewer::Damage { x, y, w, h } } else { ToViewer::HeadDamage { head: self.head as u32, x, y, w, h } });
        }
    }

    /// Write the current screen as PNG.
    pub fn screendump(&self, path: &std::path::Path) -> std::io::Result<(u32, u32)> {
        let (w, h) = *self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if w == 0 || h == 0 {
            return Err(std::io::Error::other("display is off"));
        }
        let mut px = vec![0u32; w as usize * h as usize];
        self.fb.read_into_on(self.head, w, h, &mut px);
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

    fn wants_surfaces(&self) -> bool {
        // The first monitor, while the GPU (not the firmware) is on it.
        self.head == 0
            && self.tx.as_ref().is_some_and(|t| t.surfaces())
            && self.arb.lock().unwrap_or_else(|e| e.into_inner()).gpu_on_screen
    }

    fn surface(&self, gen: u32, slot: u32, x: u32, y: u32, w: u32, h: u32) {
        let mut a = self.arb.lock().unwrap_or_else(|e| e.into_inner());
        let (mw, mh) = a.gpu_mode;
        if self.head != 0 || x.saturating_add(w) > mw || y.saturating_add(h) > mh {
            return;
        }
        a.last_gpu = Some(Instant::now());
        if a.gpu_on_screen {
            drop(a);
            self.send(ToViewer::Surface { gen, slot, x, y, w, h });
        }
    }

    fn cursor(&self, c: &Cursor) {
        // One pointer for the viewer: the first monitor's GPU says.
        if self.head > 0 {
            return;
        }
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

    /// Create the framebuffer for `heads` monitors and hand it to the
    /// viewer. A sink per monitor.
    pub fn sinks(&self, heads: usize) -> std::io::Result<Vec<Arc<FbSink>>> {
        let fb = SharedFb::create_heads(heads).map_err(|e| std::io::Error::other(e.to_string()))?;
        send_fd(&self.stream, fb.fd().as_raw_fd()).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(FbSink::all(Arc::new(fb), Some(self.tx.clone())))
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
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        self,
        input: Option<InputPipes>,
        agent: Agent,
        events: Sender<VmEvent>,
        window: Option<Box<dyn Fn(bool) + Send + Sync>>,
        listener: Option<UnixListener>,
        screens: Vec<Arc<FbSink>>,
        windows: bool,
        usb: Option<Arc<UsbPorts>>,
        net: Option<Arc<NetPorts>>,
    ) -> std::io::Result<()> {
        let ctx = Arc::new(Serve { input, agent, events, window, latest: self.clip_latest, tx: self.tx, screens, windows, ptr: Mutex::default(), usb, net });
        let c = ctx.clone();
        let first = self.stream;
        std::thread::Builder::new().name("display".into()).spawn(move || serve(first, &c))?;
        if let Some(l) = listener {
            std::thread::Builder::new().name("viewer-listen".into()).spawn(move || {
                for v in l.incoming() {
                    let Ok(v) = v else { continue };
                    if ctx.tx.attached() {
                        // Someone is watching already.
                        let _ = v.shutdown(std::net::Shutdown::Both);
                        continue;
                    }
                    let Ok(r) = v.try_clone() else { continue };
                    let Some(fb) = ctx.screens.first().map(|s| s.fb.fd().as_raw_fd()) else { continue };
                    if let Err(e) = ctx.tx.attach(v, fb) {
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
    /// Every monitor, the first first: where pointers land.
    screens: Vec<Arc<FbSink>>,
    /// A Windows guest: its tablet only reaches the main monitor.
    windows: bool,
    ptr: Mutex<Ptr>,
    /// Host USB devices plugged in while the VM runs (if it has a USB
    /// controller).
    usb: Option<Arc<UsbPorts>>,
    /// Network adapters changed while the VM runs.
    net: Option<Arc<NetPorts>>,
}

/// Where the viewer's pointer is (which monitor), and its buttons.
#[derive(Debug, Default)]
struct Ptr {
    head: usize,
    buttons: u8,
    at: (u16, u16),
}

impl Serve {
    /// Pointer input, where it should go. Windows maps the USB tablet to
    /// its main monitor only: there the tablet, on the others the guest
    /// agent moves the pointer (as SPICE's does). Without an agent that
    /// can, or another guest: the tablet, over the whole desktop.
    fn route(&self, m: ToVmm) -> Option<ToVmm> {
        let mut p = self.ptr.lock().unwrap_or_else(|e| e.into_inner());
        if let ToVmm::Button { code, down } = m {
            let bit = match code {
                ev::BTN_LEFT => 1,
                ev::BTN_RIGHT => 2,
                ev::BTN_MIDDLE => 4,
                _ => 0,
            };
            p.buttons = if down { p.buttons | bit } else { p.buttons & !bit };
        }
        let agent = self.windows && self.screens.len() > 1 && self.agent.features() & vroom_proto::features::MONITORS != 0;
        if !agent {
            return Some(desktop_pointer(m, &self.screens));
        }
        let mouse = |p: &Ptr, wheel: i8| AgentCmd::Mouse { x: p.at.0, y: p.at.1, buttons: p.buttons, wheel };
        match m {
            ToVmm::Pointer { .. } => {
                p.head = 0;
                Some(m)
            }
            ToVmm::HeadPointer { head, x, y } => {
                let modes: Vec<(u32, u32)> = self.screens.iter().map(|s| s.mode()).collect();
                let (gx, gy) = place(&modes, head as usize, x, y);
                let wide = |v: u16| (v as u32 * 65535 / POINTER_MAX as u32) as u16;
                p.head = head as usize;
                p.at = (wide(gx), wide(gy));
                self.agent.send(mouse(&p, 0));
                None
            }
            ToVmm::Button { .. } if p.head > 0 => {
                self.agent.send(mouse(&p, 0));
                None
            }
            ToVmm::Wheel { dy, .. } if p.head > 0 => {
                self.agent.send(mouse(&p, dy.clamp(-127, 127) as i8));
                None
            }
            m => Some(m),
        }
    }
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
                Ok(Some(m @ (ToVmm::UsbDisk { .. } | ToVmm::UsbFiles { .. }))) => {
                    let Some(fd) = (!fds.is_empty()).then(|| fds.remove(0)) else {
                        log::warn!("display: a USB disk without its fd");
                        continue;
                    };
                    let r = match (&c.usb, m) {
                        (Some(u), ToVmm::UsbDisk { dev, readonly }) => u.attach_disk(dev, fd, readonly),
                        (Some(u), ToVmm::UsbFiles { dev, readonly, label, token }) => u.attach_files(dev, fd, readonly, &label, &token),
                        _ => Err("this VM has no USB controller".into()),
                    };
                    if let Err(e) = r {
                        log::warn!("usb: {e}");
                        c.tx.send(&ToViewer::Status(format!("USB: {e}")));
                    }
                }
                Ok(Some(ToVmm::NetConnect { nic, on })) => {
                    let r = c.net.as_ref().map_or(Err("no network adapters".into()), |n| n.connect(nic, on));
                    if let Err(e) = r {
                        c.tx.send(&ToViewer::Status(format!("Network: {e}")));
                    }
                }
                Ok(Some(ToVmm::NetSet { nic, live, port_mac })) => {
                    // Bridged and host-only bring their vroom-netd socket.
                    let sock = if live.kind == vroom_display::NetKind::Nat {
                        None
                    } else {
                        match (!fds.is_empty()).then(|| fds.remove(0)) {
                            Some(fd) => Some(fd),
                            None => {
                                log::warn!("display: a network change without its socket");
                                continue;
                            }
                        }
                    };
                    let r = c.net.as_ref().map_or(Err("no network adapters".into()), |n| n.set(nic, live, port_mac, sock));
                    if let Err(e) = r {
                        log::warn!("net: adapter {}: {e}", nic + 1);
                        c.tx.send(&ToViewer::Status(format!("Network: adapter {}: {e}", nic + 1)));
                    }
                }
                Ok(Some(ToVmm::NetAdd { live, mac, port_mac })) => {
                    let sock = if live.kind == vroom_display::NetKind::Nat {
                        None
                    } else {
                        match (!fds.is_empty()).then(|| fds.remove(0)) {
                            Some(fd) => Some(fd),
                            None => {
                                log::warn!("display: a new network adapter without its socket");
                                continue;
                            }
                        }
                    };
                    let r = c.net.as_ref().map_or(Err("no network adapters".into()), |n| n.add(live, mac, port_mac, sock));
                    if let Err(e) = r {
                        log::warn!("net: new adapter: {e}");
                        c.tx.send(&ToViewer::Status(format!("Network: new adapter: {e}")));
                    }
                }
                Ok(Some(ToVmm::NetRemove { nic })) => {
                    let r = c.net.as_ref().map_or(Err("no network adapters".into()), |n| n.remove(nic));
                    if let Err(e) = r {
                        log::warn!("net: adapter {}: {e}", nic + 1);
                        c.tx.send(&ToViewer::Status(format!("Network: adapter {}: {e}", nic + 1)));
                    }
                }
                Ok(Some(ToVmm::Surfaces(on))) => c.tx.set_surfaces(on),
                Ok(Some(ToVmm::AgentUpdate)) => match (!fds.is_empty()).then(|| fds.remove(0)) {
                    Some(from) => c.agent.send(AgentCmd::Update { from }),
                    None => log::warn!("display: guest tools without their pipe"),
                },
                Ok(Some(ToVmm::DragOutGet { id, item })) => match (!fds.is_empty()).then(|| fds.remove(0)) {
                    Some(to) => c.agent.send(AgentCmd::DragOutGet { id, item, to }),
                    None => log::warn!("display: a drag out without its pipe"),
                },
                Ok(Some(m)) => {
                    if let Some(m) = c.route(m) {
                        handle(m, c.input.as_ref(), &c.agent, &c.events, &c.latest)
                    }
                }
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

/// With several monitors the guest's tablet spans its whole desktop:
/// the monitors side by side, top edges lined up, in order (the layout
/// the guest agent asks for). A pointer on one monitor's window becomes
/// a point on that desktop. One monitor on: as it is.
fn desktop_pointer(m: ToVmm, screens: &[Arc<FbSink>]) -> ToVmm {
    let (head, x, y) = match m {
        ToVmm::Pointer { x, y } => (0, x, y),
        ToVmm::HeadPointer { head, x, y } => (head as usize, x, y),
        m => return m,
    };
    let modes: Vec<(u32, u32)> = screens.iter().map(|s| s.mode()).collect();
    let (x, y) = place(&modes, head, x, y);
    ToVmm::Pointer { x, y }
}

/// Monitor `head`'s point `x`, `y` (0..=POINTER_MAX) on the desktop of
/// `modes` (0x0: off).
fn place(modes: &[(u32, u32)], head: usize, x: u16, y: u16) -> (u16, u16) {
    let on = |m: &&(u32, u32)| m.0 > 0 && m.1 > 0;
    let total_w: u64 = modes.iter().filter(on).map(|m| m.0 as u64).sum();
    let max_h = modes.iter().filter(on).map(|m| m.1).max().unwrap_or(0) as u64;
    let Some(&(w, h)) = modes.get(head).filter(|m| on(m)) else { return (x, y) };
    if total_w <= w as u64 && max_h <= h as u64 {
        return (x, y);
    }
    let left: u64 = modes[..head].iter().filter(on).map(|m| m.0 as u64).sum();
    let max = POINTER_MAX as u64;
    // Pixel on the desktop, then back to the tablet's range.
    let px = left + x as u64 * (w as u64 - 1) / max;
    let py = y as u64 * (h as u64 - 1) / max;
    let gx = px * max / (total_w - 1).max(1);
    let gy = py * max / (max_h - 1).max(1);
    (gx.min(max) as u16, gy.min(max) as u16)
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
            agent.send(AgentCmd::Display { head: 0, width, height, scale });
        }
        ToVmm::HeadResize { head, width, height, scale } => {
            log::debug!("display: viewer asks for {width}x{height} at {scale}% on monitor {}", head + 1);
            agent.send(AgentCmd::Display { head: head as usize, width, height, scale });
        }
        // desktop_pointer() made it a plain pointer.
        ToVmm::HeadPointer { .. } => {}
        ToVmm::Focus(f) => agent.send(AgentCmd::Focus(f)),
        ToVmm::Clipboard(t) => *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(t),
        ToVmm::Command(c) => {
            let _ = events.send(VmEvent::Ui(c));
        }
        ToVmm::DragEnter { id, items } => agent.send(AgentCmd::DragEnter { id, items }),
        ToVmm::DragCancel { id } => agent.send(AgentCmd::DragCancel { id }),
        // serve() pairs these with their fds.
        ToVmm::Drop { .. }
        | ToVmm::UsbAttach(_)
        | ToVmm::UsbDetach { .. }
        | ToVmm::UsbDisk { .. }
        | ToVmm::UsbFiles { .. }
        | ToVmm::DragOutGet { .. }
        | ToVmm::NetConnect { .. }
        | ToVmm::NetSet { .. }
        | ToVmm::NetAdd { .. }
        | ToVmm::NetRemove { .. }
        | ToVmm::Surfaces(_)
        | ToVmm::AgentUpdate => {}
        ToVmm::Coherence(on) => agent.send(AgentCmd::Coherence(on)),
        ToVmm::Window { id, op } => agent.send(AgentCmd::Window { id, op }),
        ToVmm::DragOut { id, end: false } => agent.send(AgentCmd::DragOut { id }),
        ToVmm::DragOut { id, end: true } => agent.send(AgentCmd::DragOutEnd { id }),
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
        sink.fb.read_into_on(sink.head, w, h, &mut v);
        v[0]
    }

    #[test]
    fn pointers_land_on_their_monitor() {
        // One monitor: untouched.
        assert_eq!(place(&[(1280, 800)], 0, 100, 200), (100, 200));
        assert_eq!(place(&[(1280, 800), (0, 0)], 0, 100, 200), (100, 200));
        // Two side by side, the second smaller.
        let m = [(1000, 800), (1000, 400)];
        let max = POINTER_MAX;
        assert_eq!(place(&m, 0, 0, 0), (0, 0));
        // Right edge of the first: just left of the middle.
        let (x, y) = place(&m, 0, max, max);
        assert!((16300..16400).contains(&x) && y == max, "{x} {y}");
        // Left edge of the second: the middle; its bottom is half way down.
        let (x, y) = place(&m, 1, 0, max);
        assert!((16390..16420).contains(&x) && (16340..16420).contains(&y), "{x} {y}");
        assert_eq!(place(&m, 1, max, 0), (max, 0));
        // A monitor that's off, or not there: as it came.
        assert_eq!(place(&[(1000, 800), (0, 0), (1000, 800)], 2, 0, 0).0, 16391);
        assert_eq!(place(&m, 3, 5, 5), (5, 5));
    }

    #[test]
    fn monitors_get_their_own_messages() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let tx = ViewerTx::spawn(Some(a)).unwrap();
        let s = FbSink::all(Arc::new(SharedFb::create_heads(2).unwrap()), Some(tx.clone()));
        s[1].set_mode(4, 2);
        s[1].flush(1, 1, 1, 1, &[7; 4], 4);
        s[0].set_mode(8, 8);
        tx.send(&ToViewer::Closed);
        let mut d = vroom_display::Decoder::default();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        'read: loop {
            let n = b.read(&mut buf).unwrap();
            d.push(&buf[..n]);
            while let Some(m) = d.next_to_viewer().unwrap() {
                if m == ToViewer::Closed {
                    break 'read;
                }
                got.push(m);
            }
        }
        assert_eq!(
            got,
            vec![
                ToViewer::HeadMode { head: 1, width: 4, height: 2 },
                ToViewer::HeadDamage { head: 1, x: 1, y: 1, w: 1, h: 1 },
                ToViewer::Mode { width: 8, height: 8 },
            ]
        );
        let mut r = [0u8; 4];
        s[1].fb.read_rect_on(1, 4, 1, 1, 1, 1, &mut r, 4);
        assert_eq!(r, [7; 4]);
        // A viewer that comes later gets both monitors.
        let (a, b) = UnixStream::pair().unwrap();
        tx.detach();
        tx.attach(a, s[0].fb.fd().as_raw_fd()).unwrap();
        vroom_display::recv_fd(&b).unwrap();
        let mut d = vroom_display::Decoder::default();
        let mut got = Vec::new();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        while got.len() < 4 {
            let n = (&b).read(&mut buf).unwrap();
            d.push(&buf[..n]);
            while let Ok(Some(m)) = d.next_to_viewer() {
                got.push(m);
            }
        }
        assert_eq!(got[2], ToViewer::HeadMode { head: 1, width: 4, height: 2 });
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
        // Let the sender drop what went to nobody first.
        std::thread::sleep(Duration::from_millis(100));
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
        let fb = Arc::new(SharedFb::create().unwrap());
        let s = FbSink { fb, head: 0, mode: Mutex::new((0, 0)), arb: Mutex::default(), tx: Some(ViewerTx::spawn(Some(a)).unwrap()) };
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
        let s = FbSink::headless(1).unwrap().remove(0);
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
