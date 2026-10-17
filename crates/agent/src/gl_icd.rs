//! Windows: OpenGL for apps on the 3D display (viogpu3d). vroom_gl.dll,
//! installed next to the agent, is an OpenGL driver ("ICD") that sends
//! OpenGL to the host's virgl renderer. Windows' opengl32 finds an ICD
//! through values in the display device's driver key, read when the
//! device starts; the 3D driver's own install doesn't set them, so the
//! service does, and restarts the device the first time.

/// What to do with a device's OpenGLDriverName.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Keep,
    Set,
    Remove,
}

/// `current`: the device's value; `path`: where our ICD is or would be.
/// A value naming someone else's ICD is left alone.
pub fn plan(current: Option<&str>, path: &str, installed: bool) -> Plan {
    let ours = current.is_some_and(|c| c.eq_ignore_ascii_case(path));
    let empty = current.is_none_or(|c| c.is_empty());
    match (installed, ours, empty) {
        (true, false, true) => Plan::Set,
        (false, true, _) => Plan::Remove,
        _ => Plan::Keep,
    }
}

/// Register (or unregister) vroom_gl.dll on every 3D display device; if
/// anything changed, restart the 3D display so Windows reads it.
#[cfg(windows)]
pub fn apply() {
    let Some(dll) = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.join("vroom_gl.dll"))) else { return };
    let path = dll.to_string_lossy().into_owned();
    let installed = dll.exists();
    match update(&path, installed) {
        Ok(true) => {
            log::info!("OpenGL driver {}: restarting the 3D display", if installed { "registered" } else { "removed" });
            crate::sys::gpu3d(false);
            crate::sys::gpu3d(true);
        }
        Ok(false) => {}
        Err(e) => log::warn!("OpenGL driver: {e}"),
    }
}

#[cfg(windows)]
fn update(path: &str, installed: bool) -> std::io::Result<bool> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW,
        SetupDiOpenDevRegKey, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DRV, GUID_DEVCLASS_DISPLAY, SPDRP_SERVICE, SP_DEVINFO_DATA,
    };
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Registry::{RegCloseKey, KEY_QUERY_VALUE, KEY_SET_VALUE};

    // SAFETY: a valid class GUID; the list is destroyed below.
    let set = unsafe { SetupDiGetClassDevsW(&GUID_DEVCLASS_DISPLAY, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT) };
    if set as isize == INVALID_HANDLE_VALUE as isize {
        return Err(std::io::Error::last_os_error());
    }
    let mut changed = false;
    let mut result = Ok(());
    for i in 0.. {
        // SAFETY: plain data; all zeroes is valid once cbSize is set.
        let mut d: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        // SAFETY: valid list and out pointer.
        if unsafe { SetupDiEnumDeviceInfo(set, i, &mut d) } == 0 {
            break;
        }
        let mut name = [0u16; 64];
        // SAFETY: the buffer is valid for its size in bytes.
        let got = unsafe {
            SetupDiGetDeviceRegistryPropertyW(set, &d, SPDRP_SERVICE, std::ptr::null_mut(), name.as_mut_ptr() as *mut u8, (name.len() * 2) as u32, std::ptr::null_mut())
        };
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        if got == 0 || !String::from_utf16_lossy(&name[..n]).eq_ignore_ascii_case("VioGpu3D") {
            continue;
        }
        // SAFETY: valid list and device; the key is closed below.
        let key = unsafe { SetupDiOpenDevRegKey(set, &d, DICS_FLAG_GLOBAL, 0, DIREG_DRV, KEY_QUERY_VALUE | KEY_SET_VALUE) };
        if key as isize == INVALID_HANDLE_VALUE as isize {
            result = Err(std::io::Error::last_os_error());
            continue;
        }
        let current = reg::get_sz(key, "OpenGLDriverName");
        let r = match plan(current.as_deref(), path, installed) {
            Plan::Keep => Ok(()),
            Plan::Set => {
                changed = true;
                reg::set_sz(key, "OpenGLDriverName", path).and_then(|_| reg::set_dword(key, "OpenGLVersion", 1)).and_then(|_| reg::set_dword(key, "OpenGLFlags", 1))
            }
            Plan::Remove => {
                changed = true;
                ["OpenGLDriverName", "OpenGLVersion", "OpenGLFlags"].iter().try_for_each(|v| reg::delete(key, v))
            }
        };
        if r.is_err() {
            result = r;
        }
        // SAFETY: the key opened above.
        unsafe { RegCloseKey(key) };
    }
    // SAFETY: the list from above, not used after this.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    result.map(|_| changed)
}

#[cfg(windows)]
mod reg {
    use windows_sys::Win32::System::Registry::{RegDeleteValueW, RegQueryValueExW, RegSetValueExW, HKEY, REG_DWORD, REG_SZ};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn check(r: u32) -> std::io::Result<()> {
        if r == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(r as i32))
        }
    }

    pub fn get_sz(key: HKEY, name: &str) -> Option<String> {
        let mut buf = [0u16; 512];
        let (mut ty, mut len) = (0u32, std::mem::size_of_val(&buf) as u32);
        let name = wide(name);
        // SAFETY: an open key, a NUL-terminated name, a buffer of len bytes.
        let r = unsafe { RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut len) };
        if r != 0 || ty != REG_SZ {
            return None;
        }
        let s = &buf[..(len as usize / 2).min(buf.len())];
        Some(String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]))
    }

    pub fn set_sz(key: HKEY, name: &str, value: &str) -> std::io::Result<()> {
        let (name, value) = (wide(name), wide(value));
        // SAFETY: an open key; the data is value.len() * 2 bytes.
        check(unsafe { RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, value.as_ptr() as *const u8, (value.len() * 2) as u32) })
    }

    pub fn set_dword(key: HKEY, name: &str, value: u32) -> std::io::Result<()> {
        let name = wide(name);
        // SAFETY: an open key; the data is 4 bytes.
        check(unsafe { RegSetValueExW(key, name.as_ptr(), 0, REG_DWORD, &value as *const u32 as *const u8, 4) })
    }

    pub fn delete(key: HKEY, name: &str) -> std::io::Result<()> {
        let name = wide(name);
        // SAFETY: an open key and a NUL-terminated name.
        match unsafe { RegDeleteValueW(key, name.as_ptr()) } {
            2 => Ok(()), // ERROR_FILE_NOT_FOUND: already gone
            r => check(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = r"C:\Program Files\Vroom\vroom_gl.dll";

    #[test]
    fn plans() {
        assert_eq!(plan(None, OURS, true), Plan::Set);
        assert_eq!(plan(Some(""), OURS, true), Plan::Set);
        assert_eq!(plan(Some(&OURS.to_uppercase()), OURS, true), Plan::Keep);
        // Someone else's ICD stays.
        assert_eq!(plan(Some("other.dll"), OURS, true), Plan::Keep);
        // Ours, uninstalled: take it away.
        assert_eq!(plan(Some(OURS), OURS, false), Plan::Remove);
        assert_eq!(plan(None, OURS, false), Plan::Keep);
        assert_eq!(plan(Some("other.dll"), OURS, false), Plan::Keep);
    }
}
