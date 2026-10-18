//! Windows: OpenGL and Vulkan for apps on the 3D display (viogpu3d).
//! Installed next to the agent: vroom_gl.dll, an OpenGL driver ("ICD"),
//! and a Vulkan one, vroom_vk_arm64.dll and vroom_vk_x64.dll with a
//! manifest each (vroom_vk_*.json). Windows finds them through values in
//! the display device's driver key:
//! - opengl32 reads OpenGLDriverName when the device starts. The 3D
//!   driver's own install doesn't set it, so the service does, and
//!   restarts the device the first time.
//! - The Vulkan loader reads VulkanDriverName each time an app starts: a
//!   list, ours names both manifests (a process skips the DLL of the other
//!   architecture). The 3D driver sets it to its own manifest, which names
//!   its DLL without a path: the loader fails to open it (error 87) and
//!   then fails every vkCreateInstance, ours too. So ours replaces it.
//! - Direct3D loads the UMDs UserModeDriverName lists when the device
//!   starts. Ours, vroom_d3d.dll, wraps the 3D driver's own
//!   (neptune_umd.dll) so that apps survive the 3D display going off for
//!   a save (scripts/guest/d3d/vroom_d3d.c). It takes that driver's path
//!   from vroom_d3d.txt next to it, which the service writes.

/// What to do with one of a device's driver values.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Keep,
    Set,
    Remove,
}

/// `current`: the device's value; `path`: where our ICD is or would be;
/// `replace`: values we take over. Any other value naming someone else's
/// ICD is left alone. A list value is its entries joined with '\n'.
pub fn plan(current: Option<&str>, path: &str, installed: bool, replace: impl Fn(&str) -> bool) -> Plan {
    let ours = current.is_some_and(|c| c.eq_ignore_ascii_case(path));
    let free = current.is_none_or(|c| c.is_empty() || replace(c));
    match (installed, ours, free) {
        (true, false, true) => Plan::Set,
        (false, true, _) => Plan::Remove,
        _ => Plan::Keep,
    }
}

/// Only the 3D driver package's own Vulkan manifest (see the module
/// comment).
pub fn package_vk_manifest(value: &str) -> bool {
    value.lines().all(|v| v.rsplit(['\\', '/']).next().is_some_and(|f| f.eq_ignore_ascii_case("virtio_icd.json")))
}

/// The 3D driver's Direct3D UMD, which ours wraps.
const STOCK_UMD: &str = "neptune_umd.dll";
/// Next to our UMD: the path of the one it wraps.
pub const D3D_STOCK_FILE: &str = "vroom_d3d.txt";
/// Ours and its two builds.
pub const D3D_FILES: [&str; 3] = ["vroom_d3d.dll", "vroom_d3d_arm64.dll", "vroom_d3d_x64.dll"];

/// A new UserModeDriverName list (entries joined with '\n') and the stock
/// UMD's path to save, or None to keep it. Installed, each entry naming
/// the 3D driver's own UMD becomes ours; uninstalled, ours goes back to
/// the saved stock path (kept as is without one). "<>" (no Direct3D 9
/// driver) and anyone else's entries stay.
pub fn d3d_plan(current: &str, ours: &str, installed: bool, saved: Option<&str>) -> Option<(String, Option<String>)> {
    let stock = |e: &str| e.rsplit(['\\', '/']).next().is_some_and(|f| f.eq_ignore_ascii_case(STOCK_UMD));
    let mut found = None;
    let list: Vec<String> = current
        .lines()
        .map(|e| match (installed, saved) {
            (true, _) if stock(e) => {
                found.get_or_insert_with(|| e.to_string());
                ours.to_string()
            }
            (false, Some(s)) if e.eq_ignore_ascii_case(ours) => s.to_string(),
            _ => e.to_string(),
        })
        .collect();
    let list = list.join("\n");
    (list != current).then_some((list, found))
}

/// Our Vulkan manifests, in `dir`.
pub const VK_MANIFESTS: [&str; 2] = ["vroom_vk_arm64.json", "vroom_vk_x64.json"];

/// One of our drivers: its value in the driver key, where it is ('\n'
/// between the entries of a list).
#[cfg(windows)]
struct Icd {
    value: &'static str,
    path: String,
    installed: bool,
}

