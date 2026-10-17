//! Windows: shared folders as drive letters.
//!
//! Each `[[share]]` is a virtio-fs device. virtio-win's virtiofs.exe
//! (on WinFsp) mounts one device per process, picked by its tag. So for
//! every viofs device we make one service, `VroomShare_<tag>`, running
//! `virtiofs.exe -t <tag> -m X:` with the first free letter from Z: down.
//! A service that already exists is only started: change its mount point
//! with `sc config` and it stays. Runs from `vroom-setup.cmd`
//! (`--mount-shares`) and when the agent service starts, so shares added
//! later show up after a reboot, and services of shares that are gone
//! (no device with that tag) are removed.

/// Service name prefix; the tag follows.
pub const PREFIX: &str = "VroomShare_";

/// Tags as the VM config allows them: 1-36 of `[A-Za-z0-9._-]`. Anything
/// else (from a device we don't know) is skipped: it ends up on a
/// command line.
pub fn valid_tag(t: &str) -> bool {
    !t.is_empty() && t.len() <= 36 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The drive letter a virtiofs.exe command line mounts (`-m X:`).
pub fn letter_of(cmd: &str) -> Option<char> {
    let mut it = cmd.split_whitespace();
    while let Some(a) = it.next() {
        if a == "-m" {
            let m = it.next()?.trim_matches('"').as_bytes();
            return match m {
                [l, b':'] | [l, b':', b'\\'] if l.is_ascii_alphabetic() => Some(l.to_ascii_uppercase() as char),
                _ => None,
            };
        }
    }
    None
}

/// First free letter from Z: down to D:. `used`: bit 0 = A:.
pub fn pick_letter(used: u32) -> Option<char> {
    (3..26u32).rev().find(|i| used & (1 << i) == 0).map(|i| (b'A' + i as u8) as char)
}

/// Our services (`names`) whose share is gone: no device has their tag.
pub fn stale<'a>(names: &'a [String], tags: &[String]) -> Vec<&'a str> {
    names
        .iter()
        .filter(|n| n.strip_prefix(PREFIX).is_some_and(|t| !tags.iter().any(|x| x == t)))
        .map(|n| n.as_str())
        .collect()
}

pub fn command_line(exe: &str, tag: &str, letter: char) -> String {
    format!("\"{exe}\" -t {tag} -m {letter}:")
}

/// A removable share a USB disk's files come through: its service is
/// started when the host says so, not at boot.
pub fn usb_tag(t: &str) -> bool {
    vroom_proto::usb_tag_ok(t)
}

/// Where a Linux guest shows a USB disk's files.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const MEDIA: &str = "/media/vroom";

/// The mount point for volume `label`: `/media/vroom/<label>`, or
/// `<label> 2`, `<label> 3`... when two volumes share a name.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn mount_point(label: &str, taken: &[String]) -> String {
    let mut p = format!("{MEDIA}/{label}");
    let mut n = 2;
    while taken.contains(&p) {
        p = format!("{MEDIA}/{label} {n}");
        n += 1;
    }
    p
}

/// A USB disk's files came to share `tag`, or left it.
pub fn usb_share(tag: &str, label: &str, on: bool) {
    if !usb_tag(tag) || !vroom_proto::usb_label_ok(label) {
        return;
    }
    #[cfg(any(windows, target_os = "linux"))]
    os_usb::share(tag, label, on);
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = on;
}

/// A new host: what the last one shared is gone.
pub fn usb_reset() {
    #[cfg(any(windows, target_os = "linux"))]
    os_usb::reset();
}

/// Linux: systemd mounts them for us (`systemd-run`, over D-Bus), so
/// the agent's service needs no mount privilege of its own; GNOME's
/// file manager lists them (`x-gvfs-show`).
#[cfg(target_os = "linux")]
mod os_usb {
    use std::process::Command;
    use std::sync::Mutex;

