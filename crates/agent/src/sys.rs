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
        result = Some(is_on());
        log::info!("3D display device {}", if result == Some(true) { "on" } else { "off" });
        break;
    }
    // SAFETY: the list from above, not used after this.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    result
}

#[cfg(not(windows))]
pub fn gpu3d(_on: bool) -> Option<bool> {
    None
}
