//! `bulwark run`: one VM, serial console and/or GUI viewer.

use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use bulwark_config::{NetKind, VmConfig};
use bulwark_display::Command;
use bulwark_vmm::agent::{AgentCmd, ClipboardProvider};
use bulwark_vmm::{RunState, StartMode, VmEvent, Vmm, VmmOptions};

use crate::term;

const HELP: &str = "\r\n[bulwark] Ctrl-A x: power off | s: snapshot | z: suspend | p: pause/resume | r: reset\r\n          d: ask guest to shut down | c: host clipboard -> guest | v: guest clipboard -> host\r\n          g: screendump (png)\r\n";

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
fn lock_down(cfg: &VmConfig, state_dir: &Path, clipboard: bool) -> Result<(), String> {
    let mut p = bulwark_sandbox::Policy {
        write_dirs: vec![state_dir.to_path_buf()],
        clipboard,
        gpu: cfg.display.gpu == bulwark_config::Gpu::Virtio3d,
        ..Default::default()
    };
    for d in &cfg.disks {
        if d.readonly {
            p.ro_files.push(d.path.clone());
        } else {
            p.rw_files.push(d.path.clone());
        }
    }
    for n in &cfg.nets {
        if n.mode == NetKind::Nat {
            p.network_outbound = true;
            p.listen_ports.extend(n.port_forwards.iter().map(|f| f.host_port));
        }
    }
    bulwark_sandbox::apply(&p).map_err(|e| e.to_string())?;
    log::info!("sandbox applied");
    if std::env::var_os("BULWARK_SANDBOX_SELFTEST").is_some() {
        selftest();
    }
    Ok(())
}

fn selftest() {
    let probe = |path: &str| match std::fs::read(path) {
        Ok(_) => eprintln!("sandbox selftest: {path} READABLE (bad)"),
        Err(e) => eprintln!("sandbox selftest: {path} blocked ({e})"),
    };
    probe("/etc/passwd");
    probe(&format!("{}/.ssh/known_hosts", std::env::var("HOME").unwrap_or_default()));
    match std::process::Command::new("/bin/echo").arg("x").output() {
        Ok(_) => eprintln!("sandbox selftest: exec ALLOWED (bad)"),
        Err(e) => eprintln!("sandbox selftest: exec blocked ({e})"),
    }
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(_) => eprintln!("sandbox selftest: new listener ALLOWED (bad)"),
        Err(e) => eprintln!("sandbox selftest: new listener blocked ({e})"),
    }
}

struct Session {
    vmm: Vmm,
    state_dir: PathBuf,
}

impl Session {
    fn note(&self, msg: &str) {
        eprint!("\r\n[bulwark] {msg}\r\n");
        self.vmm.viewer_status(msg);
    }

    fn act(&mut self, a: Action) -> Flow {
        match a {
            Action::PowerOff => {
                self.vmm.shutdown();
                eprint!("\r\n[bulwark] stopped\r\n");
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
                        eprint!("\r\n[bulwark] suspended to {}\r\n", d.display());
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
            VmEvent::Shutdown => {
                self.vmm.shutdown();
                eprint!("\r\n[bulwark] guest powered off\r\n");
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
            // Viewer went away without saying goodbye: keep state, suspend.
            VmEvent::ViewerClosed => self.act(Action::Suspend),
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

pub fn run(o: RunOpts) -> Result<(), String> {
    std::fs::create_dir_all(&o.state_dir).map_err(|e| format!("{}: {e}", o.state_dir.display()))?;
    let suspend = o.state_dir.join("suspend");
    let start = match (&o.restore, o.fresh) {
        (Some(p), _) => StartMode::Restore(p.clone()),
        (None, false) if suspend.join("manifest.json").exists() => {
            eprintln!("bulwark: resuming suspended VM");
            StartMode::Restore(suspend.clone())
        }
        _ => StartMode::Boot,
    };
    let resumed_suspend = matches!(&start, StartMode::Restore(p) if *p == suspend);

    let ui = match o.ui_fd {
        // SAFETY: the parent (bulwark-gui) passed us this fd and nothing
        // else in this process uses it.
        Some(fd) => Some(unsafe { UnixStream::from_raw_fd(fd) }),
        None => None,
    };
    let gui = ui.is_some();
    let clipboard = if gui { None } else { o.clipboard };
    let has_clip = clipboard.is_some();
    let hv = bulwark_hv::default_hypervisor().map_err(|e| e.to_string())?;
    let opts = VmmOptions { serial_out: Box::new(std::io::stdout()), start, clipboard, ui, state_dir: o.state_dir.clone() };
    let vmm = Vmm::start(o.config, hv, opts).map_err(|e| e.to_string())?;
    if resumed_suspend {
        // A suspend image is single use: the disks move on from here.
        let _ = std::fs::remove_dir_all(&suspend);
    }

    if o.sandbox && vmm.config().security.sandbox {
        lock_down(vmm.config(), &o.state_dir, has_clip)?;
    } else {
        eprintln!("bulwark: WARNING: running without sandbox");
    }
    let mut s = Session { vmm, state_dir: o.state_dir };
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
                s.vmm.serial_input(&fwd);
            }
        }
        if let Some(ev) = s.vmm.next_event(Duration::from_millis(20)) {
            if let Flow::Exit = s.event(ev)? {
                return Ok(());
            }
        }
    }
}
