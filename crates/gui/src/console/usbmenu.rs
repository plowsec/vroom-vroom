//! The VM window's USB menu: the Mac's USB devices, connected to the VM
//! or let go with a click while it runs, and "Always connect to this VM"
//! (the VM file's `[[usb]]`).
//!
//! We start the device's helper (or vroom-netd does, as root, for a
//! device a macOS driver holds) and hand the VMM its socket. The VMM says
//! what it has plugged in (`ToViewer::Usb`); the menu shows that.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use slint::{ModelRc, VecModel};
use vroom_display::{ToVmm, UsbDev, UsbPort};
use vroom_usb::list::HostDevice;

use super::{with, Console};
use crate::hostusb;
use crate::rootd::{self, Helper};
use crate::UsbMenuRow;

/// How long we keep our copy of a helper socket we handed the VMM: macOS
/// can drop a socket whose other copies all close while it is in flight.
const HOLD: Duration = Duration::from_secs(5);
/// A connect or let go the VMM never answered.
const BUSY_FOR: Duration = Duration::from_secs(15);
/// How often the Mac's devices are listed again while the menu is open.
const LIST_EVERY: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct UsbMenu {
    /// What the VMM has plugged in. None: no USB controller (yet).
    ports: Option<Vec<UsbPort>>,
    host: Vec<HostDevice>,
    listed: Option<Instant>,
    /// What each row of the menu is.
    targets: Vec<Target>,
    /// Devices being connected or let go, since when.
    busy: HashMap<String, Instant>,
    /// Our copies of sockets handed to the VMM, until it has them.
    held: Vec<(Instant, UnixStream)>,
    helper: Helper,
    setup_busy: bool,
    /// The devices the VM file connects at every start.
    always: Vec<(String, Option<String>)>,
    /// Connected for this run only, before a suspend: plugged back in
    /// once the resumed VM has its USB controller.
    resume: Option<Vec<UsbDev>>,
}

/// Where a VM's devices connected for this run only are kept: a resumed
/// VM gets them back. One line each: id, serial, name (tab-separated).
fn session_file(key: &str) -> std::path::PathBuf {
    crate::paths::data_dir().join("usb-session").join(key)
}

fn read_session(key: &str) -> Vec<UsbDev> {
    let s = std::fs::read_to_string(session_file(key)).unwrap_or_default();
    s.lines()
        .take(16)
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let d = UsbDev { id: f.next()?.into(), serial: f.next()?.into(), name: f.next().unwrap_or("").into() };
            d.valid().then_some(d)
        })
        .collect()
}

fn write_session(key: &str, devs: &[UsbDev]) {
    let p = session_file(key);
    if devs.is_empty() {
        let _ = std::fs::remove_file(&p);
        return;
    }
    let text: String = devs.iter().map(|d| format!("{}\t{}\t{}\n", d.id, d.serial, d.name)).collect();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::library::write_atomic(&p, text.as_bytes());
}

#[derive(Clone)]
struct Target {
    dev: UsbDev,
    host: Option<HostDevice>,
    port: Option<u8>,
}

fn key_of(d: &UsbDev) -> String {
    format!("{}/{}", d.id, d.serial)
}

/// The same device? A side without a serial number matches by id.
fn same(a: &UsbDev, b: &UsbDev) -> bool {
    a.id == b.id && (a.serial == b.serial || a.serial.is_empty() || b.serial.is_empty())
}

thread_local! {
    /// Each window's VM name and plugged-in devices, so one window can say
    /// "in use by <VM>" for another's.
    static TAKEN: RefCell<HashMap<String, (String, Vec<UsbPort>)>> = RefCell::new(HashMap::new());
}

/// The window `key` is gone.
pub(super) fn forget(key: &str) {
    TAKEN.with(|t| t.borrow_mut().remove(key));
}

