use crate::{page_size, Error, Result};

/// An anonymous host mapping that backs a chunk of guest RAM.
///
/// The mapping is surrounded by `PROT_NONE` guard pages, so an off-by-one in
/// host code faults instead of silently touching a neighbour allocation.
#[derive(Debug)]
pub struct MmapRegion {
    base: *mut u8,
    size: usize,
    guard: usize,
}

// SAFETY: the mapping is plain memory, owned by this struct; concurrent access
// is mediated by GuestMemory which only does raw copies.
unsafe impl Send for MmapRegion {}
// SAFETY: see above.
unsafe impl Sync for MmapRegion {}

impl MmapRegion {
    /// Map `size` bytes of zeroed RAM. `size` must be page aligned.
    pub fn new(size: usize) -> Result<Self> {
        let ps = page_size();
        if size == 0 || size % ps != 0 {
            return Err(Error::Unaligned(ps));
        }
        let guard = ps;
        let total = size.checked_add(2 * guard).ok_or(Error::Overflow)?;
        // SAFETY: anonymous private mapping with no address hint.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | map_noreserve(),
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(Error::Mmap(errno()));
        }
        let base = (p as *mut u8).wrapping_add(guard);
        // SAFETY: [base, base+size) lies inside the mapping we just created.
        let rc = unsafe {
            libc::mprotect(base as *mut _, size, libc::PROT_READ | libc::PROT_WRITE)
        };
        if rc != 0 {
            let e = errno();
            // SAFETY: unmapping what we mapped above.
            unsafe { libc::munmap(p, total) };
            return Err(Error::Mmap(e));
        }
        exclude_from_core_dump(base, size);
        Ok(MmapRegion { base, size, guard })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.base
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Copy out of the mapping. Caller has bounds-checked `off..off+dst.len()`.
    pub(crate) fn read_at(&self, off: usize, dst: &mut [u8]) {
        debug_assert!(off + dst.len() <= self.size);
        // SAFETY: range checked by caller; dst is a distinct host buffer.
        unsafe { std::ptr::copy_nonoverlapping(self.base.add(off), dst.as_mut_ptr(), dst.len()) }
    }

    /// Copy into the mapping. Caller has bounds-checked.
    pub(crate) fn write_at(&self, off: usize, src: &[u8]) {
        debug_assert!(off + src.len() <= self.size);
        // SAFETY: range checked by caller; src is a distinct host buffer.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(off), src.len()) }
    }

    pub(crate) fn fill_at(&self, off: usize, len: usize, val: u8) {
        debug_assert!(off + len <= self.size);
        // SAFETY: range checked by caller.
        unsafe { std::ptr::write_bytes(self.base.add(off), val, len) }
    }

    /// Fill the start of the mapping (firmware images).
    pub fn load(&self, data: &[u8]) -> Result<()> {
        if data.len() > self.size {
            return Err(Error::InvalidRange(0, data.len() as u64));
        }
        self.write_at(0, data);
        Ok(())
    }

    /// Release physical pages back to the host (balloon, reset). Reads return 0 after.
    pub fn discard(&self, off: usize, len: usize) -> Result<()> {
        let ps = page_size();
        if off % ps != 0 || len % ps != 0 || off.checked_add(len).is_none_or(|e| e > self.size) {
            return Err(Error::Unaligned(ps));
        }
        // SAFETY: range is inside our mapping. Re-mapping anonymous memory
        // over it is the portable way to zero + free pages.
        let p = unsafe {
            libc::mmap(
                self.base.add(off) as *mut _,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(Error::Mmap(errno()));
        }
        Ok(())
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the mapping created in new().
        unsafe {
            libc::munmap(
                self.base.sub(self.guard) as *mut _,
                self.size + 2 * self.guard,
            );
        }
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn map_noreserve() -> i32 {
    libc::MAP_NORESERVE
}
#[cfg(not(target_os = "linux"))]
fn map_noreserve() -> i32 {
    0
}

/// Guest RAM may hold secrets; keep it out of host core dumps.
fn exclude_from_core_dump(_base: *mut u8, _size: usize) {
    #[cfg(target_os = "linux")]
    // SAFETY: advisory call on our own mapping.
    unsafe {
        libc::madvise(_base as *mut _, _size, libc::MADV_DONTDUMP);
    }
}