/// Register (or unregister) our OpenGL and Vulkan drivers on every 3D
/// display device; if OpenGL's changed, restart the 3D display so Windows
/// reads it.
#[cfg(windows)]
pub fn apply() {
    let Some(dir) = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.to_path_buf())) else { return };
    let icd = |value, files: &[&str]| {
        let p: Vec<_> = files.iter().map(|f| dir.join(f)).collect();
        let path = p.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n");
        Icd { value, installed: p.iter().all(|p| p.exists()), path }
    };
    let (gl, vk) = (icd("OpenGLDriverName", &["vroom_gl.dll"]), icd("VulkanDriverName", &VK_MANIFESTS));
    // Direct3D's value names the forwarder only.
    let d3d = Icd { path: dir.join(D3D_FILES[0]).to_string_lossy().into_owned(), ..icd("UserModeDriverName", &D3D_FILES) };
    match update(&gl, &vk, &d3d, &dir.join(D3D_STOCK_FILE)) {
        Ok(true) => {
            log::info!("OpenGL or Direct3D driver changed: restarting the 3D display");
            crate::sys::gpu3d(false);
            crate::sys::gpu3d(true);
        }
        Ok(false) => {}
        Err(e) => log::warn!("OpenGL/Vulkan driver: {e}"),
    }
}

