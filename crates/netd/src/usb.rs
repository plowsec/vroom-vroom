//! USB devices that a macOS driver holds (keyboards, disks, webcams,
//! serial chips). Taking one from its driver needs root, so the app asks
//! us: we start the usual USB helper (`vroom-netd usb-host`, the same
//! code as `vroom usb-host`) as root, in its Seatbelt sandbox, for that
//! one device, and hand back its socket. The app passes it on to the VMM.
//! Neither ever runs as root.
//!
//! What a caller can do here: name a device by vendor:product (and
//! serial), nothing else. No paths, no bus locations, no arguments. It
//! must be in the configured group (macOS: `admin`, the people who could
//! do this with sudo anyway), the device must be plugged in and not a
//! hub, and the number of helpers and requests is capped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use vroom_net::netd::{usb_id, usb_serial_ok, UsbRequest, VERSION};
use vroom_usb::list::HostDevice;

/// Helpers alive at once, per user and in all.
pub const MAX_PER_USER: usize = 8;
pub const MAX_TOTAL: usize = 16;
/// Requests per user per minute (granted or not).
pub const MAX_PER_MINUTE: usize = 10;

/// One device, as the helper's command line names it.
pub type Device = (u16, u16, Option<String>);

/// Lists the host's USB devices.
pub type Lister = Box<dyn Fn() -> Vec<HostDevice> + Send + Sync>;
/// Is this user in that group?
pub type Member = Box<dyn Fn(u32, &str) -> bool + Send + Sync>;

/// A started helper.
pub trait Running: Send {
    /// Until it has exited.
    fn wait(&mut self);
}

impl Running for std::process::Child {
    fn wait(&mut self) {
        let _ = std::process::Child::wait(self);
    }
}

/// Starts helpers: as root for real; in-process stand-ins in tests.
pub trait Spawn: Send + Sync {
    /// A helper for `dev` that serves on `sock`.
    fn spawn(&self, dev: &Device, sock: UnixStream) -> io::Result<Box<dyn Running>>;
}

#[derive(Default)]
struct Book {
    per_user: HashMap<u32, usize>,
    total: usize,
    live: HashSet<Device>,
    recent: HashMap<u32, VecDeque<Instant>>,
}

pub struct Usb {
    pub enabled: bool,
    pub group: String,
    spawner: Box<dyn Spawn>,
    /// The host's devices now (IOKit / sysfs).
    lister: Lister,
    member: Member,
    book: Mutex<Book>,
}

/// A granted request: the caller's end, and the helper to wait for.
pub struct Grant {
    pub sock: UnixStream,
    pub helper: Box<dyn Running>,
    pub dev: Device,
    pub uid: u32,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant").field("dev", &self.dev).field("uid", &self.uid).finish()
    }
}

impl Usb {
    pub fn new(
        enabled: bool,
        group: String,
        spawner: Box<dyn Spawn>,
        lister: Lister,
        member: Member,
    ) -> Usb {
        Usb { enabled, group, spawner, lister, member, book: Mutex::default() }
    }

    fn book(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// May `uid` ask at all? Err: why not, in words for the app.
    pub fn may(&self, uid: u32) -> Result<(), String> {
        if !self.enabled {
            return Err("USB devices are turned off in /etc/vroom/netd.toml ([usb] enabled)".into());
        }
        if uid != 0 && !(self.member)(uid, &self.group) {
            return Err(format!("only members of the {} group may take USB devices from macOS", self.group));
        }
        Ok(())
    }

    /// Check a request from `uid` and start its helper.
    pub fn grant(&self, uid: u32, req: &UsbRequest) -> Result<Grant, String> {
        {
            let mut b = self.book();
            let now = Instant::now();
            let q = b.recent.entry(uid).or_default();
            while q.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
                q.pop_front();
            }
            if q.len() >= MAX_PER_MINUTE {
                return Err("too many USB requests, wait a minute".into());
            }
            q.push_back(now);
        }
        self.may(uid)?;
        if req.version != VERSION || !crate::valid_name(&req.vm) {
            return Err("invalid request".into());
        }
        let (vendor, product) = usb_id(&req.usb.id).ok_or("invalid USB id")?;
        if req.usb.serial.as_deref().is_some_and(|s| !usb_serial_ok(s)) {
            return Err("invalid serial number".into());
        }
        let dev: Device = (vendor, product, req.usb.serial.clone());
        let here = (self.lister)();
        let found: Vec<&HostDevice> =
            here.iter().filter(|d| d.vendor == vendor && d.product == product && (dev.2.is_none() || d.serial == dev.2)).collect();
        if found.is_empty() {
            return Err(format!("no USB device {} is plugged in", req.usb.id));
        }
        if found.iter().any(|d| d.class == 9) {
            return Err("hubs can't be given to a VM".into());
        }
        {
            let mut b = self.book();
            if b.live.contains(&dev) || (dev.2.is_some() && b.live.contains(&(vendor, product, None))) {
                return Err("that device is in use by a VM already".into());
            }
            if b.total >= MAX_TOTAL || b.per_user.get(&uid).copied().unwrap_or(0) >= MAX_PER_USER {
                return Err("too many USB devices in use".into());
            }
            b.total += 1;
            *b.per_user.entry(uid).or_default() += 1;
            b.live.insert(dev.clone());
        }
        let started = UnixStream::pair().and_then(|(ours, theirs)| Ok((ours, self.spawner.spawn(&dev, theirs)?)));
        match started {
            Ok((sock, helper)) => Ok(Grant { sock, helper, dev, uid }),
            Err(e) => {
                self.release(uid, &dev);
                Err(format!("couldn't start the USB helper: {e}"))
            }
        }
    }