/// What a macOS driver uses the device as, in words.
fn used_as(drivers: &[String]) -> String {
    let all = drivers.join(" ");
    let has = |w: &str| all.contains(w);
    if has("MassStorage") || has("SCSI") {
        "a disk".into()
    } else if has("HID") {
        "a keyboard, mouse or similar".into()
    } else if has("CDC") || has("Serial") || has("FTDI") || has("SiLabs") || has("CH34") || has("PL2303") {
        "a serial port".into()
    } else if has("Audio") {
        "a sound device".into()
    } else if has("Video") || has("UVC") || has("Camera") {
        "a camera".into()
    } else if has("Ethernet") || has("NCM") || has("ECM") {
        "a network adapter".into()
    } else {
        drivers.first().cloned().unwrap_or_default()
    }
}

impl Console {
    /// Starting the VM: if it resumes from a suspend, the devices it had
    /// for that run come back; else that list is stale.
    pub(super) fn usb_prepare(&mut self, resuming: bool) {
        if resuming {
            let l = read_session(&self.key);
            self.usb.resume = (!l.is_empty()).then_some(l);
        } else {
            write_session(&self.key, &[]);
        }
    }

    /// The VMM's list of what it has plugged in.
    pub(super) fn usb_list(&mut self, l: Vec<UsbPort>) {
        TAKEN.with(|t| t.borrow_mut().insert(self.key.clone(), (self.name.clone(), l.clone())));
        // Remember what this run connected beyond the VM file's devices.
        let always: Vec<(String, Option<String>)> =
            vroom_config::VmConfig::load_raw(&self.path).map(|c| c.usb.into_iter().map(|u| (u.id, u.serial)).collect()).unwrap_or_default();
        let session: Vec<UsbDev> = l
            .iter()
            .filter(|p| !always.iter().any(|(id, serial)| *id == p.dev.id && !serial.as_deref().is_some_and(|s| s != p.dev.serial)))
            .map(|p| p.dev.clone())
            .collect();
        // Not before the resume put them back: an empty first list would
        // forget them.
        if self.usb.resume.is_none() {
            write_session(&self.key, &session);
        }
        let m = &mut self.usb;
        m.ports = Some(l);
        // It has the sockets it got (or refused them): let ours go.
        m.held.clear();
        m.busy.clear();
        self.win.set_usb_shown(true);
        self.win.set_usb_count(self.usb.ports.as_ref().map_or(0, |p| p.len()) as i32);
        if let Some(want) = self.usb.resume.take() {
            let host = hostusb::host_list();
            let have = self.usb.ports.clone().unwrap_or_default();
            for d in want {
                if have.iter().any(|p| same(&p.dev, &d)) {
                    continue;
                }
                match host.iter().find(|h| same(&hostusb::dev_of(h), &d)) {
                    Some(h) => self.usb_connect(hostusb::dev_of(h), Some(h.clone())),
                    None => self.toast(&format!("{}: not plugged into the Mac, not connected again", if d.name.is_empty() { &d.id } else { &d.name })),
                }
            }
        }
        if self.win.get_usb_open() {
            self.usb_rows();
        }
    }

    /// "USB: <why>" from the VMM: a device it couldn't take.
    pub(super) fn usb_refused(&mut self) {
        self.usb.held.clear();
        self.usb.busy.clear();
        if self.win.get_usb_open() {
            self.usb_rows();
        }
    }

    pub(super) fn usb_tick(&mut self, now: Instant) {
        let m = &mut self.usb;
        m.held.retain(|(t, _)| now.duration_since(*t) < HOLD);
        let n = m.busy.len();
        m.busy.retain(|_, t| now.duration_since(*t) < BUSY_FOR);
        let stale = m.listed.is_none_or(|t| now.duration_since(t) >= LIST_EVERY);
        if self.win.get_usb_open() && (stale || n != self.usb.busy.len()) {
            self.usb_rows();
        }
    }

