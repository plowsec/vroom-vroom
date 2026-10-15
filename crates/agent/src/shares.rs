//! Windows: shared folders as drive letters.
//!
//! Each `[[share]]` is a virtio-fs device. virtio-win's virtiofs.exe
//! (on WinFsp) mounts one device per process, picked by its tag. So for
//! every viofs device we make one service, `VroomShare_<tag>`, running
//! `virtiofs.exe -t <tag> -m X:` with the first free letter from Z: down.
//! A service that already exists is only started: change its mount point
//! with `sc config` and it stays. Runs from `vroom-setup.cmd`
//! (`--mount-shares`) and when the agent service starts, so shares added
//! later show up after a reboot.

/// Service name prefix; the tag follows.
#[cfg_attr(not(windows), allow(dead_code))]
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

pub fn command_line(exe: &str, tag: &str, letter: char) -> String {
    format!("\"{exe}\" -t {tag} -m {letter}:")
}

#[cfg(windows)]
pub use os::mount_all;

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
        CloseServiceHandle, CreateServiceW, OpenSCManagerW, OpenServiceW, QueryServiceConfigW, StartServiceW, QUERY_SERVICE_CONFIGW,
        SC_HANDLE, SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_ERROR_NORMAL, SERVICE_WIN32_OWN_PROCESS,
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
        for d in devices()? {
            match tag(&d) {
                Ok(t) if valid_tag(&t) => tags.push(t),
                Ok(t) => log::warn!("shares: skipping a device with tag {t:?}"),
                Err(e) => log::warn!("shares: {d}: {e}"),
            }
        }
        if tags.is_empty() {
            return Ok(());
        }
        // SAFETY: local SCM, plain access mask.
        let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
        if scm.is_null() {
            return Err(io::Error::last_os_error());
        }
        let scm = Sc(scm);
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
            start(&svc, &name);
        }
        for t in todo {
            let Some(letter) = pick_letter(used) else {
                log::warn!("shares: no drive letter left for {t}");
                break;
            };
            used |= 1 << (letter as u8 - b'A');
            let name = format!("{PREFIX}{t}");
            let (w, display, cmd) = (wide(&name), wide(&format!("Vroom shared folder {t}")), wide(&command_line(&exe, t, letter)));
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
                    SERVICE_AUTO_START,
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
            start(&svc, &name);
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
