//! Host USB devices for a running VM: start the helper that serves one
//! (`vroom usb-host`, in its own sandbox, as us), or have vroom-netd start
//! it as root when a macOS driver holds the device. Either way we get the
//! helper's socket and hand it to the VMM (`ToVmm::UsbAttach`); the VMM
//! never starts anything, and nothing here opens the device itself.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use vroom_config::{UsbMode, VmConfig};
use vroom_display::UsbDev;
use vroom_usb::list::HostDevice;

/// The Mac's USB devices, hubs left out. With `VROOM_USB_FAKE` (test
/// mode: every helper serves a pretend device) a pretend device 1209:00aa
/// is listed too, so the menu can be tried without hardware.
pub fn host_list() -> Vec<HostDevice> {
    let mut v: Vec<HostDevice> = vroom_usb::list::list().into_iter().filter(|d| d.class != 9).collect();
    v.extend(crate::usbdisk::test_devices());
    let fake = std::env::var("VROOM_USB_FAKE").ok();
    if let Some(name) = match fake.as_deref() {
        Some("acm") => Some("Pretend serial port (test)"),
        Some("audio") => Some("Pretend sound card (test)"),
        Some("disk" | "disk-broken") => Some("Pretend USB stick (test)"),
        _ => None,
    } {
        v.push(HostDevice {
            vendor: 0x1209,
            product: 0x00aa,
            manufacturer: None,
            name: Some(name.into()),
            serial: None,
            class: 2,
            speed: Some(vroom_usb::proto::Speed::Full),
            bus: 0,
            ports: vec![0],
            interface_classes: vec![2, 10],
            claimed_by: Vec::new(),
            captured: false,
        });
    }
    v
}

/// How the app names a host device to the VMM (and in the VM file).
pub fn dev_of(d: &HostDevice) -> UsbDev {
    let text = |ch: char| (' '..='~').contains(&ch);
    let serial = d.serial.clone().filter(|s| vroom_net::netd::usb_serial_ok(s)).unwrap_or_default();
    UsbDev { id: d.id(), serial, name: d.label().chars().filter(|&c| text(c)).take(64).collect::<String>().trim().to_string() }
}

/// A macOS driver holds it, or an earlier root helper captured it:
/// taking it needs root (vroom-netd). On Linux the helper detaches kernel
/// drivers itself (with access to the device).
pub fn needs_root(d: &HostDevice) -> bool {
    d.needs_root()
}

/// Start `vroom usb-host` for `d`, as we do. Returns it and its socket.
pub fn spawn_local(d: &UsbDev) -> Result<(Child, UnixStream), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("vroom");
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    let mut c = command(&exe)?;
    c.arg("usb-host").arg("--id").arg(&d.id);
    if !d.serial.is_empty() {
        c.arg("--serial").arg(&d.serial);
    }
    let child = c
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't start the USB helper: {e}"))?;
    Ok((child, ours))
}

fn command(exe: &Path) -> Result<Command, String> {
    #[cfg(target_os = "macos")]
    {
        let prof = vroom_sandbox::usb_profile(exe).map_err(|e| e.to_string())?;
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(prof).arg(exe);
        Ok(c)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // It sandboxes itself once libusb is loaded.
        Ok(Command::new(exe))
    }
}

/// Have vroom-netd start the helper as root, for a device a macOS driver
/// holds. Blocks for a moment (a local socket).
pub fn via_netd(vm: &str, d: &UsbDev) -> Result<UnixStream, String> {
    let vm: String = vm.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).take(64).collect();
    let req = vroom_net::netd::UsbRequest {
        version: vroom_net::netd::VERSION,
        vm: if vm.is_empty() { "vm".into() } else { vm },
        usb: vroom_net::netd::UsbWanted { id: d.id.clone(), serial: (!d.serial.is_empty()).then(|| d.serial.clone()) },
    };
    vroom_net::netd::usb(Path::new(vroom_net::netd::DEFAULT_SOCKET), &req).map_err(|e| match e {
        vroom_net::netd::NetdError::Refused(m) => m,
        e => e.to_string(),
    })
}

/// "Always connect to this VM": add the device to the VM file's
/// `[[usb]]`, or take it out. Comments and layout stay.
/// `mode`: how it connects (a USB disk's files, the disk, the device).
pub fn set_always(path: &Path, d: &UsbDev, on: bool, mode: UsbMode) -> Result<(), String> {
    edit_file(path, |c| apply_always(c, d, on, mode))
}

/// A remembered choice: the VM file's entry for `d`, if it has one,
/// connects it that way from now on.
pub fn set_mode(path: &Path, d: &UsbDev, mode: UsbMode) -> Result<(), String> {
    edit_file(path, |c| {
        let serial = (!d.serial.is_empty()).then_some(d.serial.as_str());
        for u in c.usb.iter_mut().filter(|u| u.id == d.id && u.serial.as_deref().is_none_or(|s| Some(s) == serial)) {
            u.mode = mode;
        }
        Ok(())
    })
}

fn edit_file(path: &Path, f: impl FnOnce(&mut VmConfig) -> Result<(), String>) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let before = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let mut after = before.clone();
    f(&mut after)?;
    after.validate().map_err(|e| e.to_string())?;
    if after == before {
        return Ok(());
    }
    crate::edit::save(path, &before, &after)
}

pub(crate) fn apply_always(c: &mut VmConfig, d: &UsbDev, on: bool, mode: UsbMode) -> Result<(), String> {
    let serial = (!d.serial.is_empty()).then_some(d.serial.as_str());
    let same = |u: &vroom_config::Usb| u.id == d.id && u.serial.as_deref() == serial;
    if on {
        if !c.usb.iter().any(same) {
            crate::edit::add_usb(c, &d.id, serial, (!d.name.is_empty()).then_some(d.name.as_str()))?;
        }
        if let Some(u) = c.usb.iter_mut().find(|u| same(u)) {
            u.mode = mode;
        }
    } else {
        c.usb.retain(|u| !same(u));
    }
    Ok(())
}