/// Whether OpenGL's or Direct3D's value changed on any device.
#[cfg(windows)]
fn update(gl: &Icd, vk: &Icd, d3d: &Icd, stock_file: &std::path::Path) -> std::io::Result<bool> {
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
        let current = reg::get_sz(key, gl.value);
        let r = match plan(current.as_deref(), &gl.path, gl.installed, |_| false) {
            Plan::Keep => Ok(()),
            Plan::Set => {
                changed = true;
                reg::set_sz(key, gl.value, &gl.path).and_then(|_| reg::set_dword(key, "OpenGLVersion", 1)).and_then(|_| reg::set_dword(key, "OpenGLFlags", 1))
            }
            Plan::Remove => {
                changed = true;
                [gl.value, "OpenGLVersion", "OpenGLFlags"].iter().try_for_each(|v| reg::delete(key, v))
            }
        };
        if r.is_err() {
            result = r;
        }
        // Direct3D: the stock path first, then the value naming ours.
        let saved = std::fs::read_to_string(stock_file).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Some((list, stock)) = reg::get_list(key, d3d.value).and_then(|c| d3d_plan(&c, &d3d.path, d3d.installed, saved.as_deref())) {
            let r = match &stock {
                Some(s) if saved.as_deref() != Some(s.as_str()) => std::fs::write(stock_file, s),
                _ => Ok(()),
            }
            .and_then(|_| reg::set_list(key, d3d.value, &list));
            match r {
                Ok(()) => {
                    log::info!("Direct3D driver {}", if d3d.installed { "wrapped" } else { "unwrapped" });
                    changed = true;
                }
                Err(e) => result = Err(e),
            }
        }
        // Vulkan apps read it when they start: nothing to restart.
        let current = reg::get_list(key, vk.value);
        let r = match plan(current.as_deref(), &vk.path, vk.installed, package_vk_manifest) {
            Plan::Keep => Ok(()),
            Plan::Set => {
                log::info!("Vulkan driver registered (was {})", current.as_deref().unwrap_or("none"));
                reg::set_list(key, vk.value, &vk.path)
            }
            Plan::Remove => reg::delete(key, vk.value),
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
    use windows_sys::Win32::System::Registry::{RegDeleteValueW, RegQueryValueExW, RegSetValueExW, HKEY, REG_DWORD, REG_MULTI_SZ, REG_SZ};

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

    /// A REG_MULTI_SZ (or REG_SZ) value, its entries joined with '\n'.
    pub fn get_list(key: HKEY, name: &str) -> Option<String> {
        let mut buf = [0u16; 2048];
        let (mut ty, mut len) = (0u32, std::mem::size_of_val(&buf) as u32);
        let name = wide(name);
        // SAFETY: an open key, a NUL-terminated name, a buffer of len bytes.
        let r = unsafe { RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut len) };
        if r != 0 || (ty != REG_SZ && ty != REG_MULTI_SZ) {
            return None;
        }
        let s = &buf[..(len as usize / 2).min(buf.len())];
        let entries: Vec<String> = s.split(|&c| c == 0).filter(|e| !e.is_empty()).map(String::from_utf16_lossy).collect();
        Some(entries.join("\n"))
    }

    /// A REG_MULTI_SZ value from entries joined with '\n'.
    pub fn set_list(key: HKEY, name: &str, value: &str) -> std::io::Result<()> {
        let name = wide(name);
        let data: Vec<u16> = value.split('\n').flat_map(|e| e.encode_utf16().chain(Some(0))).chain(Some(0)).collect();
        // SAFETY: an open key; the data is data.len() * 2 bytes.
        check(unsafe { RegSetValueExW(key, name.as_ptr(), 0, REG_MULTI_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32) })
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
    const VK: &str = "C:\\Program Files\\Vroom\\vroom_vk_arm64.json\nC:\\Program Files\\Vroom\\vroom_vk_x64.json";
    const PACKAGE: &str = r"C:\WINDOWS\System32\DriverStore\FileRepository\viogpu3d.inf_arm64_d990ba3bca64b8b0\virtio_icd.json";

    #[test]
    fn plans() {
        let never = |_: &str| false;
        assert_eq!(plan(None, OURS, true, never), Plan::Set);
        assert_eq!(plan(Some(""), OURS, true, never), Plan::Set);
        assert_eq!(plan(Some(&OURS.to_uppercase()), OURS, true, never), Plan::Keep);
        // Someone else's ICD stays.
        assert_eq!(plan(Some("other.dll"), OURS, true, never), Plan::Keep);
        // Ours, uninstalled: take it away.
        assert_eq!(plan(Some(OURS), OURS, false, never), Plan::Remove);
        assert_eq!(plan(None, OURS, false, never), Plan::Keep);
        assert_eq!(plan(Some("other.dll"), OURS, false, never), Plan::Keep);
    }

    #[test]
    fn direct3d_wraps_the_package_umd() {
        const STOCK: &str = r"C:\WINDOWS\System32\DriverStore\FileRepository\viogpu3d.inf_arm64_d990ba3bca64b8b0\neptune_umd.dll";
        const MINE: &str = r"C:\Program Files\Vroom\vroom_d3d.dll";
        let stock = format!("<>\n{STOCK}\n{STOCK}\n{STOCK}");
        let mine = format!("<>\n{MINE}\n{MINE}\n{MINE}");
        assert_eq!(d3d_plan(&stock, MINE, true, None), Some((mine.clone(), Some(STOCK.into()))));
        assert_eq!(d3d_plan(&mine, MINE, true, Some(STOCK)), None);
        // A driver update put its own back: wrap that one.
        let newer = stock.replace("d990ba3bca64b8b0", "0123456789abcdef");
        let p = d3d_plan(&newer, MINE, true, Some(STOCK)).unwrap();
        assert_eq!((p.0, p.1), (mine.clone(), Some(STOCK.replace("d990ba3bca64b8b0", "0123456789abcdef"))));
        // Uninstalled: back to the stock driver, if we know it.
        assert_eq!(d3d_plan(&mine, MINE, false, Some(STOCK)), Some((stock.clone(), None)));
        assert_eq!(d3d_plan(&mine, MINE, false, None), None);
        assert_eq!(d3d_plan(&stock, MINE, false, Some(STOCK)), None);
        // Someone else's driver stays.
        assert_eq!(d3d_plan("<>\nC:\\other\\umd.dll", MINE, true, None), None);
    }

    #[test]
    fn vulkan_replaces_the_package_manifest() {
        assert!(package_vk_manifest(PACKAGE));
        assert!(package_vk_manifest("VIRTIO_ICD.JSON"));
        assert!(!package_vk_manifest(VK));
        assert!(!package_vk_manifest(r"C:\x\virtio_icd.json.bak"));
        assert!(package_vk_manifest(&format!("{PACKAGE}\n{PACKAGE}")));
        assert!(!package_vk_manifest(&format!("{PACKAGE}\nC:\\other.json")));
        assert!(VK_MANIFESTS.iter().all(|m| VK.contains(m)));
        assert_eq!(plan(Some(PACKAGE), VK, true, package_vk_manifest), Plan::Set);
        assert_eq!(plan(None, VK, true, package_vk_manifest), Plan::Set);
        assert_eq!(plan(Some(VK), VK, true, package_vk_manifest), Plan::Keep);
        assert_eq!(plan(Some(r"C:\other\icd.json"), VK, true, package_vk_manifest), Plan::Keep);
        // Uninstalled: ours goes, the package's stays as it is.
        assert_eq!(plan(Some(VK), VK, false, package_vk_manifest), Plan::Remove);
        assert_eq!(plan(Some(PACKAGE), VK, false, package_vk_manifest), Plan::Keep);
    }
}
