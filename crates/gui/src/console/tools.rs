//! The VM window's guest tools button. It shows when the guest's agent
//! is older than the tools this app carries (vroom_tools), or when no
//! agent came. A click:
//! - an agent that can update itself gets the new tools over the agent
//!   channel (a pipe the VMM checks and forwards; the agent checks every
//!   file against the manifest's SHA-256 and installs them);
//! - otherwise the tools disk goes in as a USB stick (read-only), and the
//!   window says what to run in the guest. Linux: the command is also put
//!   on the Mac's clipboard, for an SSH session.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use vroom_display::{ToVmm, UsbDev};
use vroom_tools::{Os, Version};

use super::{with, Console, VmState};

/// The tools stick's USB id (pid.codes' test product): ours, never a
/// device of the Mac's.
pub(super) const STICK_ID: &str = "1209:0001";
/// Running this long without an agent: offer the tools.
const MISSING_AFTER: Duration = Duration::from_secs(90);
/// An update that hasn't brought the new agent by then didn't work.
const UPDATE_WAIT: Duration = Duration::from_secs(180);
/// How long the instructions stay up.
const HOW_LONG: Duration = Duration::from_secs(20);

/// What to run in a Linux guest with the stick in.
pub(super) const LINUX_COMMAND: &str = "sudo mount -o ro LABEL=VRTOOLS /mnt && sudo sh /mnt/linux/install-agent.sh";

#[derive(Debug, Default)]
pub(super) struct Tools {
    /// The agent's hello text, once one came.
    agent: Option<String>,
    state: State,
    running_since: Option<Instant>,
    /// The tools stick is in: taken out once the new agent is there.
    stick: bool,
    updates: u32,
}

#[derive(Debug, Default, Clone, PartialEq)]
enum State {
    #[default]
    Hidden,
    /// No agent came.
    Missing,
    /// Newer tools here. `push`: the agent installs them itself.
    Old { theirs: Version, ours: Version, push: bool },
    Updating { since: Instant, to: Version },
}

thread_local! {
    /// The tools' versions, read once (the agent binaries are megabytes).
    static VERSIONS: std::cell::RefCell<HashMap<&'static str, Option<Version>>> = std::cell::RefCell::new(HashMap::new());
}

fn tools_version(os: Os) -> Option<Version> {
    VERSIONS.with(|v| v.borrow_mut().entry(os.dir_name()).or_insert_with(|| vroom_tools::version(os)).clone())
}

impl Console {
    fn tools_os(&self) -> Os {
        let hello = self.tools.agent.as_deref().and_then(vroom_proto::version::hello_os).and_then(Os::from_name);
        hello.unwrap_or(if self.windows_guest { Os::Windows } else { Os::Linux })
    }

    /// No agent said hello yet in this window.
    pub(super) fn tools_agent_first(&self) -> bool {
        self.tools.agent.is_none()
    }

    /// An agent said hello: "vroom-agent 0.1.0+348.e314084680 linux".
    pub(super) fn tools_agent(&mut self, agent: &str) {
        self.tools.agent = Some(agent.to_string());
        let theirs = Version::from_hello(agent);
        let ours = tools_version(self.tools_os());
        let was = std::mem::take(&mut self.tools.state);
        self.tools.state = match (&theirs, &ours) {
            (Some(t), Some(o)) if t.older_than(o) => {
                let push = vroom_tools::update_for(agent, self.features, o) == Some(true);
                State::Old { theirs: t.clone(), ours: o.clone(), push }
            }
            _ => State::Hidden,
        };
        if let (State::Updating { to, .. }, Some(t)) = (&was, &theirs) {
            if t.older_than(to) {
                self.toast("The guest tools update didn't take: the agent's log in the guest says why");
            } else {
                self.toast(&format!("Guest tools updated ({t})"));
            }
        }
        if self.tools.state == State::Hidden && self.tools.stick {
            self.tools_unplug();
        }
        self.tools_show();
    }

    /// The VMM said an update failed (the text is shown as it is).
    pub(super) fn tools_failed(&mut self) {
        if let State::Updating { .. } = self.tools.state {
            self.tools.state = State::Hidden;
            if let Some(a) = self.tools.agent.clone() {
                self.tools_agent(&a);
            }
        }
    }

    pub(super) fn tools_tick(&mut self, now: Instant) {
        if self.state != VmState::Running {
            self.tools.running_since = None;
            return;
        }
        let since = *self.tools.running_since.get_or_insert(now);
        match &self.tools.state {
            State::Hidden if self.tools.agent.is_none() && now.duration_since(since) > MISSING_AFTER => {
                self.tools.state = State::Missing;
                self.tools_show();
            }
            State::Updating { since, .. } if now.duration_since(*since) > UPDATE_WAIT => {
                self.toast("The guest tools update didn't finish: the agent's log in the guest says why");
                self.tools_failed();
            }
            _ => {}
        }
    }

    fn tools_show(&self) {
        let (n, tip) = match &self.tools.state {
            State::Hidden => (0, String::new()),
            State::Missing => (1, "Install guest tools: clipboard, resizing, shared folders".to_string()),
            State::Old { theirs, ours, push: true } => (2, format!("Update guest tools ({theirs} → {ours})")),
            State::Old { theirs, ours, push: false } => (2, format!("Install the new guest tools ({theirs} → {ours})")),
            State::Updating { to, .. } => (3, format!("Updating guest tools to {to}")),
        };
        self.win.set_tools(n);
        self.win.set_tools_tip(tip.into());
    }

