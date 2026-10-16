//! Shared-memory framebuffer.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::{Error, Result};

/// Largest scanout we support: 8K, so a full-screen window on a 5K or 6K
/// display fits in physical pixels. The mapping is sized for this once
/// (pages are only committed when touched), so mode changes never
/// reallocate.
pub const MAX_WIDTH: u32 = 7680;
pub const MAX_HEIGHT: u32 = 4320;
/// Most guest monitors one buffer holds: one slot of the size above each,
/// back to back.
pub const MAX_HEADS: usize = 4;
const BPP: usize = 4;

/// A mapped framebuffer: one slot per guest monitor (head). Accesses are
/// raw copies: the other process may write concurrently (tearing is fine,
/// UB is not). Nothing written to one slot reaches another.
#[derive(Debug)]
pub struct SharedFb {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
    heads: usize,
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
    // SAFETY: memfd_create with a static name. Sealable: the viewer
    // insists the size can't change under its mapping.
    let fd = unsafe { libc::memfd_create(c"vroom-fb".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
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
    /// VMM side: create and map, one monitor.
    pub fn create() -> Result<Self> {
        Self::create_heads(1)
    }

    /// VMM side: create and map, `heads` monitors (1..=MAX_HEADS).
    pub fn create_heads(heads: usize) -> Result<Self> {
        if !(1..=MAX_HEADS).contains(&heads) {
            return Err(Error::Proto("monitors"));
        }
        let fd = anon_shm()?;
        let len = max_len() * heads;
        // SAFETY: valid fd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        #[cfg(target_os = "linux")]
        // SAFETY: valid memfd; seals are plain flags.
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL);
        }
        let ptr = map(&fd, len)?;
        Ok(SharedFb { fd, ptr, len, heads })
    }

    /// Viewer side: map an fd received from the VMM.
    pub fn from_fd(fd: OwnedFd) -> Result<Self> {
        // SAFETY: libc::stat is plain data; all-zero is a valid value.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd, valid out pointer.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        // As many whole slots as it holds, at most MAX_HEADS.
        let heads = ((st.st_size.max(0) as u64) / max_len() as u64).min(MAX_HEADS as u64) as usize;
        if heads == 0 {
            return Err(Error::Proto("framebuffer too small"));
        }
        let len = max_len() * heads;
        // A VMM that shrinks the memory under our mapping would crash us
        // (SIGBUS): Linux memfds must be sealed against that. (macOS POSIX
        // shm can't be resized once sized.)
        #[cfg(target_os = "linux")]
        {
            // SAFETY: valid fd.
            let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
            if seals < 0 || seals & libc::F_SEAL_SHRINK == 0 {
                return Err(Error::Proto("framebuffer not sealed"));
            }
        }
        let ptr = map(&fd, len)?;
        Ok(SharedFb { fd, ptr, len, heads })
    }

    /// Monitors it has room for.
    pub fn heads(&self) -> usize {
        self.heads
    }

