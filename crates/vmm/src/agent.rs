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
use vroom_proto::{features, Decoder, Message, Monitor, WindowOp, SCALE_MAX, SCALE_MIN};
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
    /// Coherence on or off: the guest's windows as windows of the host.
    Coherence(bool),
    /// Coherence: do this to a window of the guest's last list.
    Window { id: u32, op: WindowOp },
    /// The mouse left the window with the guest's button held: is the
    /// guest dragging files out? The answer goes to the viewer.
    DragOut { id: u32 },
    /// That drag is over (dropped and copied, or given up).
    DragOutEnd { id: u32 },
    /// The Mac drop wants item `item`: write it into this pipe, checked.
    DragOutGet { id: u32, item: u32, to: std::os::fd::OwnedFd },
    /// Start a guest app by name (checked by the viewer protocol).
    StartApp(String),
    /// The pointer on the whole desktop (see `Message::Mouse`).
    Mouse { x: u16, y: u16, buttons: u8, wheel: i8 },
    /// A USB disk's files came to the removable share `tag`, or left.
    UsbShare { tag: String, label: String, on: bool },
    /// New guest tools: the viewer writes them into this pipe as agent
    /// frames (`UpdateBegin`, `UpdateData`); checked, then to the guest.
    Update { from: std::os::fd::OwnedFd },
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
/// How long Windows gets to show something once its 3D display is back
/// on, and how often we turn it off and on again when it doesn't.
const REVIVE_WAIT: Duration = if cfg!(test) { Duration::from_millis(400) } else { Duration::from_secs(15) };
const REVIVE_TRIES: u8 = 3;
/// How long turning it off may take (it often takes a few seconds).
const REVIVE_OFF_WAIT: Duration = Duration::from_secs(30);
/// How long it stays off before it goes on again: Windows settles on its
/// basic display first (on again at once, it often showed nothing still).
const REVIVE_OFF_PAUSE: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(3) };

/// Watching the 3D display come back (see `Service::revive`).
#[derive(Debug, Clone, Copy)]
struct Revive {
    since: Instant,
    tries: u8,
    /// We turned it off to turn it on again.
    cycling: bool,
    /// When the guest said it is off (while cycling).
    off_at: Option<Instant>,
}
/// A viewer that doesn't take a dragged file's data for this long has
/// given up on it.
const PIPE_TIMEOUT: Duration = Duration::from_secs(20);
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
    /// Coherence is on (the window asked), and the ids of the guest's
    /// last window list: the only windows the window may act on.
    coherence: bool,
    win_ids: Vec<u32>,
    /// Files dragged out of the guest, if any.
    dragout: Option<OutDrag>,
    /// Windows said its 3D display is on again (after a suspend): it
    /// should show something soon.
    revive: Option<Revive>,
    streams: u32,
    /// USB disks' files on removable shares now: (tag, volume name).
    usb_shares: Vec<(String, String)>,
    /// A guest tools update is on its way (set by its thread until done).
    updating: Arc<std::sync::atomic::AtomicBool>,
}

/// A drag of files out of the guest (see `AgentCmd::DragOut`).
struct OutDrag {
    id: u32,
    /// What the guest says it drags; None until it answers.
    items: Option<Vec<(String, bool)>>,
    /// The item on its way to the viewer.
    fetch: Option<Fetch>,
}

