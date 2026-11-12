//! `vroom run`: one VM, serial console and/or GUI viewer.

use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use vroom_config::{NetKind, VmConfig};
use vroom_display::Command;
use vroom_vmm::agent::{AgentCmd, ClipboardProvider};
use vroom_vmm::{RunState, StartMode, VmEvent, Vmm, VmmOptions};

use crate::term;

const HELP: &str = "\r\n[vroom] Ctrl-A x: power off | s: snapshot | z: suspend | p: pause/resume | r: reset\r\n          d: ask guest to shut down | c: host clipboard -> guest | v: guest clipboard -> host\r\n          g: screendump (png)\r\n";

pub struct RunOpts {
    pub config: VmConfig,
    pub restore: Option<PathBuf>,
    /// `<vm>.state` directory: snapshots/, suspend/.
    pub state_dir: PathBuf,
    pub clipboard: Option<Box<dyn ClipboardProvider>>,
    pub sandbox: bool,
    pub ui_fd: Option<i32>,
    pub fresh: bool,
}

/// What the user asked for (keyboard escape or viewer command).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    PowerOff,
    Snapshot,
    Suspend,
    TogglePause,
    Pause,
    Resume,
    Reset,
    GuestShutdown,
    PushClipboard,
    PullClipboard,
    Screendump,
}

enum Flow {
    Continue,
    Exit,
}

fn timestamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Sandbox the process now that everything is open.
fn lock_down(cfg: &VmConfig, later_shares: &[vroom_config::Share], state_dir: &Path, clipboard: bool, helpers: Vec<u32>) -> Result<(), String> {
    let mut p = vroom_sandbox::Policy {
        write_dirs: vec![state_dir.to_path_buf()],
        clipboard,
        // USB disks the app hands over while the VM runs (nothing else
        // can be reached through these rules).
        usb_storage: cfg!(target_os = "macos"),
        // With 3D the GPU is the GPU process's, unless it runs in here.
        gpu: cfg.display.gpu == vroom_config::Gpu::Virtio3d && vroom_vmm::gpu_in_process(),
        shared_ram: cfg.display.gpu == vroom_config::Gpu::Virtio3d,
        ..Default::default()
    };
    for d in &cfg.disks {
        if d.readonly {
            p.ro_files.push(d.path.clone());
        } else {
            p.rw_files.push(d.path.clone());
        }
    }
    for s in cfg.shares.iter().chain(later_shares) {
        // Rules name real paths: the share root may be a symlink.
        let real = std::fs::canonicalize(&s.path).unwrap_or_else(|_| s.path.clone());
        if s.readonly {
            p.share_ro.push(real);
        } else {
            p.share_rw.push(real);
        }
    }
    for n in &cfg.nets {
        if n.mode == NetKind::Nat {
            p.network_outbound = true;
            p.loopback |= n.allow_host_loopback;
            p.loopback_ports.extend(&n.host_ports);
            p.loopback_dns |= vroom_net::nat::host_dns().is_some_and(|a| a.ip().is_loopback());
            p.listen_ports.extend(n.port_forwards.iter().map(|f| f.host_port));
        }
    }
    p.helper_pids = helpers;
    // The balloon reads the host's free memory from it (Landlock said no:
    // the "host runs short" inflate never happened on Linux).
    if cfg!(target_os = "linux") && cfg.balloon {
        p.ro_files.push("/proc/meminfo".into());
    }
    vroom_sandbox::apply(&p).map_err(|e| e.to_string())?;
    log::info!("sandbox applied");
    if std::env::var_os("VROOM_SANDBOX_SELFTEST").is_some() {
        probes("vmm", clipboard, p.network_outbound);
    }
    Ok(())
}

/// Probe what the sandbox should block; `who` names the process.
pub(crate) fn selftest(who: &str, clipboard: bool) {
    probes(who, clipboard, false);
}

/// Probes after the file ones, in order. On Linux seccomp answers each
/// with a kill (pipe2 for Command, bind, socket(AF_UNIX), and socket()
/// at all without NAT): there one per run, `VROOM_SELFTEST_FATAL`, last.
const LATE: [&str; 4] = ["net", "exec", "listen", "unix"];