    /// (tag, mount point) of what we mounted.
    static MOUNTED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    /// Run `cmd` as a short-lived systemd service: in the system's own
    /// mount namespace, with the privileges mounting takes, which our
    /// own hardened service doesn't have. No shell: the arguments are
    /// passed as they are.
    fn run(cmd: &[&str]) -> bool {
        match Command::new("systemd-run").args(["--wait", "--collect", "--quiet", "--"]).args(cmd).output() {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                log::warn!("usb: {cmd:?}: {}", String::from_utf8_lossy(&o.stderr).trim());
                false
            }
            Err(e) => {
                log::warn!("usb: systemd-run: {e}");
                false
            }
        }
    }

    fn unmount(dir: &str) {
        // Lazily: a shell or a file manager in it doesn't keep it.
        if run(&["/usr/bin/umount", "-l", dir]) {
            run(&["/usr/bin/rmdir", dir]);
        }
    }

    pub fn share(tag: &str, label: &str, on: bool) {
        let mut m = MOUNTED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = m.iter().position(|(t, _)| t == tag) {
            let (_, dir) = m.remove(i);
            unmount(&dir);
            log::info!("usb: {tag} unmounted from {dir}");
        }
        if !on {
            return;
        }
        let taken: Vec<String> = m.iter().map(|(_, d)| d.clone()).collect();
        let dir = super::mount_point(label, &taken);
        if run(&["/usr/bin/mkdir", "-p", &dir]) && run(&["/usr/bin/mount", "-t", "virtiofs", "-o", "x-gvfs-show", tag, &dir]) {
            log::info!("usb: {tag} ({label}) on {dir}");
            m.push((tag.to_string(), dir));
        }
    }

    /// Unmount every USB disk share there is (a host that restarted, or
    /// a VM resumed: those shares are empty now).
    pub fn reset() {
        MOUNTED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        for l in mounts.lines() {
            let f: Vec<&str> = l.split(' ').collect();
            if f.len() > 2 && f[2] == "virtiofs" && super::usb_tag(f[0]) {
                // /proc/mounts writes a space as \040.
                let dir = f[1].replace("\\040", " ");
                unmount(&dir);
            }
        }
    }
}

#[cfg(windows)]
pub use os::mount_all;

/// Windows: the share's service mounts it on a drive letter; it is
/// made at boot (`mount_all`) but started only while a disk is there.
#[cfg(windows)]
mod os_usb {
    pub fn share(tag: &str, _label: &str, on: bool) {
        if let Err(e) = super::os::usb_service(tag, on) {
            log::warn!("usb: {tag}: {e}");
        }
    }

    pub fn reset() {
        for i in 0..10 {
            let _ = super::os::usb_service(&format!("vroom-usb{i}"), false);
        }
    }
}

#[cfg(windows)]
mod os {
    use std::io;

