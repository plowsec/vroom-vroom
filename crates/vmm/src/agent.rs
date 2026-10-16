//! Host side of the guest agent channel: clipboard, time sync, shutdown,
//! display size and scale.
//!
//! Clipboard rules (see docs/SECURITY.md):
//! - Text only, size-capped, each direction can be turned off.
//! - Automatic sync only while the VM has focus. Otherwise a guest could
//!   read every host copy (passwords) or plant content right before the
//!   user pastes into a host terminal.
//! - Guest updates are rate limited.
//! - Without a GUI (no focus concept) sync is explicit: push / pull.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vroom_config::Clipboard as ClipPolicy;
use vroom_proto::{features, Decoder, Message, Monitor, SCALE_MAX, SCALE_MIN};
use vroom_display::ToViewer;
use vroom_virtio::devices::gpu::GpuControl;

/// Host clipboard access, provided by the frontend.
pub trait ClipboardProvider: Send {
    fn get_text(&mut self) -> Option<String>;
    fn set_text(&mut self, text: &str);
}

/// Commands from the frontend.
#[derive(Debug)]
pub enum AgentCmd {
    /// The VM window gained/lost focus (enables automatic sync).
    Focus(bool),
    /// Send the current host clipboard to the guest now.
    PushClipboard,
    /// Copy the guest's latest clipboard to the host now.
    PullClipboard,
    SyncTime,
    /// Monitor `head`'s window size. `scale` 0: in guest pixels, the
    /// guest's UI scale is left alone. Else (HiDPI) in physical pixels,
    /// with the scale (percent) the guest's UI needs to look the same
    /// size; a guest that can't scale gets the size divided by it
    /// instead. 0x0 (not the first monitor): its window is closed.
    Display { head: usize, width: u32, height: u32, scale: u32 },
    Shutdown { reboot: bool },
    /// Turn the guest's 3D display device on or off (Windows, around a
    /// suspend: Direct3D state can't be saved).
    Gpu3d(bool),
    /// Files are dragged over the window (names only; see vroom-proto).
    DragEnter { id: u32, items: Vec<(String, bool)> },
    DragCancel { id: u32 },
    /// Dropped: the viewer writes them into this pipe, as agent frames.
    Drop { id: u32, from: std::os::fd::OwnedFd },
    /// The pointer on the whole desktop (see `Message::Mouse`).
    Mouse { x: u16, y: u16, buttons: u8, wheel: i8 },
    Stop,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentStatus {
    pub connected: bool,
    /// Guest-supplied, sanitized: printable ASCII, 64 chars at most.
    pub agent: String,
    pub features: u64,
    /// Hellos seen: goes up on every (re)connect, e.g. after a guest reboot.
    pub sessions: u32,
    /// Whether the guest says its 3D display device is on (after we asked).
    pub gpu3d: Option<bool>,
}

const POLL: Duration = Duration::from_millis(250);
const GUEST_CLIP_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Handle held by the VMM / frontend.
#[derive(Debug, Clone)]
pub struct Agent {
    tx: Sender<AgentCmd>,
    status: Arc<Mutex<AgentStatus>>,
}

impl Agent {
    pub fn send(&self, cmd: AgentCmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn status(&self) -> AgentStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The connected agent's features (0: none connected).
    pub fn features(&self) -> u64 {
        let s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if s.connected { s.features } else { 0 }
    }
}

struct Service {
    stream: UnixStream,
    /// Writes to the guest: ours, and a drop's (frame by frame).
    out: Arc<Mutex<UnixStream>>,
    policy: ClipPolicy,
    clip: Option<Box<dyn ClipboardProvider>>,
    rx: Receiver<AgentCmd>,
    status: Arc<Mutex<AgentStatus>>,
    dec: Decoder,
    focus: bool,
    /// Last text the guest reported.
    guest_clip: Option<String>,
    last_guest_update: Option<Instant>,
    /// What we last saw on / wrote to the host clipboard (echo suppression).
    host_seen: Option<String>,
    screen: Screen,
    /// Each window's last size request, redone when an agent connects.
    display: Vec<Option<(u32, u32, u32)>>,
    /// What the guest agent gets for it, sent once the GPU is ready (after
    /// a replug, see `GpuControl::replugs_done`), and the replug count it
    /// was last sent at.
    mode: Vec<Message>,
    mode_sent: Option<u32>,
}

/// The monitors whose sizes the windows drive. Resizes go through the
/// agent service: whether the guest gets physical pixels depends on its
/// agent.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    /// Each monitor's GPU and scanout on it, the first first. Linux
    /// guests: one GPU, a scanout each. Windows: a GPU each.
    pub heads: Vec<(GpuControl, usize)>,
    /// Largest mode (in pixels) the guest's driver takes; bigger windows
    /// get a smaller mode of the same shape. 0: no limit.
    pub max_pixels: u64,
    /// The VM has 3D: an agent that can switch the guest's 3D display
    /// device is asked to have it on whenever it connects (a suspend
    /// turned it off, and the VM may have been resumed, or killed).
    pub gpu3d: bool,
    /// The window, for answers about dropped files.
    pub(crate) viewer: Option<crate::display::ViewerTx>,
}

/// Windows' virtio-gpu driver (viogpudo) sizes its framebuffer when it
/// starts: 16 MiB unless the screen is already bigger then. A larger mode
/// fails and leaves the driver unable to change modes until a reboot.
pub const WINDOWS_MAX_PIXELS: u64 = (16 << 20) / 4;

/// Start the service on the host end of the agent port.
pub fn spawn(stream: UnixStream, policy: ClipPolicy, clip: Option<Box<dyn ClipboardProvider>>, screen: Screen) -> std::io::Result<Agent> {
    let (tx, rx) = mpsc::channel();
    let status = Arc::new(Mutex::new(AgentStatus::default()));
    stream.set_read_timeout(Some(POLL))?;
    let max = policy.max_bytes as usize + 64;
    let out = Arc::new(Mutex::new(stream.try_clone()?));
    let svc = Service {
        stream,
        out,
        policy,
        clip,
        rx,
        status: status.clone(),
        dec: Decoder::new(max),
        focus: false,
        guest_clip: None,
        last_guest_update: None,
        host_seen: None,
        display: vec![None; screen.heads.len().max(1)],
        screen,
        mode: Vec::new(),
        mode_sent: None,
    };
    std::thread::Builder::new().name("agent".into()).spawn(move || svc.run())?;
    Ok(Agent { tx, status })
}

impl Service {
    fn send(&mut self, m: &Message) {
        let _ = self.out.lock().unwrap_or_else(|e| e.into_inner()).write_all(&m.encode());
    }

