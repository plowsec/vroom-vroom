//! OS actions: clock, power.

#[cfg(unix)]
pub fn set_time(secs: u64, nanos: u32) -> std::io::Result<()> {
    let ts = libc::timespec { tv_sec: secs as _, tv_nsec: nanos as _ };
    // SAFETY: valid timespec pointer; kernel checks privileges.
    let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub fn set_time(secs: u64, nanos: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::SystemInformation::SetSystemTime;
    // FILETIME: 100ns ticks since 1601-01-01.
    let ticks = (secs + 11_644_473_600) * 10_000_000 + (nanos / 100) as u64;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    // SAFETY: SYSTEMTIME is plain data; all-zero is valid.
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers.
    if unsafe { windows_sys::Win32::System::Time::FileTimeToSystemTime(&ft, &mut st) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: valid pointer.
    if unsafe { SetSystemTime(&st) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn shutdown(reboot: bool) {
    log::info!("host requested {}", if reboot { "reboot" } else { "shutdown" });
    let r = if cfg!(windows) {
        std::process::Command::new("shutdown").args([if reboot { "/r" } else { "/s" }, "/t", "0"]).status()
    } else {
        std::process::Command::new("systemctl").arg(if reboot { "reboot" } else { "poweroff" }).status()
    };
    if let Err(e) = r {
        log::warn!("shutdown: {e}");
    }
}

/// Change the primary display mode (Windows). Linux guests follow the
/// virtio-gpu hotplug event on their own, so nothing to do there.
#[cfg(windows)]
pub fn set_resolution(w: u32, h: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, DEVMODEW, CDS_UPDATEREGISTRY, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT, DM_PELSWIDTH,
    };
    // SAFETY: zeroed DEVMODEW is valid once dmSize is set.
    let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    dm.dmPelsWidth = w;
    dm.dmPelsHeight = h;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
    // SAFETY: valid DEVMODEW; null device name = primary display.
    let r = unsafe { ChangeDisplaySettingsExW(std::ptr::null(), &dm, std::ptr::null_mut(), CDS_UPDATEREGISTRY, std::ptr::null()) };
    if r == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("ChangeDisplaySettingsEx: {r}")))
    }
}

#[cfg(not(windows))]
pub fn set_resolution(_w: u32, _h: u32) -> std::io::Result<()> {
    Ok(())
}

/// Windows: one display. With virtio-gpu next to the firmware framebuffer
/// (ramfb), Windows extends the desktop over both, and windows open on a
/// screen nobody sees. Keep only the virtio-gpu output ("internal").
#[cfg(windows)]
pub fn single_display() {
    use windows_sys::Win32::Devices::Display::{
        GetDisplayConfigBufferSizes, SetDisplayConfig, QDC_ONLY_ACTIVE_PATHS, SDC_APPLY, SDC_TOPOLOGY_INTERNAL,
    };
    let (mut paths, mut modes) = (0u32, 0u32);
    // SAFETY: valid out pointers.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) } != 0 || paths <= 1 {
        return;
    }
    // Several monitors of ours: the host's layout (SetLayout) says which
    // are on, and takes the basic display off.
    if crate::layout::our_displays() > 1 {
        return;
    }
    // SAFETY: no arrays: apply a stock topology.
    let r = unsafe { SetDisplayConfig(0, std::ptr::null(), 0, std::ptr::null(), SDC_APPLY | SDC_TOPOLOGY_INTERNAL) };
    log::info!("{paths} active displays; internal-only topology: {r}");
}

#[cfg(not(windows))]
pub fn single_display() {}