/// Which `LATE` probes to run, in order, and which are left out. `net`:
/// the policy allows IP sockets.
fn plan(linux: bool, net: bool, chosen: Option<&str>) -> (Vec<&'static str>, Vec<&'static str>) {
    let fatal = |n: &str| linux && (n != "net" || !net);
    let (mut run, mut skip): (Vec<&'static str>, Vec<&'static str>) = LATE.iter().copied().partition(|n| !fatal(n));
    if let Some(i) = skip.iter().position(|n| Some(*n) == chosen) {
        run.push(skip.remove(i));
    }
    (run, skip)
}

/// `net`: see `plan`.
fn probes(who: &str, clipboard: bool, net: bool) {
    let say = |m: String| eprintln!("{who} sandbox selftest: {m}");
    let linux = cfg!(target_os = "linux");
    let chosen = std::env::var("VROOM_SELFTEST_FATAL").ok();
    let (late, skipped) = plan(linux, net, chosen.as_deref());

    let read = |path: &str| match std::fs::read(path) {
        Ok(_) => say(format!("{path} READABLE (bad)")),
        Err(e) => say(format!("{path} blocked ({e})")),
    };
    read("/etc/passwd");
    read(&format!("{}/.ssh/known_hosts", std::env::var("HOME").unwrap_or_default()));
    // Another process's shared memory (notifyd's, there on every Mac).
    #[cfg(target_os = "macos")]
    {
        let name = c"apple.shm.notification_center";
        // SAFETY: valid C string; the fd, if any, is closed right away.
        #[allow(unsafe_code)]
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY) };
        if fd >= 0 {
            say("foreign shared memory OPENED (bad)".into());
            // SAFETY: an fd we just got.
            #[allow(unsafe_code)]
            unsafe {
                libc::close(fd)
            };
        } else {
            say(format!("foreign shared memory blocked ({})", std::io::Error::last_os_error()));
        }
    }
    if clipboard {
        // Read only: the user's clipboard isn't ours to change, nor to print.
        match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => say("clipboard readable".into()),
            Err(e) => say(format!("clipboard FAILED ({e})")),
        }
    }
    if !skipped.is_empty() {
        say(format!("not run, seccomp kills (one per run: VROOM_SELFTEST_FATAL=<name>): {}", skipped.join(" ")));
    }
    let t = Duration::from_secs(2);
    for probe in late {
        if linux && chosen.as_deref() == Some(probe) {
            say(format!("{probe}: expect SIGSYS now (dying = pass)"));
        }
        match probe {
            "net" => {
                // Host services on loopback TCP, then the internet.
                if let Some(port) = std::env::var("VROOM_SELFTEST_LOOPBACK_PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
                    match std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), t) {
                        Ok(_) => say("host loopback ALLOWED (bad unless allow_host_loopback)".into()),
                        Err(e) => say(format!("host loopback blocked ({e})")),
                    }
                }
                match std::net::TcpStream::connect_timeout(&([1, 1, 1, 1], 53).into(), t) {
                    Ok(_) => say("internet reachable (NAT works)".into()),
                    Err(e) => say(format!("internet: {e}")),
                }
            }
            "exec" => match std::process::Command::new("/bin/echo").arg("x").output() {
                Ok(_) => say("exec ALLOWED (bad)".into()),
                Err(e) => say(format!("exec blocked ({e})")),
            },
            "listen" => match std::net::TcpListener::bind("127.0.0.1:0") {
                Ok(_) => say("new listener ALLOWED (bad)".into()),
                Err(e) => say(format!("new listener blocked ({e})")),
            },
            _ => {
                // ssh-agent; on Linux any path does (the kill is at socket()).
                let sock = std::env::var_os("SSH_AUTH_SOCK").or_else(|| linux.then(|| "/run/dbus/system_bus_socket".into()));
                if let Some(sock) = sock {
                    match std::os::unix::net::UnixStream::connect(&sock) {
                        Ok(_) => say(format!("unix socket {} ALLOWED (bad)", sock.to_string_lossy())),
                        Err(e) => say(format!("unix sockets blocked ({e})")),
                    }
                }
            }
        }
    }
}

