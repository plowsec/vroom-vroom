//! USB disks for a running VM, without passing the USB device (macOS
//! 15.3 and later keep USB storage from any program without Full Disk
//! Access, root included):
//!
//! - "Share its files": macOS keeps the disk mounted; we open its volume
//!   and hand the VMM that directory's fd and a sandbox token for it
//!   (`ToVmm::UsbFiles`). The VMM serves it on one of its removable
//!   shares; the guest agent mounts it.
//! - "Use as a disk": we unmount it on the Mac, open the raw disk with
//!   macOS's `authopen` (its admin password prompt; Raspberry Pi Imager
//!   and balenaEtcher do the same) and hand the VMM the fd
//!   (`ToVmm::UsbDisk`). The VMM serves it as a plain USB stick. When
//!   the VM lets go of it, macOS mounts it again.
//!
//! Before we open anything we check that the disk is the chosen USB
//! device's (it hangs below that device in the IOKit registry), that it
//! is removable and not internal, and that nothing of it is the system's.
//! After the open, that the fd is that disk (its device number).

use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use vroom_usb::list::{HostDevice, HostDisk};

/// Pretend USB disks for tests (`VROOM_USB_TEST_DISKS=disk6,disk7`):
/// disks `hdiutil attach` made, which no USB device has. They are listed
/// as USB devices 1209:00ac with the disk's name as serial number.
pub const TEST_ID: (u16, u16) = (0x1209, 0x00ac);

/// What the menu says to a person who must allow vroom first.
pub const PRIVACY: &str = "macOS didn't let vroom open the disk. Allow vroom in System Settings, Privacy & Security, Files & Folders (Removable Volumes), then try again, or use \"Share its files\".";

pub fn test_disks() -> Vec<String> {
    std::env::var("VROOM_USB_TEST_DISKS")
        .ok()
        .map(|s| s.split(',').map(str::trim).filter(|d| vroom_usb::list::bsd_disk_ok(d)).map(String::from).collect())
        .unwrap_or_default()
}

/// The pretend devices for `test_disks`.
pub fn test_devices() -> Vec<HostDevice> {
    test_disks()
        .into_iter()
        .map(|bsd| HostDevice {
            vendor: TEST_ID.0,
            product: TEST_ID.1,
            manufacturer: None,
            name: Some(format!("Test disk ({bsd})")),
            serial: Some(bsd),
            class: 0,
            speed: Some(vroom_usb::proto::Speed::High),
            bus: 0,
            ports: vec![0],
            interface_classes: vec![8],
            claimed_by: vec!["IOUSBMassStorageInterfaceNub".into()],
            captured: false,
        })
        .collect()
}

fn is_test(h: &HostDevice) -> bool {
    (h.vendor, h.product) == TEST_ID
}

/// The disks of `h`, from the registry subtree below it.
pub fn disks_of(h: &HostDevice) -> Vec<HostDisk> {
    if is_test(h) {
        let bsd = h.serial.clone().unwrap_or_default();
        return if test_disks().contains(&bsd) { vroom_usb::list::disk_named(&bsd).into_iter().collect() } else { Vec::new() };
    }
    vroom_usb::list::disks(h.vendor, h.product, h.serial.as_deref())
}

/// `h`'s volumes macOS has mounted: (BSD name, where).
pub fn volumes(h: &HostDevice) -> Vec<(String, PathBuf)> {
    let disks = disks_of(h);
    let mut v: Vec<(String, PathBuf)> = vroom_usb::list::mounts().into_iter().filter(|(dev, _)| disks.iter().any(|d| d.parts.contains(dev))).collect();
    v.sort();
    v
}

/// The one disk of `h` a VM may take whole, or why not.
pub fn takeable_disk(h: &HostDevice) -> Result<HostDisk, String> {
    let disks = disks_of(h);
    let d = match disks.as_slice() {
        [] => return Err("macOS shows no disk for it (no card in the reader?)".into()),
        [d] => d.clone(),
        _ => return Err("it has several disks (a card reader?): pass the USB device instead".into()),
    };
    // Disk images (tests) aren't removable to macOS; real sticks must be.
    if !is_test(h) {
        d.takeable().map_err(|e| format!("{}: {e}", d.bsd))?;
    }
    // Never the system's: nothing of it is mounted where macOS lives.
    let system = |p: &Path| p == Path::new("/") || p.starts_with("/System") || p.starts_with("/private/var/vm");
    if vroom_usb::list::mounts().iter().any(|(dev, on)| d.parts.contains(dev) && system(on)) {
        return Err(format!("{} holds macOS itself", d.bsd));
    }
    Ok(d)
}