    /// The menu's commands; false if `cmd` isn't one.
    pub(super) fn usb_command(&mut self, cmd: &str) -> bool {
        if cmd == "usb-menu" {
            let open = !self.win.get_usb_open();
            self.win.set_usb_open(open);
            if open {
                self.usb.helper = rootd::state();
                self.usb_rows();
                // A fresh word from the helper, without holding up the menu.
                let k = self.key.clone();
                std::thread::spawn(move || {
                    let h = rootd::state_now();
                    let _ = slint::invoke_from_event_loop(move || {
                        with(&k, |c| {
                            if c.usb.helper != h {
                                c.usb.helper = h;
                                c.usb_rows();
                            }
                        });
                    });
                });
            }
            return true;
        }
        if let Some(i) = cmd.strip_prefix("usb.toggle.").and_then(|i| i.parse::<usize>().ok()) {
            self.usb_toggle(i);
        } else if let Some(i) = cmd.strip_prefix("usb.always.").and_then(|i| i.parse::<usize>().ok()) {
            self.usb_always(i);
        } else if cmd == "usb.setup" {
            self.usb_setup();
        } else {
            return false;
        }
        true
    }

    /// The menu's rows, from the Mac's devices and the VM's ports.
    fn usb_rows(&mut self) {
        let now = Instant::now();
        let m = &mut self.usb;
        m.host = hostusb::host_list();
        m.listed = Some(now);
        m.always = vroom_config::VmConfig::load_raw(&self.path).map(|c| c.usb.into_iter().map(|u| (u.id, u.serial)).collect()).unwrap_or_default();
        let ports = m.ports.clone().unwrap_or_default();
        let mut targets: Vec<Target> = m
            .host
            .iter()
            .map(|h| {
                let dev = hostusb::dev_of(h);
                let port = ports.iter().find(|p| same(&p.dev, &dev)).map(|p| p.port);
                Target { dev, host: Some(h.clone()), port }
            })
            .collect();
        // Plugged into the VM but not (or no longer) on the Mac.
        for p in &ports {
            if !targets.iter().any(|t| t.port == Some(p.port)) {
                targets.push(Target { dev: p.dev.clone(), host: None, port: Some(p.port) });
            }
        }
        let elsewhere = |d: &UsbDev| -> Option<String> {
            TAKEN.with(|t| t.borrow().iter().find(|(k, (_, l))| **k != self.key && l.iter().any(|p| same(&p.dev, d))).map(|(_, (n, _))| n.clone()))
        };
        let helper_usb = m.helper.usb();
        let mut needs_helper = false;
        let rows: Vec<UsbMenuRow> = targets
            .iter()
            .map(|t| {
                let busy = m.busy.contains_key(&key_of(&t.dev));
                let on = t.port.is_some();
                let here = t.port.and_then(|p| ports.iter().find(|x| x.port == p)).is_some_and(|p| p.here);
                let root = t.host.as_ref().is_some_and(hostusb::needs_root);
                let other = if on { None } else { elsewhere(&t.dev) };
                let always = m.always.iter().any(|(id, serial)| *id == t.dev.id && serial.as_deref().unwrap_or("") == t.dev.serial);
                let (note, warn) = if busy {
                    ((if on { "Letting go..." } else { "Connecting..." }).to_string(), false)
                } else if on && here {
                    ("Connected to this VM".into(), false)
                } else if on {
                    ("Connected, but not plugged into the Mac".into(), true)
                } else if let Some(n) = &other {
                    (format!("In use by {n}"), true)
                } else if root {
                    let what = used_as(&t.host.as_ref().map(|h| h.claimed_by.clone()).unwrap_or_default());
                    if helper_usb.is_ok() {
                        (format!("macOS uses it as {what}: the VM takes it while connected"), false)
                    } else {
                        needs_helper = true;
                        (format!("macOS uses it as {what}: needs vroom-netd"), true)
                    }
                } else if always {
                    ("Not connected now; connects at every start".into(), false)
                } else {
                    let fast = t.host.as_ref().and_then(|h| h.speed).is_some_and(|s| s.is_super());
                    (format!("{}{}", t.dev.id, if fast { " · USB 3" } else { "" }), false)
                };
                let label = if t.dev.name.is_empty() { t.dev.id.clone() } else { t.dev.name.clone() };
                UsbMenuRow { label: label.into(), note: note.into(), on, always, warn, busy, enabled: other.is_none() }
            })
            .collect();
        m.targets = targets;
        let (note, setup) = match (&m.helper, needs_helper) {
            (_, false) => (String::new(), false),
            (Helper::Ready(s), true) if !s.usb => (s.usb_why.clone(), false),
            (Helper::Old, true) => ("vroom-netd, vroom's helper that runs as root, is from an older vroom. Set it up again to connect devices macOS uses.".into(), true),
            (_, true) if !cfg!(target_os = "macos") => (String::new(), false),
            (_, true) => (
                "Devices macOS uses need vroom-netd, a small helper that runs as root and takes them from macOS for the VM. It's set up once; macOS asks for an admin password.".into(),
                true,
            ),
        };
        self.win.set_usb_rows(ModelRc::new(VecModel::from(rows)));
        self.win.set_usb_note(note.into());
        self.win.set_usb_setup(setup);
        self.win.set_usb_setup_busy(self.usb.setup_busy);
    }