    fn to_viewer(&self, m: ToViewer) {
        if let Some(v) = &self.screen.viewer {
            v.send(&m);
        }
    }

    fn hello(&mut self) {
        let f = features::CLIPBOARD_TEXT | features::TIME_SYNC | features::RESOLUTION | features::SHUTDOWN;
        self.send(&Message::Hello { features: f, agent: format!("vroom-host {}", env!("CARGO_PKG_VERSION")) });
    }

    fn run(mut self) {
        self.hello();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    self.dec.push(&buf[..n]);
                    loop {
                        match self.dec.next() {
                            Ok(Some(m)) => self.on_message(m),
                            Ok(None) => break,
                            Err(e) => log::warn!("agent: bad message from guest: {e}"),
                        }
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => return,
            }
            loop {
                match self.rx.try_recv() {
                    Ok(AgentCmd::Stop) | Err(TryRecvError::Disconnected) => return,
                    Ok(c) => self.on_cmd(c),
                    Err(TryRecvError::Empty) => break,
                }
            }
            // Each GPU once (its first scanout).
            for (g, _) in self.screen.heads.iter().filter(|h| h.1 == 0) {
                g.tick();
            }
            self.send_mode();
            if self.focus {
                self.auto_push();
            }
        }
    }

    fn on_message(&mut self, m: Message) {
        match m {
            Message::Hello { features, agent } => {
                let agent: String = agent.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(64).collect();
                log::info!("guest agent connected: {agent}");
                let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
                let sessions = s.sessions.wrapping_add(1);
                *s = AgentStatus { connected: true, agent, features, sessions, gpu3d: None };
                drop(s);
                if self.screen.gpu3d && features & features::GPU3D != 0 {
                    self.send(&Message::Gpu3d { on: true });
                }
                // A fresh agent: boot, or a guest that slept. Set its clock
                // (Windows also reads the RTC as local time until told).
                if features & vroom_proto::features::TIME_SYNC != 0 {
                    self.sync_time();
                }
                // Now it may scale: give it the window's real size.
                self.apply_display();
            }
            Message::ClipboardText(t) => self.guest_clipboard(t),
            Message::Ping(v) => self.send(&Message::Pong(v)),
            Message::Gpu3d { on } => {
                log::info!("guest: 3D display device {}", if on { "on" } else { "off" });
                self.status.lock().unwrap_or_else(|e| e.into_inner()).gpu3d = Some(on);
            }
            Message::DragReady { id, live } => self.to_viewer(ToViewer::DragReady { id, live }),
            Message::DropDone { id, ok, place } => {
                let place: String = place.chars().filter(|c| !c.is_control()).take(200).collect();
                log::info!("guest: drop {id} {}{place}", if ok { "done " } else { "failed: " });
                self.to_viewer(ToViewer::DropDone { id, ok, place });
            }
            // Host-bound only; a guest sending these is ignored.
            _ => {}
        }
    }