/// What the VMM gets for "Share its files".
pub struct Files {
    pub dir: OwnedFd,
    /// The volume's name (its mount point's last part), for the guest.
    pub label: String,
    pub token: String,
    pub readonly: bool,
    pub mount: PathBuf,
}

/// Open `h`'s volume at `mount` for the VMM. Read-only if asked, or if
/// macOS has it read-only (NTFS, a locked card).
pub fn open_files(mount: &Path, readonly: bool) -> Result<Files, String> {
    let dir = File::open(mount).map_err(|e| {
        if matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) {
            "macOS didn't let vroom read the disk. Allow vroom in System Settings, Privacy & Security, Files & Folders (Removable Volumes), then try again.".to_string()
        } else {
            format!("{}: {e}", mount.display())
        }
    })?;
    // Reading a directory is what the permission is about: try it now
    // rather than have the guest find an empty share.
    std::fs::read_dir(mount).map_err(|e| format!("{}: {e}", mount.display()))?;
    let readonly = readonly || mounted_readonly(&dir);
    let token = vroom_sandbox::issue_extension(mount, !readonly).map_err(|e| e.to_string())?;
    let name = mount.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let label = if vroom_display::usb_label_ok(&name) && vroom_proto::usb_label_ok(&name) { name } else { "USB".into() };
    Ok(Files { dir: OwnedFd::from(dir), label, token, readonly, mount: mount.to_path_buf() })
}

#[allow(unsafe_code)]
fn mounted_readonly(dir: &File) -> bool {
    // SAFETY: a zeroed statfs is plain data fstatfs fills in.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and out pointer.
    let r = unsafe { libc::fstatfs(dir.as_raw_fd(), &mut s) };
    r == 0 && (s.f_flags as u64) & (libc::MNT_RDONLY as u64) != 0
}

/// Have macOS let go of the disk's volumes. A volume in use refuses.
pub fn unmount(bsd: &str) -> Result<(), String> {
    let o = Command::new("/usr/sbin/diskutil").args(["unmountDisk", &format!("/dev/{bsd}")]).output().map_err(|e| format!("diskutil: {e}"))?;
    if o.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&o.stderr).trim().to_string();
    log::info!("usb: diskutil unmountDisk {bsd}: {why}");
    Err("macOS couldn't unmount it: a program still has files open on it. Close them (or eject it in Finder), then try again.".into())
}

/// Have macOS mount the disk's volumes again (the VM let go of it).
/// Retried for a few seconds: the VMM may still be closing it.
pub fn remount(bsd: String) {
    std::thread::spawn(move || {
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_secs(2));
            match Command::new("/usr/sbin/diskutil").args(["mountDisk", &format!("/dev/{bsd}")]).output() {
                Ok(o) if o.status.success() => {
                    log::info!("usb: {bsd} mounted on the Mac again");
                    return;
                }
                Ok(o) => log::info!("usb: diskutil mountDisk {bsd}: {}", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => log::warn!("usb: diskutil: {e}"),
            }
        }
    });
}

/// What the VMM gets for "Use as a disk".
pub struct Disk {
    pub fd: OwnedFd,
    pub bsd: String,
    pub readonly: bool,
}

/// Unmount `h`'s disk on the Mac and open it raw for the VMM. Blocks
/// (macOS may ask for an admin password): call it off the UI thread.
pub fn open_disk(h: &HostDevice, readonly: bool) -> Result<Disk, String> {
    let d = takeable_disk(h)?;
    unmount(&d.bsd)?;
    let r = open_unmounted(h, &d, readonly);
    if r.is_err() {
        // Not the VM's after all: macOS gets it back.
        remount(d.bsd.clone());
    }
    r
}

fn open_unmounted(h: &HostDevice, d: &HostDisk, readonly: bool) -> Result<Disk, String> {
    // The same disk still, after the unmount (unplugged and another one
    // plugged in would get the next number, but check anyway).
    let again = takeable_disk(h)?;
    if (again.bsd.as_str(), again.major, again.minor) != (d.bsd.as_str(), d.major, d.minor) {
        return Err("the disk changed while it was being unmounted".into());
    }
    let path = format!("/dev/r{}", d.bsd);
    let flags = if readonly { libc::O_RDONLY } else { libc::O_RDWR };
    // Ours already (a disk image): no password. Else authopen asks.
    let fd = match std::fs::OpenOptions::new().read(true).write(!readonly).open(&path) {
        Ok(f) => OwnedFd::from(f),
        Err(e) if matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) => authopen(&path, flags)?,
        Err(e) => return Err(format!("{path}: {e}")),
    };
    check_fd(&fd, d)?;
    Ok(Disk { fd, bsd: d.bsd.clone(), readonly })
}