    fn usb_toggle(&mut self, i: usize) {
        let Some(t) = self.usb.targets.get(i).cloned() else { return };
        let k = key_of(&t.dev);
        if self.usb.busy.contains_key(&k) {
            return;
        }
        if let Some(port) = t.port {
            self.usb.busy.insert(k, Instant::now());
            self.io.borrow_mut().send(ToVmm::UsbDetach { port });
            return self.usb_rows();
        }
        self.usb_connect(t.dev, t.host);
    }

    /// Start `dev`'s helper (as root through vroom-netd if a macOS driver
    /// holds it), then hand the VMM its socket.
    fn usb_connect(&mut self, dev: UsbDev, host: Option<HostDevice>) {
        let k = key_of(&dev);
        let root = host.as_ref().is_some_and(hostusb::needs_root);
        if root {
            if self.usb.helper == Helper::Unknown {
                self.usb.helper = rootd::state();
            }
            if let Err(why) = self.usb.helper.usb() {
                self.toast(&format!("{}: {why}", dev.name));
                return;
            }
        }
        self.usb.busy.insert(k, Instant::now());
        self.usb_rows();
        let (key, vm) = (self.key.clone(), self.name.clone());
        std::thread::spawn(move || {
            let r = if root { hostusb::via_netd(&vm, &dev).map(|s| (None, s)) } else { hostusb::spawn_local(&dev).map(|(c, s)| (Some(c), s)) };
            let _ = slint::invoke_from_event_loop(move || {
                with(&key, |c| c.usb_started(dev, r));
            });
        });
    }

    /// The helper for `dev` runs: hand its socket to the VMM.
    fn usb_started(&mut self, dev: UsbDev, r: Result<(Option<std::process::Child>, UnixStream), String>) {
        let (child, sock) = match r {
            Ok(x) => x,
            Err(e) => {
                self.usb.busy.remove(&key_of(&dev));
                self.toast(&format!("{}: {e}", if dev.name.is_empty() { &dev.id } else { &dev.name }));
                return self.usb_rows();
            }
        };
        if let Some(mut c) = child {
            // It ends when the VMM lets go of its socket.
            std::thread::spawn(move || {
                let _ = c.wait();
            });
        }
        let sent = match self.io.borrow().tx.as_ref() {
            Some(tx) => vroom_display::send_fd_with(tx, &ToVmm::UsbAttach(dev.clone()).encode(), sock.as_raw_fd()).map_err(|e| e.to_string()),
            None => Err("the VM is gone".into()),
        };
        match sent {
            Ok(()) => self.usb.held.push((Instant::now(), sock)),
            Err(e) => {
                self.usb.busy.remove(&key_of(&dev));
                self.toast(&format!("USB: {e}"));
            }
        }
    }

    /// The settings page put `d` in the VM file: connect it now too, as
    /// the menu does. Nothing if it is in already.
    pub(super) fn usb_plug(&mut self, d: UsbDev) {
        if self.usb.ports.is_none() {
            self.toast("This VM has no USB controller now: the device comes at its next start");
            return;
        }
        if self.usb.ports.iter().flatten().any(|p| same(&p.dev, &d)) {
            return;
        }
        match hostusb::host_list().into_iter().find(|h| same(&hostusb::dev_of(h), &d)) {
            Some(h) => self.usb_connect(hostusb::dev_of(&h), Some(h)),
            None => self.toast(&format!("{}: not plugged into the Mac now", if d.name.is_empty() { &d.id } else { &d.name })),
        }
    }

