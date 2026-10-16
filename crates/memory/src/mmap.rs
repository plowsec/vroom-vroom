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
    /// Shared memory behind it (another process maps it too: the GPU
    /// process), or None for private memory.
    shared: Option<std::sync::Mutex<std::os::fd::OwnedFd>>,
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
        if size == 0 || !size.is_multiple_of(ps) {
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
        Ok(MmapRegion { base, size, guard, shared: None })
    }

    /// Like [`MmapRegion::new`], backed by a shared memory object that
    /// another process can map (see [`MmapRegion::shared_fd`]).
    pub fn new_shared(size: usize) -> Result<Self> {
        let mut r = Self::new(size)?;
        let fd = shm_object(size)?;
        r.map_shared(&fd)?;
        r.shared = Some(std::sync::Mutex::new(fd));
        Ok(r)
    }

    /// The shared memory object, if this is shared memory: for the other
    /// process to map. Changes when the region is discarded.
    pub fn shared_fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        self.shared.as_ref().map(|f| f.lock().unwrap_or_else(|e| e.into_inner()).as_raw_fd())
    }

    fn map_shared(&self, fd: &std::os::fd::OwnedFd) -> Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: replaces our own mapping in place with the object's pages.
        let p = unsafe {
            libc::mmap(self.base as *mut _, self.size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_FIXED, fd.as_raw_fd(), 0)
        };
        if p == libc::MAP_FAILED {
            return Err(Error::Mmap(errno()));
        }
        Ok(())
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
        if !off.is_multiple_of(ps) || !len.is_multiple_of(ps) || off.checked_add(len).is_none_or(|e| e > self.size) {
            return Err(Error::Unaligned(ps));
        }
        if let Some(f) = &self.shared {
            // Shared memory can't be zeroed and freed in place: all of it
            // gets a fresh object (the other process maps the new one),
            // part of it is zeroed.
            if off == 0 && len == self.size {
                let fd = shm_object(self.size)?;
                self.map_shared(&fd)?;
                *f.lock().unwrap_or_else(|e| e.into_inner()) = fd;
            } else {
                self.fill_at(off, len, 0);
            }
            return Ok(());
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

/// An unnamed shared memory object of `size` bytes.
fn shm_object(size: usize) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    // Named only for a moment (unlinked at once); the pid in it is what
    // the VMM's sandbox lets it create.
    let name = std::ffi::CString::new(format!("/vroom-ram-{}-{n:x}-{t:x}", std::process::id())).map_err(|_| Error::Mmap(libc::EINVAL))?;
    // SAFETY: valid C string; the fd is owned below.
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint) };
    if fd < 0 {
        return Err(Error::Mmap(errno()));
    }
    // SAFETY: fd is ours from here on.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    // SAFETY: the name we just created.
    unsafe { libc::shm_unlink(name.as_ptr()) };
    use std::os::fd::AsRawFd;
    // Only the GPU process gets it, and that on purpose (spawn clears this).
    // SAFETY: plain call on our fd.
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: plain call on our fd.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
        return Err(Error::Mmap(errno()));
    }
    Ok(fd)
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