    /// Its helper ended.
    pub fn release(&self, uid: u32, dev: &Device) {
        let mut b = self.book();
        b.total = b.total.saturating_sub(1);
        if let Some(n) = b.per_user.get_mut(&uid) {
            *n = n.saturating_sub(1);
        }
        b.live.remove(dev);
    }
}

/// Is `uid` in group `name` (its primary group or a supplementary one)?
#[allow(unsafe_code)]
pub fn in_group(uid: u32, name: &str) -> bool {
    let Ok(gname) = std::ffi::CString::new(name) else { return false };
    // SAFETY: valid C string; getgrnam returns static storage or NULL.
    let g = unsafe { libc::getgrnam(gname.as_ptr()) };
    if g.is_null() {
        return false;
    }
    // SAFETY: non-null pointer from getgrnam.
    let gid = unsafe { (*g).gr_gid };
    // SAFETY: a zeroed passwd is plain data getpwuid_r fills in.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut out = std::ptr::null_mut();
    // SAFETY: buffers are ours and sized as passed.
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if r != 0 || out.is_null() {
        return false;
    }
    if pw.pw_gid == gid {
        return true;
    }
    let mut groups = vec![0 as libc::gid_t; 256];
    let mut n = groups.len() as libc::c_int;
    // SAFETY: pw_name is a C string in buf; groups has n entries.
    #[cfg(target_os = "macos")]
    let r = unsafe { libc::getgrouplist(pw.pw_name, pw.pw_gid as libc::c_int, groups.as_mut_ptr().cast(), &mut n) };
    // SAFETY: as above.
    #[cfg(not(target_os = "macos"))]
    let r = unsafe { libc::getgrouplist(pw.pw_name, pw.pw_gid, groups.as_mut_ptr(), &mut n) };
    if r < 0 && n as usize > groups.len() {
        return false;
    }
    groups.iter().take(n.max(0) as usize).any(|&g| g == gid)
}

/// Owned by root (or `owner`), and nobody else may write it or any
/// directory above it: so a user can't swap in code a root process runs.
pub fn secure_path(p: &Path, owner: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let mut cur = Some(p);
    while let Some(at) = cur {
        let m = std::fs::symlink_metadata(at).map_err(|e| format!("{}: {e}", at.display()))?;
        if m.file_type().is_symlink() {
            return Err(format!("{} is a symbolic link", at.display()));
        }
        // Sticky directories (/tmp) are no place for root's code either.
        if (m.uid() != 0 && m.uid() != owner) || m.mode() & 0o022 != 0 {
            return Err(format!("{} can be changed by others than root", at.display()));
        }
        cur = at.parent().filter(|p| !p.as_os_str().is_empty());
    }
    Ok(())
}

/// The real thing: `<our exe> usb-host` as root, in its Seatbelt
/// sandbox (macOS), with an empty environment.
pub struct Exec {
    pub exe: PathBuf,
    /// libusb, our own root-owned copy (macOS).
    pub libusb: Option<PathBuf>,
}

