//! The VM window's USB menu: the Mac's USB devices, connected to the VM
//! or let go with a click while it runs, and "Always connect to this VM"
//! (the VM file's `[[usb]]`).
//!
//! We start the device's helper (or vroom-netd does, as root, for a
//! device a macOS driver holds) and hand the VMM its socket. The VMM says
//! what it has plugged in (`ToViewer::Usb`); the menu shows that.
//!
//! A USB disk can go three ways (`usbdisk.rs`): its files as a shared
//! folder, the whole disk as a plain USB stick, or the device itself.
//! Clicking one shows the three, with "Remember for this device"; a
//! remembered choice (or the VM file's `mode`) skips the question.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use slint::{ModelRc, VecModel};
use vroom_config::UsbMode;
use vroom_display::{ToVmm, UsbDev, UsbHow, UsbPort};
use vroom_usb::list::HostDevice;

use super::{with, Console};
use crate::rootd::{self, Helper};
use crate::UsbMenuRow;
use crate::{hostusb, usbdisk};

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
    /// Our copies of sockets and fds handed to the VMM, until it has them.
    held: Vec<(Instant, std::os::fd::OwnedFd)>,
    helper: Helper,
    setup_busy: bool,
    /// The devices the VM file connects at every start, and how.
    always: Vec<(String, Option<String>, UsbMode)>,
    /// Connected for this run only, before a suspend: plugged back in
    /// once the resumed VM has its USB controller.
    resume: Option<Vec<(UsbDev, UsbMode)>>,
    /// The VM file's disks the app connects (files, disk): done once the
    /// VM has its USB controller.
    started: bool,
    /// The disk whose three ways are open, and "Remember for this
    /// device".
    choosing: Option<String>,
    remember: bool,
    /// Disks we unmounted on the Mac for the VM: (device key, BSD name,
    /// when, seen in the VMM's list yet). macOS mounts them again once
    /// the VM lets go.
    taken: Vec<(String, String, Instant, bool)>,
    /// Why the last disk couldn't be connected: stays in the menu (it
    /// may say what to allow in System Settings) until it is opened again.
    error: Option<String>,
}

/// Where a VM's devices connected for this run only are kept: a resumed
/// VM gets them back. One line each: id, serial, name, and how (files,
/// disk; none: the device), tab-separated.
fn session_file(key: &str) -> std::path::PathBuf {
    crate::paths::data_dir().join("usb-session").join(key)
}

fn read_session(key: &str) -> Vec<(UsbDev, UsbMode)> {
    let s = std::fs::read_to_string(session_file(key)).unwrap_or_default();
    s.lines()
        .take(16)
        .filter_map(|l| {
            let mut f = l.splitn(4, '\t');
            let d = UsbDev { id: f.next()?.into(), serial: f.next()?.into(), name: f.next().unwrap_or("").into() };
            let mode = f.next().and_then(UsbMode::parse).unwrap_or_default();
            d.valid().then_some((d, mode))
        })
        .collect()
}

fn write_session(key: &str, devs: &[(UsbDev, UsbMode)]) {
    let p = session_file(key);
    if devs.is_empty() {
        let _ = std::fs::remove_file(&p);
        return;
    }
    let text: String = devs.iter().map(|(d, m)| format!("{}\t{}\t{}\t{}\n", d.id, d.serial, d.name, m.as_str())).collect();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::library::write_atomic(&p, text.as_bytes());
}

fn mode_of(how: UsbHow) -> UsbMode {
    match how {
        UsbHow::Device => UsbMode::Device,
        UsbHow::Disk => UsbMode::Disk,
        UsbHow::Files => UsbMode::Files,
    }
}

