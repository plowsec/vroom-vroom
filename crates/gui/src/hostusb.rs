//! Host USB devices for a running VM: start the helper that serves one
//! (`vroom usb-host`, in its own sandbox, as us), or have vroom-netd start
//! it as root when a macOS driver holds the device. Either way we get the
//! helper's socket and hand it to the VMM (`ToVmm::UsbAttach`); the VMM
//! never starts anything, and nothing here opens the device itself.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use vroom_config::VmConfig;
use vroom_display::UsbDev;
use vroom_usb::list::HostDevice;

/// The Mac's USB devices, hubs left out. With `VROOM_USB_FAKE=acm` (test
/// mode: every helper serves a pretend serial port) a pretend device
/// 1209:00aa is listed too, so the menu can be tried without hardware.
pub fn host_list() -> Vec<HostDevice> {
    let mut v: Vec<HostDevice> = vroom_usb::list::list().into_iter().filter(|d| d.class != 9).collect();
    if std::env::var("VROOM_USB_FAKE").as_deref() == Ok("acm") {
        v.push(HostDevice {
            vendor: 0x1209,
            product: 0x00aa,
            manufacturer: None,
            name: Some("Pretend serial port (test)".into()),
            serial: None,
            class: 2,
            speed: Some(vroom_usb::proto::Speed::Full),
            bus: 0,
            ports: vec![0],
            interface_classes: vec![2, 10],
            claimed_by: Vec::new(),
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

/// A macOS driver holds it: taking it needs root (vroom-netd). On Linux
/// the helper detaches kernel drivers itself (with access to the device).
pub fn needs_root(d: &HostDevice) -> bool {
    cfg!(target_os = "macos") && !d.claimed_by.is_empty()
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
pub fn set_always(path: &Path, d: &UsbDev, on: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let before = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let mut after = before.clone();
    apply_always(&mut after, d, on)?;
    after.validate().map_err(|e| e.to_string())?;
    if after == before {
        return Ok(());
    }
    crate::edit::save(path, &before, &after)
}

fn apply_always(c: &mut VmConfig, d: &UsbDev, on: bool) -> Result<(), String> {
    let serial = (!d.serial.is_empty()).then_some(d.serial.as_str());
    let same = |u: &vroom_config::Usb| u.id == d.id && u.serial.as_deref() == serial;
    if on {
        if !c.usb.iter().any(same) {
            crate::edit::add_usb(c, &d.id, serial, (!d.name.is_empty()).then_some(d.name.as_str()))?;
        }
    } else {
        c.usb.retain(|u| !same(u));
    }
    Ok(())
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
    fn always_goes_in_the_file_and_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vm.toml");
        std::fs::write(&p, "# my vm\nname = \"vm\"\n\n[boot]\ntype = \"uefi\"\nvars = \"vm.state/efivars.fd\"\n").unwrap();
        let d = dev_of(&host(Some("A1"), false));
        set_always(&p, &d, true).unwrap();
        set_always(&p, &d, true).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.starts_with("# my vm\n"), "{t}");
        let c = VmConfig::from_toml(&t).unwrap();
        assert_eq!(c.usb.len(), 1, "once");
        assert_eq!((c.usb[0].id.as_str(), c.usb[0].serial.as_deref()), ("18a5:0302", Some("A1")));
        set_always(&p, &d, false).unwrap();
        assert!(VmConfig::from_toml(&std::fs::read_to_string(&p).unwrap()).unwrap().usb.is_empty());
    }
}