    fn can_scale(&self) -> bool {
        let s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.connected && s.features & features::SCALE != 0
    }

    fn apply_display(&mut self) {
        let Some((w, h, scale)) = self.display[0] else { return };
        let (can_scale, max) = (self.can_scale(), self.screen.max_pixels);
        let (w, h, set_scale) = guest_display(w, h, scale, can_scale, max);
        if let Some((g, sid)) = self.screen.heads.first() {
            g.resize_head(*sid, w, h);
        }
        let mut sizes = vec![Some((w, h))];
        for (i, (g, sid)) in self.screen.heads.iter().enumerate().skip(1) {
            let size = match self.display.get(i).copied().flatten() {
                Some((0, _, _)) => None,
                Some((w, h, s)) => {
                    let (w, h, _) = guest_display(w, h, s, can_scale, max);
                    Some((w, h))
                }
                // No window yet: what the GPU offers.
                None => g.head_size(*sid),
            };
            g.resize_head(*sid, size.map_or(0, |s| s.0), size.map_or(0, |s| s.1));
            sizes.push(size);
        }
        self.mode = if sizes.len() > 1 && self.status().features & features::MONITORS != 0 {
            vec![Message::SetLayout { monitors: layout(&sizes) }]
        } else {
            vec![Message::SetResolution { width: w, height: h }]
        };
        self.mode.extend(set_scale.map(|p| Message::SetScale { percent: p }));
        self.mode_sent = None;
        self.send_mode();
    }

    fn send_mode(&mut self) {
        // Every GPU's replugs (each once), once none is in the middle of one.
        let mut n = 0u32;
        for (g, _) in self.screen.heads.iter().filter(|h| h.1 == 0) {
            match g.replugs_done() {
                Some(r) => n = n.wrapping_add(r),
                None => return,
            }
        }
        if self.mode.is_empty() || self.mode_sent == Some(n) {
            return;
        }
        self.mode_sent = Some(n);
        for m in self.mode.clone() {
            self.send(&m);
        }
    }

    fn sync_time(&mut self) {
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        self.send(&Message::TimeSync { unix_secs: d.as_secs(), nanos: d.subsec_nanos() });
    }

    fn guest_clipboard(&mut self, t: String) {
        if !self.policy.guest_to_host || t.len() > self.policy.max_bytes as usize {
            return;
        }
        let now = Instant::now();
        if self.last_guest_update.is_some_and(|l| now - l < GUEST_CLIP_MIN_INTERVAL) {
            return;
        }
        self.last_guest_update = Some(now);
        if self.focus {
            if let Some(c) = self.clip.as_mut() {
                c.set_text(&t);
                self.host_seen = Some(t.clone());
            }
        }
        self.guest_clip = Some(t);
    }

    fn push_text(&mut self, t: String) {
        if !self.policy.host_to_guest || t.len() > self.policy.max_bytes as usize {
            return;
        }
        if self.guest_clip.as_deref() == Some(t.as_str()) {
            return;
        }
        self.send(&Message::ClipboardText(t.clone()));
        self.guest_clip = Some(t);
    }

