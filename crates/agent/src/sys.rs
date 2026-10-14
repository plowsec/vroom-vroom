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