struct Fetch {
    /// The guest's frames for it carry this number.
    stream: u32,
    check: vroom_proto::TreeCheck,
    pipe: UnixStream,
    bytes: u64,
    t0: Instant,
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
    // Room for dragged files' data too; clipboard text is held to the
    // policy's size where it is taken.
    let max = (policy.max_bytes as usize).max(vroom_proto::DROP_CHUNK + 64) + 64;
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
        coherence: false,
        win_ids: Vec::new(),
        dragout: None,
        streams: 0,
        revive: None,
        usb_shares: Vec::new(),
        updating: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            self.revive_tick();
            self.send_mode();
            if self.focus {
                self.auto_push();
            }
        }
    }

    /// Windows sometimes turns its 3D display back on after a resume and
    /// never shows anything on it (the guest runs, its driver says OK):
    /// turn it off and on again, as one would in Device Manager.
    /// (Plugging the monitor in again never brought one back in tests, so
    /// it isn't tried; the device off for a few seconds and on did.)
    fn revive_tick(&mut self) {
        let Some(r) = self.revive else { return };
        if r.cycling {
            match r.off_at {
                Some(t) if t.elapsed() > REVIVE_OFF_PAUSE => {
                    self.revive = Some(Revive { since: Instant::now(), cycling: false, off_at: None, ..r });
                    self.send(&Message::Gpu3d { on: true });
                }
                None if r.since.elapsed() > REVIVE_OFF_WAIT => {
                    log::warn!("gpu: Windows didn't turn its 3D display off and on again");
                    self.revive = None;
                }
                _ => {}
            }
            return;
        }
        if self.screen.heads.first().is_none_or(|h| h.0.showing()) {
            if r.tries > 0 {
                log::info!("gpu: Windows shows its 3D display again ({} times off and on)", r.tries);
            }
            self.revive = None;
        } else if r.since.elapsed() > REVIVE_WAIT {
            if r.tries >= REVIVE_TRIES {
                log::warn!("gpu: Windows shows nothing on its 3D display; turn the display device off and on in Device Manager");
                self.revive = None;
                return;
            }
            log::info!("gpu: Windows shows nothing on its 3D display: turning it off and on again");
            self.revive = Some(Revive { since: Instant::now(), tries: r.tries + 1, cycling: true, off_at: None });
            self.send(&Message::Gpu3d { on: false });
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
                self.to_viewer(ToViewer::Agent { features });
                // An agent that came back (guest reboot, new logon) while
                // the window is in Coherence.
                self.win_ids.clear();
                // USB disks' files plugged in before it came.
                if features & features::USB_SHARES != 0 {
                    for (tag, label) in self.usb_shares.clone() {
                        self.send(&Message::UsbShare { tag, label, on: true });
                    }
                }
                if self.coherence && features & features::COHERENCE != 0 {
                    self.send(&Message::Coherence { on: true });
                }
            }
            Message::Windows(list) if self.coherence => {
                self.win_ids = list.iter().map(|w| w.id).collect();
                self.to_viewer(ToViewer::Windows(list));
            }
            // Only for windows of the last list (and checked for size by
            // the decoder).
            Message::WindowIcon { id, w, h, rgba } if self.coherence && self.win_ids.contains(&id) => {
                self.to_viewer(ToViewer::WindowIcon { id, w, h, rgba });
            }
            // Checked by the decoder (count, names, icon sizes); the bar
            // only suggests them.
            Message::Apps(list) if self.coherence => self.to_viewer(ToViewer::Apps(list)),
            Message::CoherenceNo(why) if self.coherence => {
                log::info!("guest: no coherence here: {why}");
                self.to_viewer(ToViewer::CoherenceNo(why));
            }
            Message::ClipboardText(t) => self.guest_clipboard(t),
            Message::Ping(v) => self.send(&Message::Pong(v)),
            // Its screen is about to go off: the window says so.
            Message::Stopping => {
                log::info!("the guest is shutting down");
                self.to_viewer(ToViewer::Status("guest shutting down".into()));
            }
            Message::Gpu3d { on } => {
                log::info!("guest: 3D display device {}", if on { "on" } else { "off" });
                self.status.lock().unwrap_or_else(|e| e.into_inner()).gpu3d = Some(on);
                match (on, self.revive) {
                    (true, r) if self.screen.gpu3d => {
                        self.revive = Some(Revive { since: Instant::now(), tries: r.map_or(0, |r| r.tries), cycling: false, off_at: None });
                    }
                    (false, Some(r)) if r.cycling && r.off_at.is_none() => {
                        self.revive = Some(Revive { off_at: Some(Instant::now()), ..r });
                    }
                    _ => {}
                }
            }
            Message::DragReady { id, live } => self.to_viewer(ToViewer::DragReady { id, live }),
            Message::DropDone { id, ok, place } => {
                let place: String = place.chars().filter(|c| !c.is_control()).take(200).collect();
                log::info!("guest: drop {id} {}{place}", if ok { "done " } else { "failed: " });
                self.to_viewer(ToViewer::DropDone { id, ok, place });
            }
            Message::DragOutItems { id, items } => {
                // Only an answer to our question, and only once.
                if let Some(o) = self.dragout.as_mut().filter(|o| o.id == id && o.items.is_none()) {
                    log::info!("guest: drag {id} out: {} item(s)", items.len());
                    o.items = Some(items.clone());
                    self.to_viewer(ToViewer::DragOutItems { id, items });
                }
            }
            m @ (Message::DropEntry { .. } | Message::DropData { .. } | Message::DropEnd { .. } | Message::DragOutFail { .. }) => self.drag_out_frame(m),
            Message::UpdateDone { id, ok, why } => {
                // The decoder kept it to one short line.
                if ok {
                    log::info!("guest: tools update {id} checked, installing");
                    self.to_viewer(ToViewer::Status("guest tools: installing".into()));
                } else {
                    log::warn!("guest: tools update {id} refused: {why}");
                    self.to_viewer(ToViewer::Status(format!("guest tools update failed: {why}")));
                }
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
                self.revive = None;
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
            AgentCmd::Coherence(on) => {
                let had = std::mem::replace(&mut self.coherence, on);
                self.win_ids.clear();
                let s = self.status();
                if (on || had) && s.connected && s.features & features::COHERENCE != 0 {
                    self.send(&Message::Coherence { on });
                }
            }
            AgentCmd::Window { id, op } => {
                // Only windows the guest listed last, only in Coherence.
                if self.coherence && self.win_ids.contains(&id) {
                    self.send(&Message::WindowDo { id, op });
                }
            }
            AgentCmd::StartApp(name) => {
                if self.status().features & features::START_APP != 0 && vroom_proto::line_ok(&name) {
                    self.send(&Message::StartApp(name));
                }
            }
            AgentCmd::Mouse { x, y, buttons, wheel } => self.send(&Message::Mouse { x, y, buttons, wheel }),
            AgentCmd::UsbShare { tag, label, on } => {
                self.usb_shares.retain(|(t, _)| *t != tag);
                if on {
                    self.usb_shares.push((tag.clone(), label.clone()));
                }
                if self.status().features & features::USB_SHARES != 0 {
                    self.send(&Message::UsbShare { tag, label, on });
                }
            }
            AgentCmd::Drop { id, from } => {
                let out = self.out.clone();
                let r = std::thread::Builder::new().name("agent-drop".into()).spawn(move || forward_drop(id, from, &out));
                if r.is_err() {
                    self.send(&Message::DragCancel { id });
                }
            }
            AgentCmd::DragOut { id } => {
                let s = self.status();
                if !s.connected || s.features & features::DRAG_OUT == 0 {
                    self.to_viewer(ToViewer::DragOutItems { id, items: Vec::new() });
                    return;
                }
                if let Some(o) = self.dragout.take() {
                    self.send(&Message::DragOutEnd { id: o.id });
                }
                self.dragout = Some(OutDrag { id, items: None, fetch: None });
                self.send(&Message::DragOutQuery { id });
            }
            AgentCmd::DragOutEnd { id } => {
                if self.dragout.as_ref().is_some_and(|o| o.id == id) {
                    self.dragout = None;
                    self.send(&Message::DragOutEnd { id });
                }
            }
            AgentCmd::DragOutGet { id, item, to } => {
                // Anything not asked for: the pipe closes, the viewer stops.
                let pipe = UnixStream::from(to);
                let Some(o) = self.dragout.as_mut().filter(|o| o.id == id) else { return };
                let Some((name, dir)) = o.items.as_ref().and_then(|v| v.get(item as usize)).cloned() else { return };
                if pipe.set_write_timeout(Some(PIPE_TIMEOUT)).is_err() {
                    return;
                }
                // A new number: late frames of an item given up on can't
                // pass for this one's.
                self.streams = self.streams.wrapping_add(1);
                let stream = self.streams;
                o.fetch = Some(Fetch { stream, check: vroom_proto::TreeCheck::new(stream, &name, dir), pipe, bytes: 0, t0: Instant::now() });
                self.send(&Message::DragOutGet { id, item, stream });
            }
            AgentCmd::Update { from } => {
                let s = self.status();
                let why = if !s.connected {
                    Some("no guest agent is running")
                } else if s.features & features::UPDATE == 0 {
                    Some("this guest agent can't update itself: install the guest tools by hand")
                } else if self.updating.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    Some("an update is on its way already")
                } else {
                    None
                };
                if let Some(w) = why {
                    self.to_viewer(ToViewer::Status(format!("guest tools update failed: {w}")));
                    return;
                }
                let (out, busy, viewer) = (self.out.clone(), self.updating.clone(), self.screen.viewer.clone());
                let r = std::thread::Builder::new().name("agent-update".into()).spawn(move || {
                    if let Err(e) = forward_update(from, &out) {
                        log::warn!("tools update: {e}");
                        if let Some(v) = &viewer {
                            v.send(&ToViewer::Status(format!("guest tools update failed: {e}")));
                        }
                    }
                    busy.store(false, std::sync::atomic::Ordering::Release);
                });
                if r.is_err() {
                    self.updating.store(false, std::sync::atomic::Ordering::Release);
                }
            }
            AgentCmd::Stop => {}
        }
    }

    /// A frame of the item being fetched: checked, then to the viewer
    /// with the drag's id. A guest that breaks the rules loses the drag.
    fn drag_out_frame(&mut self, m: Message) {
        let Some(o) = self.dragout.as_mut() else { return };
        let id = o.id;
        let Some(f) = o.fetch.as_mut() else { return };
        let stream = match &m {
            Message::DropEntry { id, .. } | Message::DropData { id, .. } | Message::DropEnd { id } => *id,
            Message::DragOutFail { stream, .. } => *stream,
            _ => return,
        };
        if stream != f.stream {
            // One given up on.
            return;
        }
        let (fwd, done, bad) = match f.check.check(&m) {
            Ok(end) => {
                let fwd = match m {
                    Message::DropEntry { path, dir, size, .. } => Message::DropEntry { id, path, dir, size },
                    Message::DropData { data, .. } => {
                        f.bytes += data.len() as u64;
                        Message::DropData { id, data }
                    }
                    _ => Message::DropEnd { id },
                };
                (fwd, end, false)
            }
            Err(_) if matches!(m, Message::DragOutFail { .. }) => {
                let Message::DragOutFail { why, .. } = m else { return };
                let why: String = why.chars().filter(|c| !c.is_control()).take(200).collect();
                log::info!("guest: drag {id} out: {why}");
                (Message::DragOutFail { stream: id, why }, true, false)
            }
            Err(e) => {
                log::warn!("guest: drag {id} out: {e}");
                (Message::DragOutFail { stream: id, why: format!("the guest sent a bad file list ({e})") }, true, true)
            }
        };
        let gone = f.pipe.write_all(&fwd.encode()).is_err();
        if done && !bad && !gone {
            log::info!("drag {id} out: {} bytes to the viewer in {:?}", f.bytes, f.t0.elapsed());
        }
        if done || gone {
            o.fetch = None;
        }
        if bad || gone {
            // Stop the guest sending.
            self.dragout = None;
            self.send(&Message::DragOutEnd { id });
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

/// Pass a guest tools update from the viewer's pipe to the guest,
/// checked: one `UpdateBegin` (the decoder checks its file list), then
/// exactly the bytes it announced, in `UpdateData` frames of the same
/// update, nothing else. A pipe that ends early leaves the guest with an
/// update it never installs (the next one starts over).
fn forward_update(from: std::os::fd::OwnedFd, out: &Mutex<UnixStream>) -> Result<(), String> {
    let mut from = std::fs::File::from(from);
    let mut dec = Decoder::new(vroom_proto::UPDATE_CHUNK + (vroom_proto::UPDATE_FILES_MAX << 9));
    let mut buf = vec![0u8; vroom_proto::UPDATE_CHUNK];
    let send = |m: &Message| out.lock().unwrap_or_else(|e| e.into_inner()).write_all(&m.encode()).map_err(|e| format!("to the guest: {e}"));
    // (id, bytes still to come)
    let mut cur: Option<(u32, u64)> = None;
    let t0 = Instant::now();
    loop {
        let n = from.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return match cur {
                Some((_, 0)) => Ok(()),
                _ => Err("the app stopped sending it".into()),
            };
        }
        dec.push(&buf[..n]);
        while let Some(m) = dec.next().map_err(|e| format!("bad frame from the app: {e}"))? {
            match (&m, cur) {
                (Message::UpdateBegin { id, version, files }, None) => {
                    let total = files.iter().map(|f| f.size).sum::<u64>();
                    log::info!("tools update {id}: {} files, {total} bytes, version {version}", files.len());
                    cur = Some((*id, total));
                }
                (Message::UpdateData { id, data }, Some((cid, left))) if *id == cid && data.len() as u64 <= left => {
                    cur = Some((cid, left - data.len() as u64));
                }
                _ => return Err("unexpected frame from the app".into()),
            }
            send(&m)?;
            if let Some((id, 0)) = cur {
                log::info!("tools update {id}: sent in {:?}", t0.elapsed());
            }
        }
    }
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

    #[test]
    fn a_3d_display_that_shows_nothing_is_turned_off_and_on() {
        let (gpu, ctl) = vroom_virtio::devices::gpu::Gpu::with_heads(&[(1024, 768)], vec![Arc::new(NoSink)]);
        drop(gpu);
        let screen = Screen { heads: vec![(ctl, 0)], gpu3d: true, ..Default::default() };
        let (host, mut guest) = UnixStream::pair().unwrap();
        let a = spawn(host, ClipPolicy::default(), None, screen).unwrap();
        let mut d = Decoder::new(1 << 20);
        guest.write_all(&Message::Hello { features: features::GPU3D, agent: "t".into() }.encode()).unwrap();
        assert!(read_msgs(&mut guest, &mut d).contains(&Message::Gpu3d { on: true }));
        // On, and nothing shows on it: off, then on again once it's off.
        guest.write_all(&Message::Gpu3d { on: true }.encode()).unwrap();
        std::thread::sleep(REVIVE_WAIT + POLL * 3);
        assert!(read_msgs(&mut guest, &mut d).contains(&Message::Gpu3d { on: false }));
        // Off: on again, after a pause.
        guest.write_all(&Message::Gpu3d { on: false }.encode()).unwrap();
        std::thread::sleep(REVIVE_OFF_PAUSE + POLL * 3);
        assert!(read_msgs(&mut guest, &mut d).contains(&Message::Gpu3d { on: true }));
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
    fn drags_out_of_the_guest_are_checked() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let a = spawn(host, ClipPolicy { max_bytes: 8, ..ClipPolicy::default() }, None, Screen::default()).unwrap();
        let mut d = Decoder::new(1 << 20);
        read_msgs(&mut guest, &mut d);
        // An agent that can't: the guest isn't asked.
        a.send(AgentCmd::DragOut { id: 1 });
        assert!(read_msgs(&mut guest, &mut d).is_empty());
        guest.write_all(&Message::Hello { features: features::DRAG_OUT, agent: "t".into() }.encode()).unwrap();
        read_msgs(&mut guest, &mut d);
        a.send(AgentCmd::DragOut { id: 7 });
        assert_eq!(read_msgs(&mut guest, &mut d), vec![Message::DragOutQuery { id: 7 }]);
        let items = vec![("a.txt".to_string(), false), ("D".to_string(), true)];
        guest.write_all(&Message::DragOutItems { id: 7, items: items.clone() }.encode()).unwrap();
        // Asked once: a second answer changes nothing.
        guest.write_all(&Message::DragOutItems { id: 7, items: vec![("x".into(), false)] }.encode()).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let get = |item: u32, d: &mut Decoder, guest: &mut UnixStream| {
            let (r, w) = UnixStream::pair().unwrap();
            a.send(AgentCmd::DragOutGet { id: 7, item, to: w.into() });
            match read_msgs(guest, d).as_slice() {
                [Message::DragOutGet { id: 7, item: i, stream }] if *i == item => (*stream, r),
                m => panic!("{m:?}"),
            }
        };
        let (s, mut r) = get(1, &mut d, &mut guest);
        let data = vec![5u8; 300];
        for m in [
            // Late frames of another stream are dropped.
            Message::DropEntry { id: s.wrapping_add(9), path: "zzz".into(), dir: false, size: 1 },
            Message::DropEntry { id: s, path: "D".into(), dir: true, size: 300 },
            Message::DropEntry { id: s, path: "D/x".into(), dir: false, size: 300 },
            // Bigger than the clipboard's limit: still passes.
            Message::DropData { id: s, data: data.clone() },
            Message::DropEnd { id: s },
        ] {
            guest.write_all(&m.encode()).unwrap();
        }
        let mut pd = Decoder::new(1 << 20);
        assert_eq!(
            read_msgs(&mut r, &mut pd),
            vec![
                Message::DropEntry { id: 7, path: "D".into(), dir: true, size: 300 },
                Message::DropEntry { id: 7, path: "D/x".into(), dir: false, size: 300 },
                Message::DropData { id: 7, data },
                Message::DropEnd { id: 7 },
            ]
        );
        // The guest's reason goes through.
        let (s, mut r) = get(0, &mut d, &mut guest);
        guest.write_all(&Message::DragOutFail { stream: s, why: "locked\n".into() }.encode()).unwrap();
        assert_eq!(read_msgs(&mut r, &mut pd), vec![Message::DragOutFail { stream: 7, why: "locked".into() }]);
        // Something that isn't the item: the drag is over for the guest.
        let (s, mut r) = get(0, &mut d, &mut guest);
        guest.write_all(&Message::DropEntry { id: s, path: "other".into(), dir: false, size: 0 }.encode()).unwrap();
        assert!(matches!(read_msgs(&mut r, &mut pd).as_slice(), [Message::DragOutFail { stream: 7, .. }]));
        assert_eq!(read_msgs(&mut guest, &mut d), vec![Message::DragOutEnd { id: 7 }]);
        // And nothing more is fetched.
        let (r, w) = UnixStream::pair().unwrap();
        a.send(AgentCmd::DragOutGet { id: 7, item: 0, to: w.into() });
        assert!(read_msgs(&mut guest, &mut d).is_empty());
        // Its pipe was closed.
        assert!(matches!((&r).read(&mut [0u8; 8]), Ok(0) | Err(_)));
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

    /// The viewer's update frames through `forward_update`: what reaches
    /// the guest, and whether it went through.
    fn forward(frames: &[Message]) -> (Result<(), String>, Vec<Message>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (host, mut guest) = UnixStream::pair().unwrap();
        let bytes: Vec<u8> = frames.iter().flat_map(|m| m.encode()).collect();
        let w = std::thread::spawn(move || {
            let mut ours = ours;
            let _ = ours.write_all(&bytes);
        });
        let out = Mutex::new(host);
        let r = forward_update(theirs.into(), &out);
        w.join().unwrap();
        (r, read_msgs(&mut guest, &mut Decoder::new(1 << 20)))
    }

    #[test]
    fn updates_pass_only_as_announced() {
        let file = |size| vroom_proto::UpdateFile { path: "vroom-agent".into(), size, sha256: [0; 32] };
        let begin = |id, size| Message::UpdateBegin { id, version: "0.1.0".into(), files: vec![file(size)] };
        let data = |id, n| Message::UpdateData { id, data: vec![1; n] };
        let good = [begin(1, 5), data(1, 3), data(1, 2)];
        let (r, got) = forward(&good);
        assert_eq!(r, Ok(()));
        assert_eq!(got, good);
        // More than announced, another update's data, anything else, or
        // a pipe that ends early: stopped there.
        for bad in [vec![begin(1, 2), data(1, 3)], vec![begin(1, 2), data(2, 1)], vec![begin(1, 2), Message::Ping(1)], vec![data(1, 1)], vec![begin(1, 4), data(1, 2)]] {
            let (r, got) = forward(&bad);
            assert!(r.is_err(), "{bad:?}");
            assert!(got.len() < bad.len() || matches!(bad.last(), Some(Message::UpdateData { .. })), "{bad:?} {got:?}");
        }
    }
}