    use windows_sys::core::GUID;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SERVICE_ALREADY_RUNNING, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, GetLogicalDrives, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Services::{
        ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService, EnumServicesStatusExW, OpenSCManagerW, OpenServiceW,
        QueryServiceConfigW, StartServiceW, ENUM_SERVICE_STATUS_PROCESSW, QUERY_SERVICE_CONFIGW, SC_ENUM_PROCESS_INFO, SC_HANDLE,
        SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL, SERVICE_NO_CHANGE, SERVICE_STATE_ALL,
        SERVICE_STATUS, SERVICE_WIN32, SERVICE_WIN32_OWN_PROCESS,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::*;

    /// virtio-win's viofs device interface.
    const GUID_DEVINTERFACE_VIRT_FS: GUID = GUID::from_u128(0xa1b0cbf2_f488_4b29_b89f_16c78e68c737);
    /// CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_READ_ACCESS)
    const IOCTL_VIRTFS_GET_VOLUME_NAME: u32 = (0x22 << 16) | (1 << 14) | (0x800 << 2);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    struct Sc(SC_HANDLE);
    impl Drop for Sc {
        fn drop(&mut self) {
            // SAFETY: a handle we opened, closed once.
            unsafe { CloseServiceHandle(self.0) };
        }
    }

    /// Device paths of the viofs devices present.
    fn devices() -> io::Result<Vec<String>> {
        let mut len = 0u32;
        // SAFETY: valid GUID and out pointer.
        let r = unsafe { CM_Get_Device_Interface_List_SizeW(&mut len, &GUID_DEVINTERFACE_VIRT_FS, std::ptr::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) };
        if r != CR_SUCCESS {
            return Err(io::Error::other(format!("CM_Get_Device_Interface_List_Size: {r}")));
        }
        let mut buf = vec![0u16; len.clamp(1, 1 << 20) as usize];
        // SAFETY: buf is writable for its length.
        let r = unsafe {
            CM_Get_Device_Interface_ListW(&GUID_DEVINTERFACE_VIRT_FS, std::ptr::null(), buf.as_mut_ptr(), buf.len() as u32, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
        };
        if r != CR_SUCCESS {
            return Err(io::Error::other(format!("CM_Get_Device_Interface_List: {r}")));
        }
        Ok(buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect())
    }

    /// The tag of a viofs device, as its driver reports it.
    fn tag(path: &str) -> io::Result<String> {
        let p = wide(path);
        // SAFETY: valid path; plain flags.
        let h = unsafe { CreateFileW(p.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut name = [0u16; 64];
        let mut got = 0u32;
        // SAFETY: valid handle; out buffer of the size given.
        let ok = unsafe {
            DeviceIoControl(h, IOCTL_VIRTFS_GET_VOLUME_NAME, std::ptr::null(), 0, name.as_mut_ptr().cast(), (name.len() * 2) as u32, &mut got, std::ptr::null_mut())
        };
        let err = io::Error::last_os_error();
        // SAFETY: opened above.
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return Err(err);
        }
        let n = ((got / 2) as usize).min(name.len());
        let end = name[..n].iter().position(|&c| c == 0).unwrap_or(n);
        Ok(String::from_utf16_lossy(&name[..end]))
    }

    /// The command line of an installed service, if there is one.
    fn command_of(svc: &Sc) -> Option<String> {
        let mut need = 0u32;
        // SAFETY: size query with no buffer.
        unsafe { QueryServiceConfigW(svc.0, std::ptr::null_mut(), 0, &mut need) };
        let mut buf = vec![0u64; (need as usize).div_ceil(8).clamp(8, 1 << 16)];
        // SAFETY: buf is 8-aligned and writable for its byte length.
        if unsafe { QueryServiceConfigW(svc.0, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut need) } == 0 {
            return None;
        }
        // SAFETY: the call filled in the struct at the start of buf.
        let cfg = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
        if cfg.lpBinaryPathName.is_null() {
            return None;
        }
        // SAFETY: NUL-terminated string inside buf.
        let s = unsafe { std::slice::from_raw_parts(cfg.lpBinaryPathName, (0..).take_while(|&i| *cfg.lpBinaryPathName.add(i) != 0).count()) };
        Some(String::from_utf16_lossy(s))
    }

    /// Names of all services starting with PREFIX.
    fn ours(scm: &Sc) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let mut resume = 0u32;
        let mut buf = vec![0u64; 8192];
        loop {
            let (mut need, mut n) = (0u32, 0u32);
            // SAFETY: valid SCM handle; buf is 8-aligned and writable for
            // its byte length.
            let ok = unsafe {
                EnumServicesStatusExW(
                    scm.0,
                    SC_ENUM_PROCESS_INFO,
                    SERVICE_WIN32,
                    SERVICE_STATE_ALL,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                    &mut need,
                    &mut n,
                    &mut resume,
                    std::ptr::null(),
                )
            };
            let more = ok == 0 && io::Error::last_os_error().raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_MORE_DATA as i32);
            if ok == 0 && !more {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the call wrote n entries at the start of buf.
            let entries = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, n as usize) };
            for e in entries {
                // SAFETY: NUL-terminated string inside buf.
                let name = unsafe { std::slice::from_raw_parts(e.lpServiceName, (0..).take_while(|&i| *e.lpServiceName.add(i) != 0).count()) };
                let name = String::from_utf16_lossy(name);
                if name.starts_with(PREFIX) {
                    out.push(name);
                }
            }
            if !more {
                return Ok(out);
            }
            if n == 0 {
                buf.resize((need as usize).div_ceil(8).clamp(buf.len() + 1, 1 << 20), 0);
            }
        }
    }