    fn auto_push(&mut self) {
        let Some(t) = self.clip.as_mut().and_then(|c| c.get_text()) else { return };
        if self.host_seen.as_deref() != Some(t.as_str()) {
            self.host_seen = Some(t.clone());
            self.push_text(t);
        }
    }

    fn on_cmd(&mut self, c: AgentCmd) {
        match c {
            AgentCmd::Focus(f) => {
                self.focus = f;
                if f {
                    self.auto_push();
                }
            }
            AgentCmd::PushClipboard => {
                if let Some(t) = self.clip.as_mut().and_then(|c| c.get_text()) {
                    self.host_seen = Some(t.clone());
                    self.push_text(t);
                }
            }
            AgentCmd::PullClipboard => {
                if let (Some(t), Some(c)) = (self.guest_clip.clone(), self.clip.as_mut()) {
                    if self.policy.guest_to_host {
                        c.set_text(&t);
                        self.host_seen = Some(t);
                    }
                }
            }
            AgentCmd::SyncTime => self.sync_time(),
            AgentCmd::Display { head, width, height, scale } => {
                let off = head > 0 && width == 0 && height == 0;
                if (!off && (width == 0 || height == 0)) || (scale != 0 && !(SCALE_MIN..=SCALE_MAX).contains(&scale)) {
                    return;
                }
                let Some(d) = self.display.get_mut(head) else { return };
                *d = Some((width, height, scale));
                self.apply_display();
            }
            AgentCmd::Shutdown { reboot } => self.send(&Message::Shutdown { reboot }),
            AgentCmd::Gpu3d(on) => {
                self.status.lock().unwrap_or_else(|e| e.into_inner()).gpu3d = None;
                self.send(&Message::Gpu3d { on });
            }
            AgentCmd::DragEnter { id, items } => {
                let s = self.status();
                let why = if !s.connected {
                    Some("no guest agent is running")
                } else if s.features & features::FILE_DROP == 0 {
                    Some("the guest agent is too old for drag and drop (update the vroom tools in the guest)")
                } else if items.is_empty() || items.len() > vroom_proto::DROP_ITEMS_MAX || !items.iter().all(|(n, _)| vroom_proto::drop_name_ok(n)) {
                    Some("a name the guest can't take")
                } else {
                    None
                };
                match why {
                    Some(w) => self.to_viewer(ToViewer::DropDone { id, ok: false, place: w.into() }),
                    None => self.send(&Message::DragEnter { id, items }),
                }
            }
            AgentCmd::DragCancel { id } => self.send(&Message::DragCancel { id }),
            AgentCmd::Mouse { x, y, buttons, wheel } => self.send(&Message::Mouse { x, y, buttons, wheel }),
            AgentCmd::Drop { id, from } => {
                let out = self.out.clone();
                let r = std::thread::Builder::new().name("agent-drop".into()).spawn(move || forward_drop(id, from, &out));
                if r.is_err() {
                    self.send(&Message::DragCancel { id });
                }
            }
            AgentCmd::Stop => {}
        }
    }
}

impl Service {
    fn status(&self) -> AgentStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Pass a drop's frames from the viewer's pipe to the guest, checked:
/// only this drop's entries, data and end. A pipe that ends early (the
/// viewer gave up, or went) cancels it in the guest.
fn forward_drop(id: u32, from: std::os::fd::OwnedFd, out: &Mutex<UnixStream>) {
    let mut from = std::fs::File::from(from);
    let mut dec = Decoder::new(vroom_proto::DROP_CHUNK + vroom_proto::DROP_PATH_MAX + 64);
    let mut buf = vec![0u8; vroom_proto::DROP_CHUNK];
    let (mut bytes, t0) = (0u64, Instant::now());
    let send = |m: &Message| out.lock().unwrap_or_else(|e| e.into_inner()).write_all(&m.encode());
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        dec.push(&buf[..n]);
        loop {
            let m = match dec.next() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("drop {id}: bad frame from the viewer: {e}");
                    let _ = send(&Message::DragCancel { id });
                    return;
                }
            };
            let ok = match &m {
                Message::DropEntry { id: i, .. } => *i == id,
                Message::DropData { id: i, data } => {
                    bytes += data.len() as u64;
                    *i == id
                }
                Message::DropEnd { id: i } => *i == id,
                _ => false,
            };
            if !ok {
                log::warn!("drop {id}: unexpected frame from the viewer");
                let _ = send(&Message::DragCancel { id });
                return;
            }
            if send(&m).is_err() {
                return;
            }
            if matches!(m, Message::DropEnd { .. }) {
                log::info!("drop {id}: {bytes} bytes to the guest in {:?}", t0.elapsed());
                return;
            }
        }
    }
    log::info!("drop {id}: given up by the viewer");
    let _ = send(&Message::DragCancel { id });
}