struct Session {
    vmm: Vmm,
    state_dir: PathBuf,
    /// Agent connections already announced.
    agent_sessions: u32,
    next_space_check: std::time::Instant,
    low_space: u64,
    /// The window went away unasked (the app crashed): when. The VM keeps
    /// going for the app to come back; after `VIEWER_GRACE` it's suspended.
    viewer_lost: Option<std::time::Instant>,
}

/// How long a VM whose app crashed keeps running for the app to return.
const VIEWER_GRACE: Duration = Duration::from_secs(300);

/// Below this, guest writes could fail half-way: pause first.
/// `VROOM_LOW_SPACE_MIB` overrides it.
const LOW_SPACE: u64 = 2 << 30;
const SPACE_CHECK: Duration = Duration::from_secs(2);

impl Session {
    fn note(&self, msg: &str) {
        eprint!("\r\n[vroom] {msg}\r\n");
        self.vmm.viewer_status(msg);
    }

    /// Host disk nearly full: pause before guest writes start failing
    /// (a guest that sees I/O errors may corrupt its file systems).
    fn check_space(&mut self) {
        let now = std::time::Instant::now();
        if now < self.next_space_check || self.vmm.state() != RunState::Running {
            return;
        }
        self.next_space_check = now + SPACE_CHECK;
        let Ok(free) = self.vmm.host_free_bytes() else { return };
        if free < self.low_space && self.vmm.pause().is_ok() {
            self.note(&format!(
                "host disk almost full ({} MiB free): VM paused. Free some space, then resume.",
                free >> 20
            ));
        }
    }

    /// Say so when the guest agent (clipboard, resize, shutdown) comes up.
    fn check_agent(&mut self) {
        let st = self.vmm.agent().status();
        if st.sessions != self.agent_sessions {
            self.agent_sessions = st.sessions;
            self.note(&format!("guest agent connected ({})", st.agent));
        }
    }

    fn act(&mut self, a: Action) -> Flow {
        match a {
            Action::PowerOff => {
                self.vmm.shutdown();
                eprint!("\r\n[vroom] stopped\r\n");
                return Flow::Exit;
            }
            Action::Snapshot => {
                let d = self.state_dir.join("snapshots").join(format!("snap-{}", timestamp()));
                match self.vmm.snapshot(&d, "manual") {
                    Ok(()) => self.note(&format!("snapshot: {}", d.display())),
                    Err(e) => self.note(&format!("snapshot failed: {e}")),
                }
            }
            Action::Suspend => {
                let d = self.state_dir.join("suspend");
                let _ = std::fs::remove_dir_all(&d);
                match self.vmm.suspend_in_place(&d) {
                    Ok(()) => {
                        eprint!("\r\n[vroom] suspended to {}\r\n", d.display());
                        return Flow::Exit;
                    }
                    Err(e) => self.note(&format!("suspend failed: {e}")),
                }
            }
            Action::TogglePause => {
                let a = if self.vmm.state() == RunState::Paused { Action::Resume } else { Action::Pause };
                return self.act(a);
            }
            Action::Pause => match self.vmm.pause() {
                Ok(()) => self.note("paused"),
                Err(e) => self.note(&e.to_string()),
            },
            Action::Resume => match self.vmm.resume() {
                Ok(()) => self.note("running"),
                Err(e) => self.note(&e.to_string()),
            },
            Action::Reset => {
                if let Err(e) = self.vmm.reset() {
                    self.note(&format!("reset failed: {e}"));
                }
            }
            Action::GuestShutdown => {
                self.vmm.agent().send(AgentCmd::Shutdown { reboot: false });
                self.note("asked guest to shut down");
            }
            Action::PushClipboard => {
                self.vmm.agent().send(AgentCmd::PushClipboard);
                self.note("host clipboard sent to guest");
            }
            Action::Screendump => {
                let p = self.state_dir.join(format!("screen-{}.png", timestamp()));
                match self.vmm.screendump(&p) {
                    Ok((w, h)) => self.note(&format!("screen {w}x{h}: {}", p.display())),
                    Err(e) => self.note(&format!("screendump: {e}")),
                }
            }
            Action::PullClipboard => {
                self.vmm.agent().send(AgentCmd::PullClipboard);
                self.note("guest clipboard copied to host");
            }
        }
        Flow::Continue
    }