/// The fd is that disk: a character device with its device number.
fn check_fd(fd: &OwnedFd, d: &HostDisk) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let f = File::from(fd.try_clone().map_err(|e| e.to_string())?);
    let m = f.metadata().map_err(|e| e.to_string())?;
    let want = ((d.major as u64) << 24) | (d.minor as u64 & 0xff_ffff);
    if !m.file_type().is_char_device() || m.rdev() != want {
        return Err(format!("/dev/r{} isn't the disk it should be", d.bsd));
    }
    Ok(())
}

/// Open `path` with macOS's `/usr/libexec/authopen`: it asks for an
/// admin password, opens the file with `flags` and passes us the fd over
/// its standard output (`-stdoutpipe`, SCM_RIGHTS).
fn authopen(path: &str, flags: i32) -> Result<OwnedFd, String> {
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    let child = Command::new("/usr/libexec/authopen")
        .args(["-stdoutpipe", "-o", &flags.to_string(), path])
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("authopen: {e}"))?;
    // Our copy of its end is gone with the Command: the socket ends when
    // authopen does.
    let fd = take_fd(&ours);
    let out = child.wait_with_output().map_err(|e| format!("authopen: {e}"))?;
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    match fd {
        Some(fd) if out.status.success() => Ok(fd),
        _ => Err(authopen_error(out.status.code(), &err)),
    }
}

/// The fd authopen sends, reading its output to the end.
fn take_fd(sock: &UnixStream) -> Option<OwnedFd> {
    let mut fds = Vec::new();
    let mut buf = [0u8; 256];
    while let Ok(n) = vroom_ipc::read_with_fds(sock, &mut buf, &mut fds) {
        if n == 0 {
            break;
        }
    }
    // One is all it sends; more would be odd: keep the first.
    (!fds.is_empty()).then(|| fds.remove(0))
}

/// authopen's failure, in words for the menu.
fn authopen_error(code: Option<i32>, err: &str) -> String {
    let e = err.to_ascii_lowercase();
    if e.contains("not permitted") {
        PRIVACY.into()
    } else if e.contains("cancel") || e.contains("authoriz") || code == Some(1) && err.is_empty() {
        "cancelled".into()
    } else if err.is_empty() {
        format!("authopen failed ({})", code.map_or("killed".into(), |c| c.to_string()))
    } else {
        format!("authopen: {err}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fd_comes_over_the_pipe() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let f = tempfile::tempfile().unwrap();
        // What authopen does: one message with the fd, then it exits.
        vroom_ipc::send_fd_with(&theirs, b"x", f.as_raw_fd()).unwrap();
        drop(theirs);
        assert!(take_fd(&ours).is_some());
        // It refused: nothing comes, the pipe just ends.
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        assert!(take_fd(&ours).is_none());
    }

    #[test]
    fn authopen_errors_in_words() {
        assert_eq!(authopen_error(Some(1), "authopen: open /dev/rdisk5: Operation not permitted"), PRIVACY);
        assert_eq!(authopen_error(Some(1), ""), "cancelled");
        assert!(authopen_error(Some(2), "no such file").contains("no such file"));
    }

    #[test]
    fn only_disks_of_the_device() {
        // A device that is no disk has none; a pretend one only when the
        // test list names it.
        let mut h = test_devices_for("disk999");
        assert!(disks_of(&h).is_empty());
        h.vendor = 0x05ac;
        assert!(takeable_disk(&h).is_err());
    }

    fn test_devices_for(bsd: &str) -> HostDevice {
        HostDevice {
            vendor: TEST_ID.0,
            product: TEST_ID.1,
            manufacturer: None,
            name: None,
            serial: Some(bsd.into()),
            class: 0,
            speed: None,
            bus: 0,
            ports: vec![0],
            interface_classes: vec![8],
            claimed_by: vec![],
            captured: false,
        }
    }

    #[test]
    fn a_volume_opens_with_a_token() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("STICK")).unwrap();
        let f = open_files(&d.path().join("STICK"), true).unwrap();
        assert_eq!(f.label, "STICK");
        assert!(f.readonly && !f.token.is_empty());
        assert!(open_files(&d.path().join("gone"), true).is_err());
    }
}