/// Monitors side by side, top edges lined up, in order: what the display
/// server maps the pointer for.
fn layout(sizes: &[Option<(u32, u32)>]) -> Vec<Option<Monitor>> {
    let mut x = 0i32;
    sizes
        .iter()
        .map(|s| {
            s.map(|(w, h)| {
                let m = Monitor { x, y: 0, width: w, height: h };
                x = x.saturating_add(w as i32);
                m
            })
        })
        .collect()
}

/// What the guest gets for a window of `w` x `h` at `scale`: its size,
/// and the scale message for its agent, if any.
fn guest_display(w: u32, h: u32, scale: u32, can_scale: bool, max_pixels: u64) -> (u32, u32, Option<u32>) {
    let (w, h, s) = unlimited(w, h, scale, can_scale);
    let px = w as u64 * h as u64;
    if max_pixels == 0 || px <= max_pixels {
        return (w, h, s);
    }
    // Same shape, fewer pixels; the UI scale shrinks with them.
    let k = (px as f64 / max_pixels as f64).sqrt();
    let (w, h) = (((w as f64 / k) as u32).max(1), ((h as f64 / k) as u32).max(1));
    let s = s.map(|s| if s == 0 { 0 } else { ((s as f64 / k).round() as u32).max(SCALE_MIN) });
    (w, h, s)
}

