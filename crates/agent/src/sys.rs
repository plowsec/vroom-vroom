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