impl Spawn for Exec {
    fn spawn(&self, dev: &Device, sock: UnixStream) -> io::Result<Box<dyn Running>> {
        use std::process::{Command, Stdio};
        // SAFETY: geteuid has no preconditions.
        #[allow(unsafe_code)]
        let euid = unsafe { libc::geteuid() };
        secure_path(&self.exe, euid).map_err(io::Error::other)?;
        let mut c;
        #[cfg(target_os = "macos")]
        {
            let lib = self.libusb.as_ref().ok_or_else(|| io::Error::other("no libusb next to vroom-netd: run the setup again"))?;
            secure_path(lib, euid).map_err(io::Error::other)?;
            let prof = vroom_sandbox::usb_profile_root(&self.exe, lib).map_err(|e| io::Error::other(e.to_string()))?;
            c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(prof).arg(&self.exe);
        }
        #[cfg(not(target_os = "macos"))]
        {
            // Landlock and no_new_privs once libusb is loaded.
            c = Command::new(&self.exe);
        }
        c.arg("usb-host").arg("--id").arg(format!("{:04x}:{:04x}", dev.0, dev.1));
        if let Some(s) = &dev.2 {
            c.arg("--serial").arg(s);
        }
        if let Some(l) = &self.libusb {
            c.arg("--libusb").arg(l);
        }
        let child = c
            .env_clear()
            .env("RUST_LOG", "info")
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(sock)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        log::info!("usb: helper {} for {:04x}:{:04x}", child.id(), dev.0, dev.1);
        Ok(Box::new(child))
    }
}