fn unlimited(w: u32, h: u32, scale: u32, can_scale: bool) -> (u32, u32, Option<u32>) {
    match (scale, can_scale) {
        // HiDPI off: leave the guest's scale alone, or put back the
        // user's if we changed it earlier.
        (0, true) => (w, h, Some(0)),
        (0, false) => (w, h, None),
        (s, true) => (w, h, Some(s)),
        // Physical pixels at 1x would make everything tiny: the old way.
        (s, false) => ((w as u64 * 100 / s as u64).max(1) as u32, (h as u64 * 100 / s as u64).max(1) as u32, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct FakeClip(Arc<Mutex<Option<String>>>);
    impl ClipboardProvider for FakeClip {
        fn get_text(&mut self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn set_text(&mut self, t: &str) {
            *self.0.lock().unwrap() = Some(t.to_string());
        }
    }

    fn read_msgs(s: &mut UnixStream, d: &mut Decoder) -> Vec<Message> {
        s.set_read_timeout(Some(Duration::from_millis(400))).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = s.read(&mut buf) {
            if n == 0 {
                break;
            }
            d.push(&buf[..n]);
            while let Ok(Some(m)) = d.next() {
                out.push(m);
            }
        }
        out
    }

    #[test]
    fn guests_that_cant_scale_get_logical_pixels() {
        assert_eq!(guest_display(2560, 1600, 200, true, 0), (2560, 1600, Some(200)));
        assert_eq!(guest_display(2560, 1600, 200, false, 0), (1280, 800, None));
        assert_eq!(guest_display(2560, 1600, 0, true, 0), (2560, 1600, Some(0)));
        assert_eq!(guest_display(1280, 800, 0, false, 0), (1280, 800, None));
        // Windows' driver: at most 16 MiB of pixels.
        assert_eq!(guest_display(2560, 1600, 200, true, WINDOWS_MAX_PIXELS), (2560, 1600, Some(200)));
        let (w, h, s) = guest_display(3456, 2234, 200, true, WINDOWS_MAX_PIXELS);
        assert!(w as u64 * h as u64 <= WINDOWS_MAX_PIXELS && w > 2500 && matches!(s, Some(140..=150)), "{w}x{h} {s:?}");
        assert_eq!(guest_display(3840, 2160, 0, false, WINDOWS_MAX_PIXELS).2, None);
        assert_eq!(guest_display(7680, 4320, 0, true, WINDOWS_MAX_PIXELS).2, Some(0));
        assert_eq!(guest_display(7680, 4320, 200, true, WINDOWS_MAX_PIXELS).2, Some(100));
    }

    #[test]
    fn display_waits_for_an_agent_that_scales() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let a = spawn(host, ClipPolicy::default(), None, Screen::default()).unwrap();
        let mut d = Decoder::new(1 << 20);
        a.send(AgentCmd::Display { head: 0, width: 3456, height: 2234, scale: 200 });
        a.send(AgentCmd::Display { head: 0, width: 10, height: 10, scale: 7 }); // ignored
        let m = read_msgs(&mut guest, &mut d);
        assert!(m.contains(&Message::SetResolution { width: 1728, height: 1117 }), "{m:?}");
        assert!(!m.iter().any(|m| matches!(m, Message::SetScale { .. })), "{m:?}");
        let f = features::RESOLUTION | features::SCALE;
        guest.write_all(&Message::Hello { features: f, agent: "t".into() }.encode()).unwrap();
        let m = read_msgs(&mut guest, &mut d);
        assert!(m.contains(&Message::SetResolution { width: 3456, height: 2234 }), "{m:?}");
        assert!(m.contains(&Message::SetScale { percent: 200 }), "{m:?}");
        a.send(AgentCmd::Stop);
    }

    #[test]
    fn monitors_get_a_layout() {
        assert_eq!(
            layout(&[Some((1280, 800)), None, Some((800, 600))]),
            vec![Some(Monitor { x: 0, y: 0, width: 1280, height: 800 }), None, Some(Monitor { x: 1280, y: 0, width: 800, height: 600 })]
        );
        let (gpu, ctl) = vroom_virtio::devices::gpu::Gpu::with_heads(&[(1024, 768), (1024, 768)], vec![Arc::new(NoSink), Arc::new(NoSink)]);
        drop(gpu);
        let screen = Screen { heads: vec![(ctl.clone(), 0), (ctl.clone(), 1)], ..Default::default() };
        let (host, mut guest) = UnixStream::pair().unwrap();
        let a = spawn(host, ClipPolicy::default(), None, screen).unwrap();
        let mut d = Decoder::new(1 << 20);
        guest.write_all(&Message::Hello { features: features::RESOLUTION | features::MONITORS, agent: "t".into() }.encode()).unwrap();
        a.send(AgentCmd::Display { head: 0, width: 1280, height: 800, scale: 0 });
        let m = read_msgs(&mut guest, &mut d);
        let second = Some(Monitor { x: 1280, y: 0, width: 1024, height: 768 });
        assert!(m.contains(&Message::SetLayout { monitors: vec![Some(Monitor { x: 0, y: 0, width: 1280, height: 800 }), second] }), "{m:?}");
        // The second window closes: that monitor goes.
        a.send(AgentCmd::Display { head: 1, width: 0, height: 0, scale: 0 });
        let m = read_msgs(&mut guest, &mut d);
        assert!(m.iter().any(|m| matches!(m, Message::SetLayout { monitors } if monitors[1].is_none())), "{m:?}");
        assert_eq!(ctl.head_size(1), None);
        a.send(AgentCmd::Display { head: 1, width: 640, height: 480, scale: 0 });
        a.send(AgentCmd::Display { head: 5, width: 640, height: 480, scale: 0 }); // no such monitor
        read_msgs(&mut guest, &mut d);
        assert_eq!(ctl.head_size(1), Some((640, 480)));
        a.send(AgentCmd::Stop);
    }

    struct NoSink;
    impl vroom_virtio::devices::gpu::DisplaySink for NoSink {
        fn set_mode(&self, _: u32, _: u32) {}
        fn flush(&self, _: u32, _: u32, _: u32, _: u32, _: &[u8], _: usize) {}
    }

    #[test]
    fn drops_reach_the_guest_checked() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let a = spawn(host, ClipPolicy::default(), None, Screen::default()).unwrap();
        let mut d = Decoder::new(1 << 20);
        read_msgs(&mut guest, &mut d);
        let items = vec![("a.txt".to_string(), false)];
        // No agent that takes drops yet: nothing goes to the guest.
        a.send(AgentCmd::DragEnter { id: 1, items: items.clone() });
        assert!(read_msgs(&mut guest, &mut d).is_empty());
        guest.write_all(&Message::Hello { features: features::FILE_DROP, agent: "t".into() }.encode()).unwrap();
        read_msgs(&mut guest, &mut d);
        a.send(AgentCmd::DragEnter { id: 2, items: items.clone() });
        a.send(AgentCmd::DragEnter { id: 3, items: vec![("../x".into(), false)] });
        assert_eq!(read_msgs(&mut guest, &mut d), vec![Message::DragEnter { id: 2, items }]);

        let frames = [
            Message::DropEntry { id: 2, path: "a.txt".into(), dir: false, size: 3 },
            Message::DropData { id: 2, data: b"abc".to_vec() },
            Message::DropEnd { id: 2 },
        ];
        let (r, mut w) = UnixStream::pair().unwrap();
        for f in &frames {
            w.write_all(&f.encode()).unwrap();
        }
        drop(w);
        a.send(AgentCmd::Drop { id: 2, from: r.into() });
        assert_eq!(read_msgs(&mut guest, &mut d), frames.to_vec());

        // Another drop's frames, or anything else, cancel it.
        let (r, mut w) = UnixStream::pair().unwrap();
        w.write_all(&Message::DropEntry { id: 9, path: "a".into(), dir: false, size: 0 }.encode()).unwrap();
        a.send(AgentCmd::Drop { id: 4, from: r.into() });
        assert_eq!(read_msgs(&mut guest, &mut d), vec![Message::DragCancel { id: 4 }]);
        // So does a pipe that ends early.
        let (r, w) = UnixStream::pair().unwrap();
        drop(w);
        a.send(AgentCmd::Drop { id: 5, from: r.into() });
        assert_eq!(read_msgs(&mut guest, &mut d), vec![Message::DragCancel { id: 5 }]);
        a.send(AgentCmd::Stop);
    }

    #[test]
    fn focus_gates_clipboard() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let clip = FakeClip::default();
        let a = spawn(host, ClipPolicy::default(), Some(Box::new(clip.clone())), Screen::default()).unwrap();
        let mut d = Decoder::new(1 << 20);
        assert!(matches!(read_msgs(&mut guest, &mut d)[0], Message::Hello { .. }));

        // Not focused: host copies are NOT sent; guest copies NOT applied.
        *clip.0.lock().unwrap() = Some("host secret".into());
        guest.write_all(&Message::ClipboardText("from guest".into()).encode()).unwrap();
        assert!(read_msgs(&mut guest, &mut d).is_empty());
        assert_eq!(clip.0.lock().unwrap().as_deref(), Some("host secret"));

        // Explicit pull applies the guest text.
        a.send(AgentCmd::PullClipboard);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(clip.0.lock().unwrap().as_deref(), Some("from guest"));

        // Focused: host changes flow to the guest.
        a.send(AgentCmd::Focus(true));
        *clip.0.lock().unwrap() = Some("copied on host".into());
        let m = read_msgs(&mut guest, &mut d);
        assert!(m.contains(&Message::ClipboardText("copied on host".into())), "{m:?}");
        a.send(AgentCmd::Stop);
    }

    #[test]
    fn oversize_and_disabled_direction() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let clip = FakeClip::default();
        let pol = ClipPolicy { host_to_guest: true, guest_to_host: false, max_bytes: 8 };
        let a = spawn(host, pol, Some(Box::new(clip.clone())), Screen::default()).unwrap();
        a.send(AgentCmd::Focus(true));
        guest.write_all(&Message::ClipboardText("x".into()).encode()).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(*clip.0.lock().unwrap(), None);
        *clip.0.lock().unwrap() = Some("way more than eight bytes".into());
        let mut d = Decoder::new(1 << 20);
        let m = read_msgs(&mut guest, &mut d);
        assert!(!m.iter().any(|m| matches!(m, Message::ClipboardText(_))), "{m:?}");
        a.send(AgentCmd::Stop);
    }
}
