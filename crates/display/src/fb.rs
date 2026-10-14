//! Shared-memory framebuffer.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::{Error, Result};

/// Largest scanout we support. The mapping is sized for this once (pages
/// are only committed when touched), so mode changes never reallocate.
pub const MAX_WIDTH: u32 = 5120;
pub const MAX_HEIGHT: u32 = 2880;
const BPP: usize = 4;

/// A mapped framebuffer. Accesses are raw copies: the other process may
/// write concurrently (tearing is fine, UB is not).
#[derive(Debug)]
pub struct SharedFb {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: plain shared memory; all access is via raw copies.
unsafe impl Send for SharedFb {}
// SAFETY: see above.
unsafe impl Sync for SharedFb {}

fn max_len() -> usize {
    MAX_WIDTH as usize * MAX_HEIGHT as usize * BPP
}

#[cfg(target_os = "linux")]
fn anon_shm() -> std::io::Result<OwnedFd> {
    // SAFETY: memfd_create with a static name.
    let fd = unsafe { libc::memfd_create(c"bulwark-fb".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh fd we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn anon_shm() -> std::io::Result<OwnedFd> {
    let mut r = [0u8; 8];
    getrandom::getrandom(&mut r).map_err(|e| std::io::Error::other(e.to_string()))?;
    let name = std::ffi::CString::new(format!("/bwfb-{:016x}", u64::from_le_bytes(r)))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // SAFETY: valid C string; exclusive create, owner-only.
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Unlink right away: only fd holders can reach it.
    // SAFETY: valid C string.
    unsafe { libc::shm_unlink(name.as_ptr()) };
    // SAFETY: fresh fd we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn map(fd: &OwnedFd, len: usize) -> Result<*mut u8> {
    // SAFETY: shared mapping of an fd we hold, length checked by caller.
    let p = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
    };
    if p == libc::MAP_FAILED {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(p as *mut u8)
}

impl SharedFb {
    /// VMM side: create and map.
    pub fn create() -> Result<Self> {
        let fd = anon_shm()?;
        let len = max_len();
        // SAFETY: valid fd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        let ptr = map(&fd, len)?;
        Ok(SharedFb { fd, ptr, len })
    }

    /// Viewer side: map an fd received from the VMM.
    pub fn from_fd(fd: OwnedFd) -> Result<Self> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd, valid out pointer.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        let len = max_len();
        if (st.st_size as u64) < len as u64 {
            return Err(Error::Proto("framebuffer too small"));
        }
        let ptr = map(&fd, len)?;
        Ok(SharedFb { fd, ptr, len })
    }

    pub fn fd(&self) -> &OwnedFd {
        &self.fd
    }

    /// Bytes per row for a given width (tightly packed).
    pub fn stride(width: u32) -> usize {
        width as usize * BPP
    }

    /// Is a w x h mode valid for this buffer?
    pub fn mode_ok(width: u32, height: u32) -> bool {
        (1..=MAX_WIDTH).contains(&width) && (1..=MAX_HEIGHT).contains(&height)
    }

    /// Write rows of a rect. `src` holds `h` rows of `w*4` bytes with `src_stride`.
    pub fn write_rect(&self, fb_width: u32, x: u32, y: u32, w: u32, h: u32, src: &[u8], src_stride: usize) {
        let stride = Self::stride(fb_width);
        let row = w as usize * BPP;
        for r in 0..h as usize {
            let so = r * src_stride;
            let dof = (y as usize + r) * stride + x as usize * BPP;
            if so + row > src.len() || dof + row > self.len {
                return;
            }
            // SAFETY: bounds checked against both buffers above.
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr().add(so), self.ptr.add(dof), row) };
        }
    }

    /// Copy the visible w x h image into `dst` (one u32 per pixel, 0RGB).
    pub fn read_into(&self, width: u32, height: u32, dst: &mut [u32]) {
        let n = (width as usize * height as usize).min(dst.len());
        if n * BPP > self.len {
            return;
        }
        // SAFETY: n*4 bytes are inside the mapping; dst has room for n u32s.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr as *const u32, dst.as_mut_ptr(), n) };
    }
}

impl Drop for SharedFb {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(self.ptr as *mut _, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_share_read() {
        let a = SharedFb::create().unwrap();
        let fd2 = a.fd().try_clone().unwrap();
        let b = SharedFb::from_fd(fd2).unwrap();
        let px = [1u8, 2, 3, 0, 4, 5, 6, 0];
        a.write_rect(4, 1, 1, 2, 1, &px, 8);
        let mut out = vec![0u32; 8];
        b.read_into(4, 2, &mut out);
        assert_eq!(out[5], u32::from_le_bytes([1, 2, 3, 0]));
        assert_eq!(out[6], u32::from_le_bytes([4, 5, 6, 0]));
        // Out-of-bounds writes are ignored, not UB.
        a.write_rect(MAX_WIDTH, MAX_WIDTH - 1, MAX_HEIGHT - 1, 2, 2, &[0; 16], 8);
    }
}
