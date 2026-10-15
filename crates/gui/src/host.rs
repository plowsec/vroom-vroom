//! What this machine has: CPUs, memory, network interfaces, free space.

use std::ffi::CStr;
use std::path::Path;

pub fn cpus() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4)
}

#[allow(unsafe_code)]
pub fn memory_bytes() -> u64 {
    // SAFETY: sysconf has no memory effects.
    let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    if pages > 0 && size > 0 {
        pages as u64 * size as u64
    } else {
        8 << 30
    }
}

#[allow(unsafe_code)]
pub fn free_bytes(p: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: zeroed statvfs is valid plain data.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid C string and out pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// Physical-looking interfaces a VM could bridge to ("en0", "eth0").
#[allow(unsafe_code)]
pub fn interfaces() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a list we free below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: cur walks the list getifaddrs returned.
        let ifa = unsafe { &*cur };
        // SAFETY: ifa_name is a NUL-terminated string owned by the list.
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
        let up = ifa.ifa_flags & libc::IFF_UP as u32 != 0;
        let lo = ifa.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
        let real = ["en", "eth", "wl", "enp", "eno"].iter().any(|p| name.starts_with(p));
        if up && !lo && real && !out.contains(&name) {
            out.push(name);
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: freeing the list we got.
    unsafe { libc::freeifaddrs(head) };
    out.sort();
    out
}
