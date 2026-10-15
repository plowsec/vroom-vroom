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
use vroom_proto::{features, Decoder, Message, SCALE_MAX, SCALE_MIN};
use vroom_virtio::devices::gpu::GpuControl;

/// Host clipboard access, provided by the frontend.
pub trait ClipboardProvider: Send {
    fn get_text(&mut self) -> Option<String>;
    fn set_text(&mut self, text: &str);
}

/// Commands from the frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCmd {
    /// The VM window gained/lost focus (enables automatic sync).
    Focus(bool),
    /// Send the current host clipboard to the guest now.
    PushClipboard,
    /// Copy the guest's latest clipboard to the host now.
    PullClipboard,
    SyncTime,
    /// The window's size. `scale` 0: in guest pixels, the guest's UI
    /// scale is left alone. Else (HiDPI) in physical pixels, with the
    /// scale (percent) the guest's UI needs to look the same size; a
    /// guest that can't scale gets the size divided by it instead.
    Display { width: u32, height: u32, scale: u32 },
    Shutdown { reboot: bool },
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
}

struct Service {
    stream: UnixStream,
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
    /// The window's last size request, redone when an agent connects.
    display: Option<(u32, u32, u32)>,
}

/// The monitor whose size the window drives. Resizes go through the agent
/// service: whether the guest gets physical pixels depends on its agent.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    pub gpu: Option<GpuControl>,
    /// Largest mode (in pixels) the guest's driver takes; bigger windows
    /// get a smaller mode of the same shape. 0: no limit.
    pub max_pixels: u64,
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
    let svc = Service {
        stream,
        policy,
        clip,
        rx,
        status: status.clone(),
        dec: Decoder::new(max),
        focus: false,
        guest_clip: None,
        last_guest_update: None,
        host_seen: None,
        screen,
        display: None,
    };
    std::thread::Builder::new().name("agent".into()).spawn(move || svc.run())?;
    Ok(Agent { tx, status })
}

impl Service {
    fn send(&mut self, m: &Message) {
        let _ = self.stream.write_all(&m.encode());
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
                *s = AgentStatus { connected: true, agent, features, sessions };
                drop(s);
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
            // Host-bound only; a guest sending these is ignored.
            _ => {}
        }
    }

    fn can_scale(&self) -> bool {
        let s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.connected && s.features & features::SCALE != 0
    }

    fn apply_display(&mut self) {
        let Some((w, h, scale)) = self.display else { return };
        let (w, h, set_scale) = guest_display(w, h, scale, self.can_scale(), self.screen.max_pixels);
        if let Some(g) = &self.screen.gpu {
            g.resize(w, h);
        }
        self.send(&Message::SetResolution { width: w, height: h });
        if let Some(p) = set_scale {
            self.send(&Message::SetScale { percent: p });
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
            AgentCmd::Display { width, height, scale } => {
                if width == 0 || height == 0 || (scale != 0 && !(SCALE_MIN..=SCALE_MAX).contains(&scale)) {
                    return;
                }
                self.display = Some((width, height, scale));
                self.apply_display();
            }
            AgentCmd::Shutdown { reboot } => self.send(&Message::Shutdown { reboot }),
            AgentCmd::Stop => {}
        }
    }
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
        a.send(AgentCmd::Display { width: 3456, height: 2234, scale: 200 });
        a.send(AgentCmd::Display { width: 10, height: 10, scale: 7 }); // ignored
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
