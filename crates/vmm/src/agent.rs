//! Host side of the guest agent channel: clipboard, time sync, shutdown.
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

use bulwark_config::Clipboard as ClipPolicy;
use bulwark_proto::{features, Decoder, Message};

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
    SetResolution(u32, u32),
    Shutdown { reboot: bool },
    Stop,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentStatus {
    pub connected: bool,
    pub agent: String,
    pub features: u64,
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
}

/// Start the service on the host end of the agent port.
pub fn spawn(stream: UnixStream, policy: ClipPolicy, clip: Option<Box<dyn ClipboardProvider>>) -> std::io::Result<Agent> {
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
        self.send(&Message::Hello { features: f, agent: format!("bulwark-host {}", env!("CARGO_PKG_VERSION")) });
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
                log::info!("guest agent connected: {agent}");
                let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
                *s = AgentStatus { connected: true, agent, features };
            }
            Message::ClipboardText(t) => self.from_guest(t),
            Message::Ping(v) => self.send(&Message::Pong(v)),
            // Host-bound only; a guest sending these is ignored.
            _ => {}
        }
    }

    fn from_guest(&mut self, t: String) {
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
            AgentCmd::SyncTime => {
                let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                self.send(&Message::TimeSync { unix_secs: d.as_secs(), nanos: d.subsec_nanos() });
            }
            AgentCmd::SetResolution(w, h) => self.send(&Message::SetResolution { width: w, height: h }),
            AgentCmd::Shutdown { reboot } => self.send(&Message::Shutdown { reboot }),
            AgentCmd::Stop => {}
        }
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
    fn focus_gates_clipboard() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let clip = FakeClip::default();
        let a = spawn(host, ClipPolicy::default(), Some(Box::new(clip.clone()))).unwrap();
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
        let a = spawn(host, pol, Some(Box::new(clip.clone()))).unwrap();
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