    fn event(&mut self, ev: VmEvent) -> Result<Flow, String> {
        Ok(match ev {
            VmEvent::Debug(stop) => {
                self.vmm.pause().map_err(|e| e.to_string())?;
                self.note(&format!("debug stop: CPU {} at {:#x} ({:?})", stop.cpu, stop.pc, stop.reason));
                Flow::Continue
            }
            VmEvent::Shutdown => {
                self.vmm.shutdown();
                eprint!("\r\n[vroom] guest powered off\r\n");
                Flow::Exit
            }
            VmEvent::Reset => {
                self.vmm.reset().map_err(|e| format!("reboot failed: {e}"))?;
                Flow::Continue
            }
            VmEvent::Crashed(e) => {
                self.vmm.shutdown();
                return Err(format!("vm crashed: {e}"));
            }
            VmEvent::Ui(c) => self.act(match c {
                Command::Pause => Action::Pause,
                Command::Resume => Action::Resume,
                Command::Snapshot => Action::Snapshot,
                Command::Reset => Action::Reset,
                Command::Shutdown => Action::GuestShutdown,
                Command::PowerOff => Action::PowerOff,
                Command::Suspend => Action::Suspend,
            }),
            // The window went away without asking (closing it asks for a
            // suspend): the app crashed. Keep running for it to come back.
            VmEvent::ViewerClosed => {
                if self.viewer_lost.is_none() {
                    self.viewer_lost = Some(std::time::Instant::now());
                    self.note("window gone; the VM keeps running for vroom to show it again, and suspends in 5 minutes");
                }
                Flow::Continue
            }
            VmEvent::ViewerAttached => {
                self.viewer_lost = None;
                self.note("window back");
                Flow::Continue
            }
        })
    }
}

fn escape_action(b: u8) -> Option<Action> {
    Some(match b {
        b'x' => Action::PowerOff,
        b's' => Action::Snapshot,
        b'z' => Action::Suspend,
        b'p' => Action::TogglePause,
        b'r' => Action::Reset,
        b'd' => Action::GuestShutdown,
        b'c' => Action::PushClipboard,
        b'v' => Action::PullClipboard,
        b'g' => Action::Screendump,
        _ => return None,
    })
}

/// `Vmm::start_confined`'s error: the VMM's or the sandbox's, as text.
struct StartError(String);

impl From<vroom_vmm::Error> for StartError {
    fn from(e: vroom_vmm::Error) -> Self {
        StartError(e.to_string())
    }
}