/// USB disks seen before, by id: one a root helper captured earlier
/// shows macOS no interfaces, so it no longer looks like a disk.
fn disks_file() -> std::path::PathBuf {
    crate::paths::data_dir().join("usb-disks")
}

/// A USB disk (it can go as its files or a whole disk): macOS says so
/// now, or did before. macOS only.
pub fn is_disk(h: &HostDevice) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let id = h.id();
    let known = std::fs::read_to_string(disks_file()).unwrap_or_default();
    let seen = known.lines().any(|l| l == id);
    if h.is_storage() && !seen {
        let mut lines: Vec<&str> = known.lines().rev().take(255).collect();
        lines.reverse();
        lines.push(&id);
        if let Some(dir) = disks_file().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::library::write_atomic(&disks_file(), (lines.join("\n") + "\n").as_bytes());
    }
    h.is_storage() || seen
}

/// Choices remembered for a device ("Remember for this device"), for
/// every VM: one line each, id, serial and mode, tab-separated.
fn modes_file() -> std::path::PathBuf {
    crate::paths::data_dir().join("usb-modes")
}

pub fn remembered(d: &UsbDev) -> Option<UsbMode> {
    let s = std::fs::read_to_string(modes_file()).ok()?;
    s.lines().find_map(|l| {
        let mut f = l.splitn(3, '\t');
        let (id, serial, mode) = (f.next()?, f.next()?, f.next()?);
        (id == d.id && serial == d.serial).then(|| UsbMode::parse(mode)).flatten()
    })
}

pub fn remember(d: &UsbDev, mode: UsbMode) -> Result<(), String> {
    let p = modes_file();
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    let mut lines: Vec<String> = old
        .lines()
        .filter(|l| {
            let mut f = l.splitn(3, '\t');
            !(f.next() == Some(d.id.as_str()) && f.next() == Some(d.serial.as_str()))
        })
        .take(256)
        .map(String::from)
        .collect();
    lines.push(format!("{}\t{}\t{}", d.id, d.serial, mode.as_str()));
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    crate::library::write_atomic(&p, (lines.join("\n") + "\n").as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(serial: Option<&str>, claimed: bool) -> HostDevice {
        HostDevice {
            vendor: 0x18a5,
            product: 0x0302,
            manufacturer: Some("Verbatim".into()),
            name: Some("STORE N GO\u{7}".into()),
            serial: serial.map(String::from),
            class: 0,
            speed: None,
            bus: 2,
            ports: vec![1, 3],
            interface_classes: vec![8],
            claimed_by: if claimed { vec!["IOUSBMassStorageInterfaceNub".into()] } else { vec![] },
            captured: false,
        }
    }

    #[test]
    fn names_for_the_vmm() {
        let d = dev_of(&host(Some("0703896E1A305748"), true));
        assert_eq!((d.id.as_str(), d.serial.as_str(), d.name.as_str()), ("18a5:0302", "0703896E1A305748", "Verbatim STORE N GO"));
        assert!(d.valid());
        assert_eq!(dev_of(&host(Some("bad\nserial"), false)).serial, "", "not plain text: by id");
        assert_eq!(needs_root(&host(None, true)), cfg!(target_os = "macos"));
        assert!(!needs_root(&host(None, false)));
    }

    #[test]
    fn choices_are_remembered_per_device() {
        let t = tempfile::tempdir().unwrap();
        crate::paths::TEST_HOME.with(|h| *h.borrow_mut() = Some(t.path().to_path_buf()));
        let a = dev_of(&host(Some("A1"), true));
        let b = dev_of(&host(Some("B2"), true));
        assert_eq!(remembered(&a), None);
        remember(&a, UsbMode::Files).unwrap();
        remember(&b, UsbMode::Disk).unwrap();
        remember(&a, UsbMode::Disk).unwrap();
        assert_eq!((remembered(&a), remembered(&b)), (Some(UsbMode::Disk), Some(UsbMode::Disk)));
        assert_eq!(std::fs::read_to_string(modes_file()).unwrap().lines().count(), 2, "once per device");
    }

    #[test]
    fn always_goes_in_the_file_and_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vm.toml");
        std::fs::write(&p, "# my vm\nname = \"vm\"\n\n[boot]\ntype = \"uefi\"\nvars = \"vm.state/efivars.fd\"\n").unwrap();
        let d = dev_of(&host(Some("A1"), false));
        set_always(&p, &d, true, UsbMode::Device).unwrap();
        set_always(&p, &d, true, UsbMode::Device).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.starts_with("# my vm\n"), "{t}");
        let c = VmConfig::from_toml(&t).unwrap();
        assert_eq!(c.usb.len(), 1, "once");
        assert_eq!((c.usb[0].id.as_str(), c.usb[0].serial.as_deref()), ("18a5:0302", Some("A1")));
        set_always(&p, &d, false, UsbMode::Device).unwrap();
        // A disk shared as files: the mode goes in with it, and a
        // remembered choice changes it.
        set_always(&p, &d, true, UsbMode::Files).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("mode = \"files\""), "{t}");
        set_mode(&p, &d, UsbMode::Disk).unwrap();
        assert_eq!(VmConfig::from_toml(&std::fs::read_to_string(&p).unwrap()).unwrap().usb[0].mode, UsbMode::Disk);
        set_always(&p, &d, false, UsbMode::Device).unwrap();
        assert!(VmConfig::from_toml(&std::fs::read_to_string(&p).unwrap()).unwrap().usb.is_empty());
    }
}
