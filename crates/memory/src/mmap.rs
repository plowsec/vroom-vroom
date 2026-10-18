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
    #[cfg(feature = "research")]
    /// Immutable baseline behind a private mapping. Reset remaps the file so
    /// pages dirtied by the guest are dropped without copying RAM.
    cow: std::sync::Mutex<Option<std::os::fd::OwnedFd>>,
    #[cfg(feature = "research")]
    /// A read-only shared view of that baseline, to copy pages back from.
    cow_view: std::sync::atomic::AtomicPtr<u8>,
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
        let rc =
            unsafe { libc::mprotect(base as *mut _, size, libc::PROT_READ | libc::PROT_WRITE) };
        if rc != 0 {
            let e = errno();
            // SAFETY: unmapping what we mapped above.
            unsafe { libc::munmap(p, total) };
            return Err(Error::Mmap(e));
        }
        exclude_from_core_dump(base, size);
        Ok(MmapRegion {
            base,
            size,
            guard,
            shared: None,
            #[cfg(feature = "research")]
            cow: std::sync::Mutex::new(None),
            #[cfg(feature = "research")]
            cow_view: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        })
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
        self.shared
            .as_ref()
            .map(|f| f.lock().unwrap_or_else(|e| e.into_inner()).as_raw_fd())
    }

    fn map_shared(&self, fd: &std::os::fd::OwnedFd) -> Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: replaces our own mapping in place with the object's pages.
        let p = unsafe {
            libc::mmap(
                self.base as *mut _,
                self.size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd.as_raw_fd(),
                0,
            )
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
        if !off.is_multiple_of(ps)
            || !len.is_multiple_of(ps)
            || off.checked_add(len).is_none_or(|e| e > self.size)
        {
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

    #[cfg(feature = "research")]
    pub fn can_snapshot_cow(&self) -> bool {
        self.shared.is_none()
    }

    #[cfg(feature = "research")]
    /// Freeze this mapping's current bytes as a file-backed private baseline.
    pub fn snapshot_cow(&self) -> Result<()> {
        if self.shared.is_some() {
            return Err(Error::Cow("shared RAM cannot be snapshotted with COW"));
        }
        let fd = cow_object(self.size)?;
        use std::os::fd::AsRawFd;
        // Copy through a temporary shared mapping. `pwrite` is not portable
        // for the anonymous shared-memory objects used on macOS.
        // SAFETY: a new mapping of the object we just made, `size` bytes;
        // the result is checked below.
        let tmp = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if tmp == libc::MAP_FAILED {
            return Err(Error::Mmap(errno()));
        }
        // SAFETY: both mappings cover `size` bytes and do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.base, tmp.cast(), self.size) };
        // Keep it, read-only, to copy pages back from (`restore_cow_copy`).
        // SAFETY: the mapping returned above.
        unsafe { libc::mprotect(tmp, self.size, libc::PROT_READ) };
        let old = self.cow_view.swap(tmp.cast(), std::sync::atomic::Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: the view of an earlier baseline, `size` bytes.
            unsafe { libc::munmap(old.cast(), self.size) };
        }

        // MAP_FIXED is only used after removing our own central mapping. The
        // guard pages stay intact, so host overrun checks remain effective.
        // SAFETY: our own central mapping, remapped at once below; the VM
        // is paused (the caller's rule), so nothing touches it meanwhile.
        let rc = unsafe { libc::munmap(self.base.cast(), self.size) };
        if rc != 0 {
            return Err(Error::Mmap(errno()));
        }
        // SAFETY: MAP_FIXED over the range we just unmapped, same size.
        let mapped = unsafe {
            libc::mmap(
                self.base.cast(),
                self.size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                fd.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(Error::Mmap(errno()));
        }
        *self.cow.lock().unwrap_or_else(|e| e.into_inner()) = Some(fd);
        Ok(())
    }

    #[cfg(feature = "research")]
    /// Drop guest-dirtied private pages so the frozen baseline is visible.
    pub fn restore_cow(&self) -> Result<()> {
        let held = self.cow.lock().unwrap_or_else(|e| e.into_inner());
        let Some(fd) = held.as_ref() else {
            return Err(Error::Cow("no COW baseline is armed"));
        };
        use std::os::fd::AsRawFd;
        // Remapping the same private file is portable: macOS treats
        // MADV_DONTNEED as advisory and may retain dirty private pages.
        // SAFETY: our own central mapping, remapped at once below; the VM
        // is paused (the caller's rule), so nothing touches it meanwhile.
        let rc = unsafe { libc::munmap(self.base.cast(), self.size) };
        if rc != 0 {
            return Err(Error::Mmap(errno()));
        }
        // SAFETY: MAP_FIXED over the range we just unmapped, same size.
        let mapped = unsafe {
            libc::mmap(
                self.base.cast(),
                self.size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                fd.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(Error::Mmap(errno()));
        }
        Ok(())
    }

    #[cfg(feature = "research")]
    /// Copy the COW baseline back over `len` bytes at `off` (pages a reset
    /// knows were written), without remapping anything.
    pub fn restore_cow_copy(&self, off: usize, len: usize) -> Result<()> {
        // Copy from the shared view, not read(2) from the file: a page of
        // ours that isn't resident faults in from that same file, which a
        // read of it into our pages can deadlock on.
        let view = self.cow_view.load(std::sync::atomic::Ordering::Acquire);
        if view.is_null() {
            return Err(Error::Cow("no COW baseline is armed"));
        }
        if off.checked_add(len).is_none_or(|e| e > self.size) {
            return Err(Error::Overflow);
        }
        // SAFETY: both ranges are inside their `size`-byte mappings and
        // don't overlap; the VM is paused (the caller's rule).
        unsafe { std::ptr::copy_nonoverlapping(view.add(off), self.base.add(off), len) };
        Ok(())
    }

    #[cfg(feature = "research")]
    /// Read the COW baseline (not the live RAM) at `off`.
    pub fn read_cow_baseline(&self, off: usize, buf: &mut [u8]) -> Result<()> {
        let view = self.cow_view.load(std::sync::atomic::Ordering::Acquire);
        if view.is_null() {
            return Err(Error::Cow("no COW baseline is armed"));
        }
        if off.checked_add(buf.len()).is_none_or(|e| e > self.size) {
            return Err(Error::Overflow);
        }
        // SAFETY: inside the `size`-byte baseline view.
        unsafe { std::ptr::copy_nonoverlapping(view.add(off), buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }

    #[cfg(feature = "research")]
    /// Change the COW baseline itself at `off` (resets keep the new bytes).
    /// Pages the guest hasn't written since the last reset show them at
    /// once; the caller writes the live mapping too.
    pub fn write_cow_baseline(&self, off: usize, bytes: &[u8]) -> Result<()> {
        let view = self.cow_view.load(std::sync::atomic::Ordering::Acquire);
        if view.is_null() {
            return Err(Error::Cow("no COW baseline is armed"));
        }
        if off.checked_add(bytes.len()).is_none_or(|e| e > self.size) {
            return Err(Error::Overflow);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let page = page_size();
        let first = off & !(page - 1);
        let span = (off + bytes.len()).div_ceil(page) * page - first;
        // SAFETY: pages inside the shared baseline view (`size` bytes, page
        // aligned); writable only for this copy, the VM is paused.
        unsafe {
            if libc::mprotect(view.add(first).cast(), span, libc::PROT_READ | libc::PROT_WRITE) != 0 {
                return Err(Error::Mmap(errno()));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), view.add(off), bytes.len());
            libc::mprotect(view.add(first).cast(), span, libc::PROT_READ);
        }
        Ok(())
    }

    #[cfg(feature = "research")]
    /// Offsets of pages that differ from the COW baseline (a slow check of
    /// a reset: it reads all of RAM and the baseline).
    pub fn cow_verify(&self) -> Result<Vec<usize>> {
        let held = self.cow.lock().unwrap_or_else(|e| e.into_inner());
        let Some(fd) = held.as_ref() else {
            return Err(Error::Cow("no COW baseline is armed"));
        };
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::from(fd.try_clone().map_err(|e| Error::Mmap(e.raw_os_error().unwrap_or(libc::EIO)))?);
        let ps = page_size();
        let mut want = vec![0u8; ps];
        let mut differ = Vec::new();
        for off in (0..self.size).step_by(ps) {
            file.read_exact_at(&mut want, off as u64)
                .map_err(|e| Error::Mmap(e.raw_os_error().unwrap_or(libc::EIO)))?;
            // SAFETY: inside our mapping; the VM is paused (the caller's rule).
            let have = unsafe { std::slice::from_raw_parts(self.base.add(off), ps) };
            if have != want.as_slice() {
                differ.push(off);
            }
        }
        Ok(differ)
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        #[cfg(feature = "research")]
        {
            let view = *self.cow_view.get_mut();
            if !view.is_null() {
                // SAFETY: the baseline view made by snapshot_cow, `size` bytes.
                unsafe { libc::munmap(view.cast(), self.size) };
            }
        }
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
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Named only for a moment (unlinked at once); the pid in it is what
    // the VMM's sandbox lets it create.
    let name = std::ffi::CString::new(format!("/vroom-ram-{}-{n:x}-{t:x}", std::process::id()))
        .map_err(|_| Error::Mmap(libc::EINVAL))?;
    // SAFETY: valid C string; the fd is owned below.
    let fd = unsafe {
        libc::shm_open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600 as libc::c_uint,
        )
    };
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

#[cfg(feature = "research")]
fn cow_object(size: usize) -> Result<std::os::fd::OwnedFd> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("vroom-cow-{}-{n:x}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| Error::Mmap(e.raw_os_error().unwrap_or(libc::EIO)))?;
    let _ = std::fs::remove_file(&path);
    file.set_len(size as u64)
        .map_err(|e| Error::Mmap(e.raw_os_error().unwrap_or(libc::EIO)))?;
    Ok(std::os::fd::OwnedFd::from(file))
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