pub fn run(o: RunOpts) -> Result<(), String> {
    std::fs::create_dir_all(&o.state_dir).map_err(|e| format!("{}: {e}", o.state_dir.display()))?;
    if o.config.display.gpu == vroom_config::Gpu::Virtio3d {
        // DXMT keeps the Metal versions of Direct3D shaders: per VM, so
        // one guest can't plant them for another. Set before any thread
        // (DXMT reads it when Windows first draws).
        std::env::set_var("DXMT_SHADER_CACHE_PATH", o.state_dir.join("shaders"));
    }
    let suspend = o.state_dir.join("suspend");
    if suspend.is_dir() && !suspend.join("manifest.json").exists() {
        // Killed while suspending: the image is incomplete. The disks are
        // as the guest left them, like after a power cut.
        eprintln!("vroom: the last suspend didn't finish (the VM was killed while saving); booting fresh");
        let _ = std::fs::remove_dir_all(&suspend);
    }
    let start = match (&o.restore, o.fresh) {
        (Some(p), _) => StartMode::Restore(p.clone()),
        (None, false) if suspend.join("manifest.json").exists() => {
            eprintln!("vroom: resuming suspended VM");
            StartMode::Restore(suspend.clone())
        }
        _ => StartMode::Boot,
    };
    let resumed_suspend = matches!(&start, StartMode::Restore(p) if *p == suspend);

    // SAFETY: --ui-fd is a socket the GUI passed to us and nobody else owns.
    let ui = o.ui_fd.map(|fd| unsafe { UnixStream::from_raw_fd(fd) });
    let gui = ui.is_some();
    let clipboard = if gui { None } else { o.clipboard };
    let has_clip = clipboard.is_some();
    let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
    let opts = VmmOptions { serial_out: term::guest_out(), start, clipboard, ui, state_dir: o.state_dir.clone(), sandbox: o.sandbox };
    // Built with every CPU parked; the guest runs only once we're locked
    // down (a resumed guest would go on right where it was).
    let vmm = Vmm::start_confined(o.config, hv, opts, |vmm| {
        // A suspend image is single use: the disks move on from here.
        // Linux: before lock-down, seccomp has no unlinkat (a failed
        // lock-down then loses the image). macOS: after, so a failed
        // lock-down leaves it for the next try.
        let early = cfg!(target_os = "linux");
        if resumed_suspend && early {
            let _ = std::fs::remove_dir_all(&suspend);
        }
        if o.sandbox && vmm.config().security.sandbox {
            lock_down(vmm.config(), vmm.later_shares(), &o.state_dir, has_clip, vmm.helper_pids()).map_err(StartError)?;
        } else {
            eprintln!("vroom: WARNING: running without sandbox");
        }
        if resumed_suspend && !early {
            let _ = std::fs::remove_dir_all(&suspend);
        }
        Ok(())
    })
    .map_err(|StartError(e)| e)?;
    let mut s = Session { vmm, state_dir: o.state_dir, agent_sessions: 0, viewer_lost: None,
        next_space_check: std::time::Instant::now(),
        low_space: std::env::var("VROOM_LOW_SPACE_MIB").ok().and_then(|v| v.parse::<u64>().ok()).map_or(LOW_SPACE, |m| m << 20),
    };
    s.vmm.viewer_status("running");

    let _raw = if gui { None } else { Some(term::RawTerminal::enable()) };
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut buf = [0u8; 256];
            let mut stdin = std::io::stdin();
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;

    let mut escape = false;
    loop {
        while let Ok(bytes) = rx.try_recv() {
            let mut fwd = Vec::with_capacity(bytes.len());
            for b in bytes {
                if escape {
                    escape = false;
                    if b == 0x01 {
                        fwd.push(0x01);
                    } else if let Some(a) = escape_action(b) {
                        if let Flow::Exit = s.act(a) {
                            return Ok(());
                        }
                    } else {
                        eprint!("{HELP}");
                    }
                } else if b == 0x01 {
                    escape = true;
                } else {
                    fwd.push(b);
                }
            }
            if !fwd.is_empty() {
                if let Err(e) = s.vmm.serial_input(&fwd) {
                    eprintln!("vroom: {e}");
                }
            }
        }
        if let Some(ev) = s.vmm.next_event(Duration::from_millis(20)) {
            if let Flow::Exit = s.event(ev)? {
                return Ok(());
            }
        }
        s.check_agent();
        s.check_space();
        if s.viewer_lost.is_some_and(|t| t.elapsed() > VIEWER_GRACE) {
            // Nobody came back: save it rather than run unseen. If that
            // can't be done, it stays up for a window to come and say why.
            s.viewer_lost = None;
            s.note("no window came back: suspending");
            if let Flow::Exit = s.act(Action::Suspend) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::plan;

    #[test]
    fn selftest_fatal_probes_last_and_one_per_run() {
        assert_eq!(plan(false, false, None), (vec!["net", "exec", "listen", "unix"], vec![]));
        assert_eq!(plan(true, true, None), (vec!["net"], vec!["exec", "listen", "unix"]));
        assert_eq!(plan(true, false, None), (vec![], vec!["net", "exec", "listen", "unix"]));
        assert_eq!(plan(true, true, Some("listen")), (vec!["net", "listen"], vec!["exec", "unix"]));
        assert_eq!(plan(true, false, Some("net")), (vec!["net"], vec!["exec", "listen", "unix"]));
        assert_eq!(plan(true, true, Some("bogus")).0, vec!["net"]);
    }
}