    /// The settings page took `id` (`serial`) out of the VM file: let go
    /// of it now.
    pub(super) fn usb_unplug(&mut self, id: &str, serial: Option<&str>) {
        let d = UsbDev { id: id.to_string(), serial: serial.unwrap_or_default().to_string(), name: String::new() };
        let Some(p) = self.usb.ports.iter().flatten().find(|p| same(&p.dev, &d)).cloned() else { return };
        self.usb.busy.insert(key_of(&p.dev), Instant::now());
        self.io.borrow_mut().send(ToVmm::UsbDetach { port: p.port });
    }

    fn usb_always(&mut self, i: usize) {
        let Some(t) = self.usb.targets.get(i).cloned() else { return };
        let on = !self.usb.always.iter().any(|(id, serial)| *id == t.dev.id && serial.as_deref().unwrap_or("") == t.dev.serial);
        match hostusb::set_always(&self.path, &t.dev, on) {
            Ok(()) => self.toast(if on { "Saved: it connects at every start" } else { "It no longer connects at start" }),
            Err(e) => self.toast(&format!("Couldn't save: {e}")),
        }
        self.usb_rows();
    }

    fn usb_setup(&mut self) {
        if self.usb.setup_busy {
            return;
        }
        self.usb.setup_busy = true;
        self.win.set_usb_setup_busy(true);
        let k = self.key.clone();
        rootd::in_background(rootd::install, move |r| {
            let done = with(&k, |c| {
                c.usb.setup_busy = false;
                c.usb.helper = rootd::state();
                match &r {
                    Ok(()) => c.toast("vroom-netd is set up"),
                    Err(e) if e == "cancelled" => {}
                    Err(e) => c.toast(&format!("Setup failed: {e}")),
                }
                c.usb_rows();
            });
            if done.is_none() {
                if let Err(e) = r {
                    crate::msg::toast(format!("Setup failed: {e}"), 3);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_and_words() {
        let d = |id: &str, s: &str| UsbDev { id: id.into(), serial: s.into(), name: String::new() };
        assert!(same(&d("1209:000a", ""), &d("1209:000a", "A1")), "no serial: by id");
        assert!(!same(&d("1209:000a", "B2"), &d("1209:000a", "A1")));
        assert!(!same(&d("1209:000b", ""), &d("1209:000a", "")));
        assert_eq!(used_as(&["IOUSBMassStorageInterfaceNub".into()]), "a disk");
        assert_eq!(used_as(&["AppleUserUSBHostHIDDevice".into()]), "a keyboard, mouse or similar");
        assert_eq!(used_as(&["AppleUSBCDCACMData".into()]), "a serial port");
        assert_eq!(used_as(&["SomethingElse".into()]), "SomethingElse");
    }

    #[test]
    fn devices_of_a_run_are_kept_for_its_resume() {
        let t = tempfile::tempdir().unwrap();
        crate::paths::TEST_HOME.with(|h| *h.borrow_mut() = Some(t.path().to_path_buf()));
        let d = |id: &str, s: &str, n: &str| UsbDev { id: id.into(), serial: s.into(), name: n.into() };
        let devs = vec![d("1209:000a", "A1", "dev board"), d("0781:5583", "", "")];
        write_session("k", &devs);
        assert_eq!(read_session("k"), devs);
        // A hand-edited or damaged file: bad lines are skipped.
        std::fs::write(session_file("k"), "1209:000a\tA1\tok\nnot a device\n../../x\t\t\n").unwrap();
        assert_eq!(read_session("k"), vec![d("1209:000a", "A1", "ok")]);
        write_session("k", &[]);
        assert!(!session_file("k").exists());
        assert!(read_session("nothing").is_empty());
    }
}