    /// For the settings: the guest's tools version, ours, and what the
    /// button does ("" while updating).
    pub(super) fn tools_info(&self) -> (String, String, &'static str) {
        let have = self.tools.agent.as_deref().and_then(Version::from_hello).map(|v| v.to_string()).unwrap_or_default();
        let ours = tools_version(self.tools_os()).map(|v| v.to_string()).unwrap_or_default();
        let action = match &self.tools.state {
            State::Updating { .. } => "",
            State::Old { .. } => "Update",
            State::Missing => "Install",
            State::Hidden if self.tools.agent.is_some() => "Reinstall",
            State::Hidden => "Install",
        };
        (have, ours, action)
    }

    /// The settings' button: the toolbar's, and a reinstall when current.
    pub(super) fn tools_act(&mut self) {
        match self.tools.state.clone() {
            State::Hidden => match tools_version(self.tools_os()) {
                Some(ours) if self.tools.agent.is_some() && self.features & vroom_proto::features::UPDATE != 0 => self.tools_push(ours),
                _ => self.tools_plug(),
            },
            _ => self.tools_click(),
        }
    }

    /// The button.
    pub(super) fn tools_click(&mut self) {
        match self.tools.state.clone() {
            State::Hidden | State::Updating { .. } => {}
            State::Old { ours, push: true, .. } => self.tools_push(ours),
            State::Missing | State::Old { push: false, .. } => self.tools_plug(),
        }
    }

    /// Send the new tools to the agent.
    fn tools_push(&mut self, to: Version) {
        let os = self.tools_os();
        let Some(dir) = vroom_tools::os_dir(os) else {
            self.toast("This app has no guest tools to send");
            return;
        };
        self.tools.updates += 1;
        let id = self.tools.updates;
        let sent = UnixStream::pair().and_then(|(ours, theirs)| {
            let io = self.io.borrow();
            let tx = io.tx.as_ref().ok_or_else(|| std::io::Error::other("the VM is gone"))?;
            vroom_display::send_fd_with(tx, &ToVmm::AgentUpdate.encode(), theirs.as_raw_fd()).map_err(|e| std::io::Error::other(e.to_string()))?;
            // As for drops: not closed at once (macOS may pass on a socket
            // already shut when our copy goes while it is in flight).
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                drop(theirs);
            });
            Ok(ours)
        });
        let key = self.key.clone();
        let version = to.to_string();
        let r = sent.and_then(|mut ours| {
            std::thread::Builder::new().name("tools-update".into()).spawn(move || {
                if let Err(e) = vroom_tools::bundle::write(&mut ours, id, &version, &dir) {
                    log::warn!("guest tools update: {e}");
                    let _ = slint::invoke_from_event_loop(move || {
                        with(&key, |c| {
                            c.toast(&format!("guest tools update failed: {e}"));
                            c.tools_failed();
                        });
                    });
                }
            })
        });
        match r {
            Ok(_) => {
                self.tools.state = State::Updating { since: Instant::now(), to };
                self.tools_show();
            }
            Err(e) => self.toast(&format!("guest tools update failed: {e}")),
        }
    }

    /// Put the tools disk in as a USB stick and say what to run.
    fn tools_plug(&mut self) {
        let Some(img) = vroom_tools::image() else {
            self.toast("This app has no guest tools disk (scripts/make-guest-tools.sh builds it)");
            return;
        };
        let os = self.tools_os();
        if !self.usb_controller() {
            self.toast_for(&format!("This VM has no USB: attach {} as a disk in its settings", img.display()), HOW_LONG);
            return;
        }
        if !self.tools.stick {
            let file = match std::fs::File::open(&img) {
                Ok(f) => f,
                Err(e) => return self.toast(&format!("{}: {e}", img.display())),
            };
            let dev = UsbDev { id: STICK_ID.into(), serial: String::new(), name: "vroom tools".into() };
            let sent = match self.io.borrow().tx.as_ref() {
                Some(tx) => vroom_display::send_fd_with(tx, &ToVmm::UsbDisk { dev, readonly: true }.encode(), file.as_raw_fd()).map_err(|e| e.to_string()),
                None => Err("the VM is gone".into()),
            };
            if let Err(e) = sent {
                return self.toast(&format!("guest tools: {e}"));
            }
            self.tools_hold(file);
            self.tools.stick = true;
        }
        match os {
            Os::Linux => {
                if let Some(cb) = self.clipboard.as_mut() {
                    let _ = cb.set_text(LINUX_COMMAND.to_string());
                }
                self.toast_for(&format!("Tools disk in. In the guest (copied): {LINUX_COMMAND}"), HOW_LONG);
            }
            Os::Windows => self.toast_for("Tools disk in. In Windows: open the VRTOOLS drive, run vroom-setup", HOW_LONG),
        }
    }

    /// Keep our copy of the stick's fd until the VMM surely has it.
    fn tools_hold(&mut self, f: std::fs::File) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            drop(f);
        });
    }

    fn tools_unplug(&mut self) {
        if let Some(port) = self.usb_port_of(STICK_ID) {
            self.io.borrow_mut().send(ToVmm::UsbDetach { port });
        }
        self.tools.stick = false;
    }
}