#[derive(Clone)]
struct Target {
    dev: UsbDev,
    host: Option<HostDevice>,
    /// Its places in the VM: a port pair, or removable shares (several
    /// volumes of one disk).
    ports: Vec<UsbPort>,
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

/// The menu's note for a connected device.
fn connected_note(ports: &[UsbPort], files: &[String]) -> (String, bool) {
    let ro = if ports.iter().any(|p| p.readonly) { " (read-only)" } else { "" };
    match ports.first().map(|p| p.how) {
        Some(UsbHow::Files) if !files.is_empty() => (format!("Its files are shared with this VM{ro}: {}", files.join(", ")), false),
        Some(UsbHow::Files) => (format!("Its files are shared with this VM{ro}"), false),
        Some(UsbHow::Disk) if ports.iter().all(|p| p.here) => (format!("Connected as a disk{ro}"), false),
        Some(UsbHow::Disk) => ("Connected as a disk, but macOS took it back".into(), true),
        _ if ports.iter().all(|p| p.here) => ("Connected to this VM".into(), false),
        _ => ("Connected, but not plugged into the Mac".into(), true),
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
        self.usb.started = false;
    }

    fn vm_usb(&self) -> Vec<vroom_config::Usb> {
        vroom_config::VmConfig::load_raw(&self.path).map(|c| c.usb).unwrap_or_default()
    }

    /// The VMM's list of what it has plugged in.
    pub(super) fn usb_list(&mut self, l: Vec<UsbPort>) {
        TAKEN.with(|t| t.borrow_mut().insert(self.key.clone(), (self.name.clone(), l.clone())));
        // Remember what this run connected beyond the VM file's devices.
        let always = self.vm_usb();
        let in_file = |d: &UsbDev| always.iter().any(|u| u.id == d.id && !u.serial.as_deref().is_some_and(|s| s != d.serial));
        let mut session: Vec<(UsbDev, UsbMode)> = Vec::new();
        for p in l.iter().filter(|p| !in_file(&p.dev)) {
            if !session.iter().any(|(d, _)| same(d, &p.dev)) {
                session.push((p.dev.clone(), mode_of(p.how)));
            }
        }
        // Not before the resume put them back: an empty first list would
        // forget them.
        if self.usb.resume.is_none() {
            write_session(&self.key, &session);
        }
        // Disks the VM let go of go back to macOS.
        let mut back = Vec::new();
        self.usb.taken.retain_mut(|(k, bsd, _, seen)| {
            let there = l.iter().any(|p| p.how == UsbHow::Disk && key_of(&p.dev) == *k);
            if there {
                *seen = true;
                return true;
            }
            if *seen {
                back.push(bsd.clone());
            }
            !*seen
        });
        back.into_iter().for_each(usbdisk::remount);
        let m = &mut self.usb;
        m.ports = Some(l);
        // It has the sockets it got (or refused them): let ours go.
        m.held.clear();
        m.busy.clear();
        self.win.set_usb_shown(true);
        self.win.set_usb_count(self.usb.ports.as_ref().map_or(0, |p| p.len()) as i32);
        let host = hostusb::host_list();
        let have = self.usb.ports.clone().unwrap_or_default();
        if let Some(want) = self.usb.resume.take() {
            for (d, mode) in want {
                if have.iter().any(|p| same(&p.dev, &d)) {
                    continue;
                }
                match host.iter().find(|h| same(&hostusb::dev_of(h), &d)) {
                    Some(h) => self.usb_connect_as(hostusb::dev_of(h), Some(h.clone()), mode, false),
                    None => self.toast(&format!("{}: not plugged into the Mac, not connected again", if d.name.is_empty() { &d.id } else { &d.name })),
                }
            }
        }
        // The VM file's disks connected as files or as a disk: ours to do.
        if !self.usb.started {
            self.usb.started = true;
            for u in always.iter().filter(|u| u.mode != UsbMode::Device) {
                let d = UsbDev { id: u.id.clone(), serial: u.serial.clone().unwrap_or_default(), name: u.name.clone().unwrap_or_default() };
                if have.iter().any(|p| same(&p.dev, &d)) {
                    continue;
                }
                if let Some(h) = host.iter().find(|h| same(&hostusb::dev_of(h), &d)) {
                    self.usb_connect_as(hostusb::dev_of(h), Some(h.clone()), u.mode, u.readonly);
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
        // A disk it never plugged in goes back to macOS.
        let have = self.usb.ports.clone().unwrap_or_default();
        let mut back = Vec::new();
        self.usb.taken.retain(|(k, bsd, _, seen)| {
            let keep = *seen || have.iter().any(|p| key_of(&p.dev) == *k);
            if !keep {
                back.push(bsd.clone());
            }
            keep
        });
        back.into_iter().for_each(usbdisk::remount);
        if self.win.get_usb_open() {
            self.usb_rows();
        }
    }

    /// The VM is gone: every disk it had goes back to macOS.
    pub(super) fn usb_closed(&mut self) {
        for (_, bsd, _, _) in std::mem::take(&mut self.usb.taken) {
            usbdisk::remount(bsd);
        }
    }

    pub(super) fn usb_tick(&mut self, now: Instant) {
        let m = &mut self.usb;
        m.held.retain(|(t, _)| now.duration_since(*t) < HOLD);
        let n = m.busy.len();
        m.busy.retain(|_, t| now.duration_since(*t) < BUSY_FOR);
        // A disk the VMM never took (it would have said so) goes back.
        let mut back = Vec::new();
        m.taken.retain(|(_, bsd, t, seen)| {
            let keep = *seen || now.duration_since(*t) < BUSY_FOR + Duration::from_secs(5);
            if !keep {
                back.push(bsd.clone());
            }
            keep
        });
        back.into_iter().for_each(usbdisk::remount);
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
                self.usb.choosing = None;
                self.usb.error = None;
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
        // The same by device id, for scripts (VROOM_SHOT_DO):
        // "usb.toggle-id.18a5:0302", "usb.how-id.18a5:0302.files".
        if let Some(rest) = cmd.strip_prefix("usb.toggle-id.").or_else(|| cmd.strip_prefix("usb.how-id.")) {
            let (id, how) = rest.split_once('.').unwrap_or((rest, ""));
            if self.usb.targets.is_empty() {
                self.usb_rows();
            }
            if let Some(i) = self.usb.targets.iter().position(|t| t.dev.id == id) {
                let c = if how.is_empty() { format!("usb.toggle.{i}") } else { format!("usb.how.{i}.{how}") };
                return self.usb_command(&c);
            }
            return true;
        }
        if let Some(i) = cmd.strip_prefix("usb.toggle.").and_then(|i| i.parse::<usize>().ok()) {
            self.usb_toggle(i);
        } else if let Some(i) = cmd.strip_prefix("usb.always.").and_then(|i| i.parse::<usize>().ok()) {
            self.usb_always(i);
        } else if let Some((i, how)) = cmd.strip_prefix("usb.how.").and_then(|r| r.split_once('.')) {
            if let (Ok(i), Some(mode)) = (i.parse::<usize>(), UsbMode::parse(how)) {
                self.usb_how(i, mode);
            }
        } else if cmd == "usb.remember" {
            self.usb.remember = !self.usb.remember;
            self.win.set_usb_remember(self.usb.remember);
        } else if cmd == "usb.setup" {
            self.usb_setup();
        } else {
            return false;
        }
        true
    }

    /// How `dev` connects without asking: the VM file's mode, or the
    /// remembered choice; None: a disk, so ask; Some(Device) otherwise.
    fn known_mode(&self, dev: &UsbDev, host: Option<&HostDevice>) -> Option<UsbMode> {
        let file = self.usb.always.iter().find(|(id, serial, _)| *id == dev.id && serial.as_deref().unwrap_or("") == dev.serial).map(|a| a.2);
        if let Some(m) = file.filter(|m| *m != UsbMode::Device) {
            return Some(m);
        }
        if let Some(m) = hostusb::remembered(dev) {
            return Some(m);
        }
        if host.is_some_and(|h| h.is_storage()) && cfg!(target_os = "macos") {
            return file;
        }
        Some(UsbMode::Device)
    }

    /// The menu's rows, from the Mac's devices and the VM's ports.
    fn usb_rows(&mut self) {
        let now = Instant::now();
        self.usb.host = hostusb::host_list();
        self.usb.listed = Some(now);
        self.usb.always = self.vm_usb().into_iter().map(|u| (u.id, u.serial, u.mode)).collect();
        let ports = self.usb.ports.clone().unwrap_or_default();
        let mut targets: Vec<Target> = self
            .usb
            .host
            .iter()
            .map(|h| {
                let dev = hostusb::dev_of(h);
                let mine = ports.iter().filter(|p| same(&p.dev, &dev)).cloned().collect();
                Target { dev, host: Some(h.clone()), ports: mine }
            })
            .collect();
        // Plugged into the VM but not (or no longer) on the Mac.
        for p in &ports {
            if !targets.iter().any(|t| t.ports.iter().any(|q| q.port == p.port)) {
                match targets.iter_mut().find(|t| t.host.is_none() && same(&t.dev, &p.dev)) {
                    Some(t) => t.ports.push(p.clone()),
                    None => targets.push(Target { dev: p.dev.clone(), host: None, ports: vec![p.clone()] }),
                }
            }
        }
        let elsewhere = |d: &UsbDev| -> Option<String> {
            TAKEN.with(|t| t.borrow().iter().find(|(k, (_, l))| **k != self.key && l.iter().any(|p| same(&p.dev, d))).map(|(_, (n, _))| n.clone()))
        };
        let helper_usb = self.usb.helper.usb();
        let mut needs_helper = false;
        let mut rows = Vec::new();
        for t in &targets {
            let k = key_of(&t.dev);
            let busy = self.usb.busy.contains_key(&k);
            let on = !t.ports.is_empty();
            let root = t.host.as_ref().is_some_and(hostusb::needs_root);
            let other = if on { None } else { elsewhere(&t.dev) };
            let always = self.usb.always.iter().any(|(id, serial, _)| *id == t.dev.id && serial.as_deref().unwrap_or("") == t.dev.serial);
            let mode = self.known_mode(&t.dev, t.host.as_ref());
            let storage = t.host.as_ref().is_some_and(|h| h.is_storage()) && cfg!(target_os = "macos");
            let choosing = !on && !busy && self.usb.choosing.as_deref() == Some(k.as_str());
            let (note, warn) = if busy {
                ((if on { "Letting go..." } else { "Connecting..." }).to_string(), false)
            } else if on {
                let files: Vec<String> =
                    t.host.as_ref().map(|h| usbdisk::volumes(h).into_iter().map(|(_, p)| p.display().to_string()).collect()).unwrap_or_default();
                connected_note(&t.ports, &files)
            } else if let Some(n) = &other {
                (format!("In use by {n}"), true)
            } else if storage && mode == Some(UsbMode::Files) {
                ("A disk: connects its files".into(), false)
            } else if storage && mode == Some(UsbMode::Disk) {
                ("A disk: connects as a disk".into(), false)
            } else if storage && mode.is_none() {
                ("A disk: choose how the VM gets it".into(), false)
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
            rows.push(UsbMenuRow { label: label.into(), note: note.into(), on, always, warn, busy, enabled: other.is_none(), choosing });
        }
        self.usb.targets = targets;
        let (note, setup) = match (&self.usb.helper, needs_helper) {
            (_, false) => (String::new(), false),
            (Helper::Ready(s), true) if !s.usb => (s.usb_why.clone(), false),
            (Helper::Old, true) => ("vroom-netd, vroom's helper that runs as root, is from an older vroom. Set it up again to connect devices macOS uses.".into(), true),
            (_, true) if !cfg!(target_os = "macos") => (String::new(), false),
            (_, true) => (
                "Devices macOS uses need vroom-netd, a small helper that runs as root and takes them from macOS for the VM. It's set up once; macOS asks for an admin password.".into(),
                true,
            ),
        };
        // A disk that couldn't be connected says why first.
        let (note, setup) = match &self.usb.error {
            Some(e) => (e.clone(), false),
            None => (note, setup),
        };
        self.win.set_usb_rows(ModelRc::new(VecModel::from(rows)));
        self.win.set_usb_note(note.into());
        self.win.set_usb_setup(setup);
        self.win.set_usb_setup_busy(self.usb.setup_busy);
        self.win.set_usb_remember(self.usb.remember);
    }

    /// A disk couldn't be connected: say why, in the menu until it closes.
    fn usb_fail(&mut self, why: &str) {
        self.toast("Couldn't connect the disk: the USB menu says why");
        self.usb.error = Some(why.to_string());
        if self.win.get_usb_open() {
            self.usb_rows();
        }
    }

    fn usb_toggle(&mut self, i: usize) {
        let Some(t) = self.usb.targets.get(i).cloned() else { return };
        let k = key_of(&t.dev);
        if self.usb.busy.contains_key(&k) {
            return;
        }
        if !t.ports.is_empty() {
            self.usb.busy.insert(k, Instant::now());
            for p in &t.ports {
                self.io.borrow_mut().send(ToVmm::UsbDetach { port: p.port });
            }
            return self.usb_rows();
        }
        match self.known_mode(&t.dev, t.host.as_ref()) {
            Some(mode) => {
                let ro = self.vm_usb().iter().any(|u| u.id == t.dev.id && u.readonly);
                self.usb_connect_as(t.dev, t.host, mode, ro);
            }
            None => {
                // A disk: show the three ways (a second click hides them).
                let open = self.usb.choosing.as_deref() == Some(k.as_str());
                self.usb.choosing = if open { None } else { Some(k) };
                self.usb.remember = false;
                self.usb_rows();
            }
        }
    }

    /// One of a disk's three ways was picked.
    fn usb_how(&mut self, i: usize, mode: UsbMode) {
        let Some(t) = self.usb.targets.get(i).cloned() else { return };
        self.usb.choosing = None;
        if self.usb.remember {
            if let Err(e) = hostusb::remember(&t.dev, mode).and_then(|_| hostusb::set_mode(&self.path, &t.dev, mode)) {
                self.toast(&format!("Couldn't remember it: {e}"));
            }
        }
        self.usb.remember = false;
        let ro = self.vm_usb().iter().any(|u| u.id == t.dev.id && u.readonly);
        self.usb_connect_as(t.dev, t.host, mode, ro);
    }

    fn usb_connect_as(&mut self, dev: UsbDev, host: Option<HostDevice>, mode: UsbMode, readonly: bool) {
        match (mode, host) {
            (UsbMode::Device, host) => self.usb_connect(dev, host),
            (UsbMode::Files, Some(h)) => self.usb_files(dev, &h, readonly),
            (UsbMode::Disk, Some(h)) => self.usb_disk(dev, h, readonly),
            (_, None) => self.toast(&format!("{}: not plugged into the Mac now", if dev.name.is_empty() { &dev.id } else { &dev.name })),
        }
    }

    /// "Share its files": each of its volumes macOS has mounted, on a
    /// removable share of the VM.
    fn usb_files(&mut self, dev: UsbDev, h: &HostDevice, readonly: bool) {
        let name = if dev.name.is_empty() { dev.id.clone() } else { dev.name.clone() };
        let vols = usbdisk::volumes(h);
        if vols.is_empty() {
            return self.usb_fail(&format!("{name}: macOS has none of its volumes mounted (not formatted, or a format macOS can't read). Use it as a disk instead."));
        }
        for (_, mount) in vols {
            let f = match usbdisk::open_files(&mount, readonly) {
                Ok(f) => f,
                Err(e) => return self.usb_fail(&format!("{name}: {e}")),
            };
            let m = ToVmm::UsbFiles { dev: dev.clone(), readonly: f.readonly, label: f.label.clone(), token: f.token.clone() };
            let sent = match self.io.borrow().tx.as_ref() {
                Some(tx) => vroom_display::send_fd_with(tx, &m.encode(), f.dir.as_raw_fd()).map_err(|e| e.to_string()),
                None => Err("the VM is gone".into()),
            };
            match sent {
                Ok(()) => {
                    log::info!("usb: {} shared with the VM", f.mount.display());
                    self.usb.held.push((Instant::now(), f.dir));
                    self.usb.busy.insert(key_of(&dev), Instant::now());
                }
                Err(e) => return self.toast(&format!("USB: {e}")),
            }
        }
        self.usb_rows();
    }

    /// "Use as a disk": unmount it on the Mac, open it raw (macOS may ask
    /// for a password) off the UI thread, hand the VMM the fd.
    fn usb_disk(&mut self, dev: UsbDev, h: HostDevice, readonly: bool) {
        let k = key_of(&dev);
        self.usb.busy.insert(k, Instant::now());
        self.usb_rows();
        let key = self.key.clone();
        std::thread::spawn(move || {
            let r = usbdisk::open_disk(&h, readonly);
            let _ = slint::invoke_from_event_loop(move || {
                with(&key, |c| c.usb_disk_open(dev, r));
            });
        });
    }

    fn usb_disk_open(&mut self, dev: UsbDev, r: Result<usbdisk::Disk, String>) {
        let name = if dev.name.is_empty() { dev.id.clone() } else { dev.name.clone() };
        let d = match r {
            Ok(d) => d,
            Err(e) => {
                self.usb.busy.remove(&key_of(&dev));
                if e != "cancelled" {
                    self.usb_fail(&format!("{name}: {e}"));
                }
                return self.usb_rows();
            }
        };
        let m = ToVmm::UsbDisk { dev: dev.clone(), readonly: d.readonly };
        let sent = match self.io.borrow().tx.as_ref() {
            Some(tx) => vroom_display::send_fd_with(tx, &m.encode(), d.fd.as_raw_fd()).map_err(|e| e.to_string()),
            None => Err("the VM is gone".into()),
        };
        match sent {
            Ok(()) => {
                log::info!("usb: /dev/{} handed to the VM", d.bsd);
                self.usb.taken.push((key_of(&dev), d.bsd, Instant::now(), false));
                self.usb.held.push((Instant::now(), d.fd));
            }
            Err(e) => {
                self.usb.busy.remove(&key_of(&dev));
                usbdisk::remount(d.bsd);
                self.toast(&format!("USB: {e}"));
            }
        }
        self.usb_rows();
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
            Ok(()) => self.usb.held.push((Instant::now(), std::os::fd::OwnedFd::from(sock))),
            Err(e) => {
                self.usb.busy.remove(&key_of(&dev));
                self.toast(&format!("USB: {e}"));
            }
        }
    }

    /// The settings page put `d` in the VM file: connect it now too, as
    /// the menu does (as its `mode` says). Nothing if it is in already.
    pub(super) fn usb_plug(&mut self, d: UsbDev) {
        if self.usb.ports.is_none() {
            self.toast("This VM has no USB controller now: the device comes at its next start");
            return;
        }
        if self.usb.ports.iter().flatten().any(|p| same(&p.dev, &d)) {
            return;
        }
        let entry = self.vm_usb().into_iter().find(|u| u.id == d.id && u.serial.as_deref().unwrap_or("") == d.serial);
        let (mode, ro) = entry.map_or((UsbMode::Device, false), |u| (u.mode, u.readonly));
        match hostusb::host_list().into_iter().find(|h| same(&hostusb::dev_of(h), &d)) {
            Some(h) => self.usb_connect_as(hostusb::dev_of(&h), Some(h), mode, ro),
            None => self.toast(&format!("{}: not plugged into the Mac now", if d.name.is_empty() { &d.id } else { &d.name })),
        }
    }

    /// The settings page took `id` (`serial`) out of the VM file: let go
    /// of it now.
    pub(super) fn usb_unplug(&mut self, id: &str, serial: Option<&str>) {
        let d = UsbDev { id: id.to_string(), serial: serial.unwrap_or_default().to_string(), name: String::new() };
        let ports: Vec<UsbPort> = self.usb.ports.iter().flatten().filter(|p| same(&p.dev, &d)).cloned().collect();
        for p in ports {
            self.usb.busy.insert(key_of(&p.dev), Instant::now());
            self.io.borrow_mut().send(ToVmm::UsbDetach { port: p.port });
        }
    }

    fn usb_always(&mut self, i: usize) {
        let Some(t) = self.usb.targets.get(i).cloned() else { return };
        let on = !self.usb.always.iter().any(|(id, serial, _)| *id == t.dev.id && serial.as_deref().unwrap_or("") == t.dev.serial);
        // Connected: the way it is connected now. Else what it would be.
        let mode = t.ports.first().map(|p| mode_of(p.how)).or_else(|| self.known_mode(&t.dev, t.host.as_ref())).unwrap_or_default();
        match hostusb::set_always(&self.path, &t.dev, on, mode) {
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
    fn notes_say_how_it_is_connected() {
        let p = |how, here, readonly| UsbPort { port: 64, dev: UsbDev::default(), here, always: false, how, readonly };
        assert_eq!(connected_note(&[p(UsbHow::Files, true, true)], &["/Volumes/test".into()]).0, "Its files are shared with this VM (read-only): /Volumes/test");
        assert_eq!(connected_note(&[p(UsbHow::Disk, true, false)], &[]).0, "Connected as a disk");
        assert!(connected_note(&[p(UsbHow::Disk, false, false)], &[]).1, "a warning");
        assert_eq!(connected_note(&[p(UsbHow::Device, true, false)], &[]).0, "Connected to this VM");
    }

    #[test]
    fn devices_of_a_run_are_kept_for_its_resume() {
        let t = tempfile::tempdir().unwrap();
        crate::paths::TEST_HOME.with(|h| *h.borrow_mut() = Some(t.path().to_path_buf()));
        let d = |id: &str, s: &str, n: &str| UsbDev { id: id.into(), serial: s.into(), name: n.into() };
        let devs = vec![(d("1209:000a", "A1", "dev board"), UsbMode::Device), (d("18a5:0302", "", ""), UsbMode::Files)];
        write_session("k", &devs);
        assert_eq!(read_session("k"), devs);
        // A hand-edited or damaged file: bad lines are skipped; an older
        // file (no mode) means the device.
        std::fs::write(session_file("k"), "1209:000a\tA1\tok\nnot a device\n../../x\t\t\n").unwrap();
        assert_eq!(read_session("k"), vec![(d("1209:000a", "A1", "ok"), UsbMode::Device)]);
        write_session("k", &[]);
        assert!(!session_file("k").exists());
        assert!(read_session("nothing").is_empty());
    }
}