/// Turn Windows' 3D display device (UTM's viogpu3d, service VioGpu3D) off
/// or on, as Device Manager does. Off, Windows falls back to its basic
/// display and drops every Direct3D object, which the host can't save in
/// a suspend. Whether it is on afterwards; None without such a device.
#[cfg(windows)]
pub fn gpu3d(on: bool) -> Option<bool> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_Status, SetupDiCallClassInstaller, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
        SetupDiGetDeviceRegistryPropertyW, SetupDiSetClassInstallParamsW, CM_PROB_DISABLED, CR_SUCCESS, DICS_DISABLE, DICS_ENABLE,
        DICS_FLAG_GLOBAL, DIF_PROPERTYCHANGE, DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY, SPDRP_SERVICE, SP_CLASSINSTALL_HEADER, SP_DEVINFO_DATA,
        SP_PROPCHANGE_PARAMS,
    };
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    // SAFETY: a valid class GUID; the list is destroyed below.
    let set = unsafe { SetupDiGetClassDevsW(&GUID_DEVCLASS_DISPLAY, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT) };
    if set as isize == INVALID_HANDLE_VALUE as isize {
        return None;
    }
    let mut result = None;
    for i in 0.. {
        // SAFETY: plain data; all zeroes is valid once cbSize is set.
        let mut d: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        // SAFETY: valid list and out pointer.
        if unsafe { SetupDiEnumDeviceInfo(set, i, &mut d) } == 0 {
            break;
        }
        let mut name = [0u16; 64];
        let mut ty = 0u32;
        // SAFETY: the buffer is valid for its size in bytes.
        let got = unsafe {
            SetupDiGetDeviceRegistryPropertyW(set, &d, SPDRP_SERVICE, &mut ty, name.as_mut_ptr() as *mut u8, (name.len() * 2) as u32, std::ptr::null_mut())
        };
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        if got == 0 || !String::from_utf16_lossy(&name[..n]).eq_ignore_ascii_case("VioGpu3D") {
            continue;
        }
        let is_on = || {
            let (mut status, mut problem) = (0u32, 0u32);
            // SAFETY: valid out pointers and a devnode from the list.
            let r = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, d.DevInst, 0) };
            r == CR_SUCCESS && problem != CM_PROB_DISABLED
        };
        if is_on() != on {
            let p = SP_PROPCHANGE_PARAMS {
                ClassInstallHeader: SP_CLASSINSTALL_HEADER { cbSize: std::mem::size_of::<SP_CLASSINSTALL_HEADER>() as u32, InstallFunction: DIF_PROPERTYCHANGE },
                StateChange: if on { DICS_ENABLE } else { DICS_DISABLE },
                Scope: DICS_FLAG_GLOBAL,
                HwProfile: 0,
            };
            // SAFETY: valid list, device and parameters of the size given.
            let ok = unsafe {
                SetupDiSetClassInstallParamsW(set, &d, &p.ClassInstallHeader, std::mem::size_of::<SP_PROPCHANGE_PARAMS>() as u32) != 0
                    && SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set, &d) != 0
            };
            if !ok {
                log::warn!("3D display device: {}", std::io::Error::last_os_error());
            }
        }
        // Several (a GPU per monitor): it's done when every one is.
        let now = is_on();
        log::info!("3D display device {i} {}", if now { "on" } else { "off" });
        result = Some(match result {
            None => now,
            Some(r) if on => r && now,
            Some(r) => r || now,
        });
    }
    // SAFETY: the list from above, not used after this.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    result
}

#[cfg(not(windows))]
pub fn gpu3d(_on: bool) -> Option<bool> {
    None
}