    /// Stop and delete a service.
    fn remove(scm: &Sc, name: &str) {
        let w = wide(name);
        // SAFETY: valid SCM handle and name.
        let h = unsafe { OpenServiceW(scm.0, w.as_ptr(), SERVICE_ALL_ACCESS) };
        if h.is_null() {
            return;
        }
        let svc = Sc(h);
        // SAFETY: zeroed SERVICE_STATUS is a valid out value.
        let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle and out pointer; failing (not running) is fine.
        unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) };
        // SAFETY: valid handle.
        if unsafe { DeleteService(svc.0) } == 0 {
            log::warn!("shares: delete {name}: {}", io::Error::last_os_error());
        } else {
            log::info!("shares: removed {name} (share gone)");
        }
    }

    fn start(svc: &Sc, name: &str) {
        // SAFETY: valid service handle, no arguments.
        if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) {
                log::warn!("shares: start {name}: {e}");
            }
        }
    }

    /// Make and start a service per viofs device (see the module doc).
    pub fn mount_all() -> io::Result<()> {
        let exe = std::env::current_exe()?.with_file_name("virtiofs.exe");
        if !exe.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} is missing (run vroom-setup.cmd)", exe.display())));
        }
        let exe = exe.to_string_lossy().into_owned();
        let mut tags = Vec::new();
        // Only remove services when every device's tag is known: one we
        // failed to read may still be in use.
        let mut all = true;
        for d in devices()? {
            match tag(&d) {
                Ok(t) if valid_tag(&t) => tags.push(t),
                Ok(t) => log::warn!("shares: skipping a device with tag {t:?}"),
                Err(e) => {
                    all = false;
                    log::warn!("shares: {d}: {e}");
                }
            }
        }
        // SAFETY: local SCM, plain access mask.
        let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
        if scm.is_null() {
            return Err(io::Error::last_os_error());
        }
        let scm = Sc(scm);
        if all {
            match ours(&scm) {
                Ok(names) => stale(&names, &tags).into_iter().for_each(|n| remove(&scm, n)),
                Err(e) => log::warn!("shares: listing services: {e}"),
            }
        }
        // SAFETY: plain call.
        let mut used = unsafe { GetLogicalDrives() };
        let mut todo = Vec::new();
        for t in &tags {
            let name = format!("{PREFIX}{t}");
            let w = wide(&name);
            // SAFETY: valid SCM handle and name.
            let h = unsafe { OpenServiceW(scm.0, w.as_ptr(), SERVICE_ALL_ACCESS) };
            if h.is_null() {
                todo.push(t);
                continue;
            }
            let svc = Sc(h);
            if let Some(l) = command_of(&svc).as_deref().and_then(letter_of) {
                used |= 1 << (l as u8 - b'A');
            }
            // A USB disk's share starts when a disk is there (an older
            // agent made it start at boot: not any more).
            if usb_tag(t) {
                let none = SERVICE_NO_CHANGE;
                // SAFETY: valid service handle; NULL leaves the rest as is.
                let ok = unsafe {
                    ChangeServiceConfigW(svc.0, none, SERVICE_DEMAND_START, none, std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null())
                };
                if ok == 0 {
                    log::warn!("shares: {name}: {}", io::Error::last_os_error());
                }
            } else {
                start(&svc, &name);
            }
        }
        for t in todo {
            let Some(letter) = pick_letter(used) else {
                log::warn!("shares: no drive letter left for {t}");
                break;
            };
            used |= 1 << (letter as u8 - b'A');
            let name = format!("{PREFIX}{t}");
            let usb = usb_tag(t);
            let what = if usb { format!("Vroom USB disk files {t}") } else { format!("Vroom shared folder {t}") };
            let (w, display, cmd) = (wide(&name), wide(&what), wide(&command_line(&exe, t, letter)));
            let deps: Vec<u16> = "WinFsp.Launcher\0VirtioFsDrv\0\0".encode_utf16().collect();
            // SAFETY: valid SCM handle and NUL-terminated strings; deps is
            // a double-NUL-terminated list; LocalSystem account.
            let h = unsafe {
                CreateServiceW(
                    scm.0,
                    w.as_ptr(),
                    display.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    if usb { SERVICE_DEMAND_START } else { SERVICE_AUTO_START },
                    SERVICE_ERROR_NORMAL,
                    cmd.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    deps.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if h.is_null() {
                log::warn!("shares: create {name}: {}", io::Error::last_os_error());
                continue;
            }
            let svc = Sc(h);
            log::info!("shares: {t} on {letter}:");
            if !usb {
                start(&svc, &name);
            }
        }
        Ok(())
    }

    /// Start (`on`) or stop the service of USB disk share `tag`; made
    /// first if the boot didn't (the agent started before the device).
    pub fn usb_service(tag: &str, on: bool) -> io::Result<()> {
        let open = || -> io::Result<Option<Sc>> {
            // SAFETY: local SCM, plain access mask.
            let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
            if scm.is_null() {
                return Err(io::Error::last_os_error());
            }
            let scm = Sc(scm);
            let w = wide(&format!("{PREFIX}{tag}"));
            // SAFETY: valid SCM handle and name.
            let h = unsafe { OpenServiceW(scm.0, w.as_ptr(), SERVICE_ALL_ACCESS) };
            Ok((!h.is_null()).then_some(Sc(h)))
        };
        let svc = match open()? {
            Some(s) => s,
            None if on => {
                mount_all()?;
                open()?.ok_or_else(|| io::Error::other("no service for it"))?
            }
            None => return Ok(()),
        };
        if on {
            start(&svc, tag);
        } else {
            // SAFETY: zeroed SERVICE_STATUS is a valid out value.
            let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
            // SAFETY: valid handle and out pointer; failing (not running) is fine.
            unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert!(valid_tag("host") && valid_tag("a.b_c-1"));
        for bad in ["", "a b", "a\"b", "x -m C:", &"a".repeat(37), "é"] {
            assert!(!valid_tag(bad), "{bad:?}");
        }
    }

    #[test]
    fn usb_mount_points() {
        assert!(usb_tag("vroom-usb0") && !usb_tag("host") && !usb_tag("vroom-usb"));
        assert_eq!(mount_point("STICK", &[]), "/media/vroom/STICK");
        let taken = vec!["/media/vroom/STICK".to_string(), "/media/vroom/STICK 2".to_string()];
        assert_eq!(mount_point("STICK", &taken), "/media/vroom/STICK 3");
        assert_eq!(mount_point("TEST 1", &taken), "/media/vroom/TEST 1");
    }

    #[test]
    fn stale_services() {
        let names: Vec<String> = ["VroomShare_host", "VroomShare_old", "VroomShare_", "Other_old"].map(String::from).into();
        assert_eq!(stale(&names, &["host".into()]), ["VroomShare_old", "VroomShare_"]);
        assert_eq!(stale(&names, &["host".into(), "old".into(), "".into()]), Vec::<&str>::new());
        assert_eq!(stale(&[], &[]), Vec::<&str>::new());
    }

    #[test]
    fn letters() {
        let exe = r"C:\Program Files\Vroom\virtiofs.exe";
        let cmd = command_line(exe, "host", 'Z');
        assert_eq!(cmd, r#""C:\Program Files\Vroom\virtiofs.exe" -t host -m Z:"#);
        assert_eq!(letter_of(&cmd), Some('Z'));
        assert_eq!(letter_of(r#""x.exe" -t a -m "y:""#), Some('Y'));
        // A folder mount point holds no letter.
        assert_eq!(letter_of(r"x.exe -t a -m C:\Shares\a"), None);
        assert_eq!(letter_of("x.exe -t a"), None);
        assert_eq!(letter_of("x.exe -m"), None);
        // C: and D: in use: Z: first, then down.
        assert_eq!(pick_letter(0b1100), Some('Z'));
        assert_eq!(pick_letter(0b1100 | 1 << 25), Some('Y'));
        // Never A:, B: or C:.
        assert_eq!(pick_letter(!0b111), None);
        assert_eq!(pick_letter(!0b1000), Some('D'));
    }
}