/// `vroom-netd usb-host --id V:P [--serial S] [--libusb PATH]`: the helper
/// itself, serving on stdin.
pub fn host_main(args: &[String]) -> Result<(), String> {
    use std::os::fd::FromRawFd;
    let mut it = args.iter();
    let (mut id, mut serial, mut lib) = (None, None, None);
    while let Some(a) = it.next() {
        let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
        match a.as_str() {
            "--id" => id = Some(v.clone()),
            "--serial" => serial = Some(v.clone()),
            "--libusb" => lib = Some(v.clone()),
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    let (vendor, product) = id.as_deref().and_then(usb_id).ok_or("--id vendor:product")?;
    if let Some(l) = &lib {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        vroom_usb::libusb::only_from(l);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = l;
    }
    // SAFETY: fd 0 is the socket netd set up for us; nobody else in this
    // process uses stdin.
    #[allow(unsafe_code)]
    let sock = unsafe { UnixStream::from_raw_fd(0) };
    let sel = vroom_usb::Selector { vendor, product, serial };
    let lockdown = || -> io::Result<()> {
        // macOS: netd started us under sandbox-exec already.
        #[cfg(target_os = "linux")]
        vroom_sandbox::apply_usb_helper().map_err(io::Error::other)?;
        Ok(())
    };
    vroom_usb::helper_main(sock, sel, lockdown).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vroom_net::netd::UsbWanted;

    /// In-process helpers: the pretend serial port on the socket.
    struct Fake {
        started: Arc<Mutex<Vec<Device>>>,
    }

    struct Thread(Option<std::thread::JoinHandle<()>>);
    impl Running for Thread {
        fn wait(&mut self) {
            if let Some(t) = self.0.take() {
                let _ = t.join();
            }
        }
    }

    impl Spawn for Fake {
        fn spawn(&self, dev: &Device, sock: UnixStream) -> io::Result<Box<dyn Running>> {
            self.started.lock().unwrap().push(dev.clone());
            let (v, p) = (dev.0, dev.1);
            let t = std::thread::spawn(move || {
                let _ = vroom_usb::helper::serve(sock, Box::new(vroom_usb::fake::FakeOpener::new(vroom_usb::fake::FakeKind::Acm, v, p, None)));
            });
            Ok(Box::new(Thread(Some(t))))
        }
    }

    fn dev(vendor: u16, product: u16, class: u8, serial: Option<&str>) -> HostDevice {
        HostDevice {
            vendor,
            product,
            manufacturer: None,
            name: None,
            serial: serial.map(String::from),
            class,
            speed: None,
            bus: 1,
            ports: vec![1],
            interface_classes: vec![],
            claimed_by: vec!["AppleUSBCDC".into()],
        }
    }

    fn usb(enabled: bool) -> (Usb, Arc<Mutex<Vec<Device>>>) {
        let started = Arc::new(Mutex::new(Vec::new()));
        let u = Usb::new(
            enabled,
            "admin".into(),
            Box::new(Fake { started: started.clone() }),
            Box::new(|| vec![dev(0x1209, 0x000a, 0, Some("A1")), dev(0x1209, 0x000b, 0, None), dev(0x05e3, 0x0610, 9, None)]),
            Box::new(|uid, g| g == "admin" && uid == 501),
        );
        (u, started)
    }

    fn req(id: &str, serial: Option<&str>) -> UsbRequest {
        UsbRequest { version: VERSION, vm: "dev".into(), usb: UsbWanted { id: id.into(), serial: serial.map(String::from) } }
    }

    #[test]
    fn who_may() {
        let (u, _) = usb(true);
        assert!(u.may(501).is_ok());
        assert!(u.may(0).is_ok(), "root");
        assert!(u.may(502).unwrap_err().contains("admin"));
        let (off, _) = usb(false);
        assert!(off.may(501).unwrap_err().contains("turned off"));
        assert!(off.grant(501, &req("1209:000a", None)).is_err());
    }

    #[test]
    fn requests_are_checked() {
        let (u, started) = usb(true);
        assert!(u.grant(502, &req("1209:000a", None)).is_err(), "not in the group");
        assert!(u.grant(501, &req("1209:0a", None)).is_err());
        assert!(u.grant(501, &req("1209:000a", Some("a\nb"))).is_err());
        assert!(u.grant(501, &UsbRequest { vm: "../x".into(), ..req("1209:000a", None) }).is_err());
        assert!(u.grant(501, &req("1209:000a", Some("B2"))).unwrap_err().contains("plugged in"), "another serial");
        assert!(u.grant(501, &req("dead:beef", None)).unwrap_err().contains("plugged in"));
        assert!(u.grant(501, &req("05e3:0610", None)).unwrap_err().contains("hub"));
        assert!(started.lock().unwrap().is_empty(), "nothing started for any of these");
    }

    #[test]
    fn a_grant_serves_that_device_and_only_once() {
        let (u, started) = usb(true);
        let mut g = u.grant(501, &req("1209:000a", Some("A1"))).unwrap();
        assert_eq!(started.lock().unwrap().as_slice(), &[(0x1209, 0x000a, Some("A1".to_string()))]);
        // It is a helper: it says the device is there.
        let mut r = std::io::BufReader::new(g.sock.try_clone().unwrap());
        let ev = vroom_usb::proto::read_event(&mut r).unwrap().unwrap();
        assert!(matches!(ev, vroom_usb::proto::Event::Attached { vendor: 0x1209, product: 0x000a, .. }), "{ev:?}");
        // Taken: not twice, not by id alone either.
        assert!(u.grant(501, &req("1209:000a", Some("A1"))).unwrap_err().contains("in use"));
        // Its socket closes: the helper ends, the device is free again.
        drop(r);
        g.sock.shutdown(std::net::Shutdown::Both).unwrap();
        g.helper.wait();
        u.release(g.uid, &g.dev);
        assert!(u.grant(501, &req("1209:000a", Some("A1"))).is_ok());
    }

    #[test]
    fn counts_and_rates_are_capped() {
        let (u, _) = usb(true);
        let mut held = Vec::new();
        for _ in 0..MAX_PER_MINUTE {
            match u.grant(501, &req("1209:000b", None)) {
                Ok(g) => held.push(g),
                Err(e) => assert!(e.contains("in use"), "{e}"),
            }
        }
        assert_eq!(held.len(), 1);
        assert!(u.grant(501, &req("1209:000b", None)).unwrap_err().contains("wait a minute"));
        // Per user and in all, whatever the rate.
        let (u, _) = usb(true);
        u.book().total = MAX_TOTAL;
        assert!(u.grant(501, &req("1209:000a", None)).unwrap_err().contains("too many USB devices"));
    }

    #[test]
    fn root_code_must_be_roots() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("lib.dylib");
        std::fs::write(&f, b"x").unwrap();
        // SAFETY: getuid has no preconditions.
        #[allow(unsafe_code)]
        let me = unsafe { libc::getuid() };
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        // The temp dir's parents may be anyone's on some machines; check
        // what we control.
        if secure_path(dir.path(), me).is_ok() {
            assert!(secure_path(&f, me).is_ok());
        }
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(secure_path(&f, me).is_err(), "anyone can write it");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let l = dir.path().join("link");
        std::os::unix::fs::symlink(&f, &l).unwrap();
        assert!(secure_path(&l, me).is_err(), "a link");
        assert!(secure_path(&f, u32::MAX - 1).is_err() || me == 0, "someone else's");
    }

    #[test]
    fn group_lookup() {
        // SAFETY: getuid has no preconditions.
        #[allow(unsafe_code)]
        let me = unsafe { libc::getuid() };
        assert!(!in_group(me, "no-such-group-vroom"));
        // root is in wheel (macOS) / root (Linux) by its primary group.
        assert!(in_group(0, if cfg!(target_os = "macos") { "wheel" } else { "root" }));
    }
}