/// Windows: a monitor after the first comes on a GPU of its own, always
/// a plain 2D one (Direct3D can't share frames between two 3D GPUs).
/// Both virtio GPU drivers match every virtio GPU, and Windows picks one
/// by its own ranking: the 3D one fails to start on a 2D GPU, and
/// installing the 2D one on a GPU can make Windows move the first GPU to
/// it too. So put the 2D driver (viogpudo, "DOD") on the extra GPUs and,
/// with `first_3d` (the host has 3D), the 3D one on the first, when they
/// are in the driver store, as choosing one in Device Manager does.
/// Nothing happens for a GPU that has the right one already.
#[cfg(windows)]
pub fn gpu_drivers(first_3d: bool) {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiBuildDriverInfoList, SetupDiCallClassInstaller, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiEnumDriverInfoW,
        SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW, SetupDiSetSelectedDriverW, DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY,
        CM_Get_DevNode_Status, CR_SUCCESS, DIF_INSTALLDEVICE, SPDIT_COMPATDRIVER, SPDRP_DEVICEDESC, SPDRP_HARDWAREID, SP_DEVINFO_DATA, SP_DRVINFO_DATA_V2_W,
    };
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    let text = |w: &[u16]| String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())]);
    // SAFETY: a valid class GUID; the list is destroyed below.
    let set = unsafe { SetupDiGetClassDevsW(&GUID_DEVCLASS_DISPLAY, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT) };
    if set as isize == INVALID_HANDLE_VALUE as isize {
        return;
    }
    for i in 0.. {
        // SAFETY: plain data; all zeroes is valid once cbSize is set.
        let mut d: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        // SAFETY: valid list and out pointer.
        if unsafe { SetupDiEnumDeviceInfo(set, i, &mut d) } == 0 {
            break;
        }
        let prop = |p| {
            let mut buf = [0u16; 512];
            let mut ty = 0u32;
            // SAFETY: the buffer is valid for its size in bytes.
            let ok = unsafe { SetupDiGetDeviceRegistryPropertyW(set, &d, p, &mut ty, buf.as_mut_ptr() as *mut u8, (buf.len() * 2) as u32, std::ptr::null_mut()) };
            // A multi-string: its first is enough.
            if ok != 0 { text(&buf) } else { String::new() }
        };
        let hwid = prop(SPDRP_HARDWAREID);
        // The 3D driver on a GPU without 3D (the VM was set to 2D) fails
        // to start: Windows would get its basic display (800x600).
        let failed = || {
            let (mut status, mut problem) = (0u32, 0u32);
            // SAFETY: valid out pointers and a devnode from the list.
            unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, d.DevInst, 0) == CR_SUCCESS && problem != 0 }
        };
        let want = match crate::layout::gpu_head(&hwid) {
            Some(0) if first_3d => "VirtIO GPU 3D",
            Some(0) if prop(SPDRP_DEVICEDESC).contains("VirtIO GPU 3D") && failed() => "VirtIO GPU DOD",
            Some(h) if h > 0 => "VirtIO GPU DOD",
            _ => continue,
        };
        if prop(SPDRP_DEVICEDESC).contains(want) {
            continue;
        }
        // SAFETY: valid list and device.
        if unsafe { SetupDiBuildDriverInfoList(set, &mut d, SPDIT_COMPATDRIVER) } == 0 {
            continue;
        }
        let mut chosen = None;
        for j in 0.. {
            // SAFETY: plain data; all zeroes is valid once cbSize is set.
            let mut drv: SP_DRVINFO_DATA_V2_W = unsafe { std::mem::zeroed() };
            drv.cbSize = std::mem::size_of::<SP_DRVINFO_DATA_V2_W>() as u32;
            // SAFETY: valid list, device, out pointer.
            if unsafe { SetupDiEnumDriverInfoW(set, &d, SPDIT_COMPATDRIVER, j, &mut drv) } == 0 {
                break;
            }
            let desc = text(&{ drv.Description });
            if desc.contains(want) {
                // The newest such driver.
                if chosen.as_ref().is_none_or(|c: &SP_DRVINFO_DATA_V2_W| { drv.DriverVersion } > { c.DriverVersion }) {
                    chosen = Some(drv);
                }
            }
        }
        let Some(mut drv) = chosen else {
            log::warn!("GPU {hwid}: no \"{want}\" driver in the driver store (2D: viogpudo from virtio-win, 3D: viogpu3d)");
            continue;
        };
        // This device only: DiInstallDevice would put the driver on every
        // GPU it matches as well (Windows' "secondary devices").
        // SAFETY: valid list, device and a driver from its list.
        let ok = unsafe { SetupDiSetSelectedDriverW(set, &mut d, &mut drv) != 0 && SetupDiCallClassInstaller(DIF_INSTALLDEVICE, set, &d) != 0 };
        if ok {
            log::info!("GPU {hwid}: \"{want}\" driver installed");
        } else {
            log::warn!("GPU {hwid}: \"{want}\" driver: {}", std::io::Error::last_os_error());
        }
    }
    // SAFETY: the list from above, not used after this.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn gpu_drivers(_first_3d: bool) {}

/// Linux: whether the desktop this process was started in is still there
/// (its Wayland socket, or an X server that takes a connection). None:
/// started without one.
#[cfg(target_os = "linux")]
pub fn display_alive() -> Option<bool> {
    if let (Some(w), Some(dir)) = (std::env::var_os("WAYLAND_DISPLAY"), std::env::var_os("XDG_RUNTIME_DIR")) {
        let p = std::path::Path::new(&dir).join(w);
        return Some(std::fs::symlink_metadata(p).is_ok());
    }
    std::env::var_os("DISPLAY")?;
    Some(x11rb::connect(None).is_ok())
}