    /// Start of `head`'s slot, if there is one.
    fn slot(&self, head: usize) -> Option<(*mut u8, usize)> {
        (head < self.heads).then(|| {
            let n = max_len();
            // SAFETY: head < heads: the slot is inside the mapping.
            (unsafe { self.ptr.add(head * n) }, n)
        })
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
    #[allow(clippy::too_many_arguments)]
    pub fn write_rect(&self, fb_width: u32, x: u32, y: u32, w: u32, h: u32, src: &[u8], src_stride: usize) {
        self.write_rect_on(0, fb_width, x, y, w, h, src, src_stride)
    }

    /// [`write_rect`] on monitor `head`'s slot.
    #[allow(clippy::too_many_arguments)]
    pub fn write_rect_on(&self, head: usize, fb_width: u32, x: u32, y: u32, w: u32, h: u32, src: &[u8], src_stride: usize) {
        let Some((base, len)) = self.slot(head) else { return };
        let stride = Self::stride(fb_width);
        let row = w as usize * BPP;
        if x.saturating_add(w) > fb_width {
            return;
        }
        for r in 0..h as usize {
            let so = r * src_stride;
            let dof = (y as usize + r) * stride + x as usize * BPP;
            if so + row > src.len() || dof + row > len {
                return;
            }
            // SAFETY: bounds checked against both buffers above.
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr().add(so), base.add(dof), row) };
        }
    }

    /// Copy the `w` x `h` rect at `x`,`y` of a `fb_width` wide image into
    /// `dst` (raw XRGB bytes, rows `dst_stride` apart). Rows that don't fit
    /// either buffer are skipped.
    #[allow(clippy::too_many_arguments)]
    pub fn read_rect(&self, fb_width: u32, x: u32, y: u32, w: u32, h: u32, dst: &mut [u8], dst_stride: usize) {
        self.read_rect_on(0, fb_width, x, y, w, h, dst, dst_stride)
    }

    /// [`read_rect`] from monitor `head`'s slot.
    #[allow(clippy::too_many_arguments)]
    pub fn read_rect_on(&self, head: usize, fb_width: u32, x: u32, y: u32, w: u32, h: u32, dst: &mut [u8], dst_stride: usize) {
        let Some((base, len)) = self.slot(head) else { return };
        let stride = Self::stride(fb_width);
        let row = w as usize * BPP;
        if x.saturating_add(w) > fb_width {
            return;
        }
        for r in 0..h as usize {
            let so = (y as usize + r) * stride + x as usize * BPP;
            let d = r * dst_stride;
            if so + row > len || d + row > dst.len() {
                return;
            }
            // SAFETY: bounds checked against both buffers above.
            unsafe { std::ptr::copy_nonoverlapping(base.add(so), dst.as_mut_ptr().add(d), row) };
        }
    }

    /// Copy the visible w x h image into `dst` (one u32 per pixel, 0RGB).
    pub fn read_into(&self, width: u32, height: u32, dst: &mut [u32]) {
        self.read_into_on(0, width, height, dst)
    }

    /// [`read_into`] from monitor `head`'s slot.
    pub fn read_into_on(&self, head: usize, width: u32, height: u32, dst: &mut [u32]) {
        let Some((base, len)) = self.slot(head) else { return };
        let n = (width as usize * height as usize).min(dst.len());
        if n * BPP > len {
            return;
        }
        // SAFETY: n*4 bytes are inside the slot; dst has room for n u32s.
        unsafe { std::ptr::copy_nonoverlapping(base as *const u32, dst.as_mut_ptr(), n) };
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
        // A rect: row 1, pixels 1..3, into a 2-pixel-wide buffer.
        let mut r = vec![0u8; 16];
        b.read_rect(4, 1, 1, 2, 1, &mut r, 8);
        assert_eq!(&r[..8], &px);
        // Out of bounds: nothing read, nothing touched.
        let mut r = vec![9u8; 16];
        b.read_rect(4, 3, 0, 2, 1, &mut r, 8);
        b.read_rect(MAX_WIDTH, 0, MAX_HEIGHT - 1, 1, 2, &mut r[..4], 4);
        assert!(r[4..].iter().all(|v| *v == 9));
        // The largest mode works end to end.
        assert!(SharedFb::mode_ok(MAX_WIDTH, MAX_HEIGHT) && !SharedFb::mode_ok(MAX_WIDTH + 1, 1));
        a.write_rect(MAX_WIDTH, MAX_WIDTH - 1, MAX_HEIGHT - 1, 1, 1, &[7; 4], 4);
        let mut r = [0u8; 4];
        b.read_rect(MAX_WIDTH, MAX_WIDTH - 1, MAX_HEIGHT - 1, 1, 1, &mut r, 4);
        assert_eq!(r, [7; 4]);
        assert_eq!(b.heads(), 1);
    }

    #[test]
    fn monitors_have_slots_of_their_own() {
        assert!(SharedFb::create_heads(0).is_err() && SharedFb::create_heads(MAX_HEADS + 1).is_err());
        let a = SharedFb::create_heads(2).unwrap();
        let b = SharedFb::from_fd(a.fd().try_clone().unwrap()).unwrap();
        assert_eq!(b.heads(), 2);
        a.write_rect_on(1, 4, 0, 0, 1, 1, &[9, 9, 9, 0], 4);
        let mut r = [0u8; 4];
        b.read_rect_on(1, 4, 0, 0, 1, 1, &mut r, 4);
        assert_eq!(r, [9, 9, 9, 0]);
        b.read_rect_on(0, 4, 0, 0, 1, 1, &mut r, 4);
        assert_eq!(r, [0; 4], "head 0 untouched");
        // The last row of a slot doesn't spill into the next one.
        a.write_rect_on(0, MAX_WIDTH, 0, MAX_HEIGHT - 1, 1, 2, &[5; 8], 4);
        b.read_rect_on(1, MAX_WIDTH, 0, 0, 1, 1, &mut r, 4);
        assert_eq!(r, [9, 9, 9, 0]);
        // No such head: nothing happens.
        a.write_rect_on(2, 4, 0, 0, 1, 1, &[1; 4], 4);
        let mut px = [7u32; 1];
        b.read_into_on(3, 1, 1, &mut px);
        assert_eq!(px, [7]);
        // A rect wider than the image is refused (it would wrap rows).
        a.write_rect_on(1, 4, 3, 0, 2, 1, &[1; 8], 8);
        b.read_rect_on(1, 4, 3, 0, 1, 1, &mut r, 4);
        assert_eq!(r, [0; 4]);
    }
}
