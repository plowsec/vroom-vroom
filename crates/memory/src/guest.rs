use std::io::{Read, Write};
use std::sync::atomic::{fence, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;

use crate::{page_size, ByteValued, Error, GuestAddress, MmapRegion, Result};

/// One contiguous chunk of guest physical RAM.
#[derive(Debug)]
pub struct GuestRegion {
    start: GuestAddress,
    mmap: MmapRegion,
}

impl GuestRegion {
    pub fn start(&self) -> GuestAddress {
        self.start
    }

    pub fn len(&self) -> u64 {
        self.mmap.size() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.mmap.size() == 0
    }

    /// Exclusive end address.
    pub fn end(&self) -> GuestAddress {
        self.start.unchecked_add(self.len())
    }

    /// Host pointer of the backing mapping. Only for handing to the hypervisor.
    pub fn host_ptr(&self) -> *mut u8 {
        self.mmap.as_ptr()
    }

    pub fn mmap(&self) -> &MmapRegion {
        &self.mmap
    }

    fn contains(&self, a: GuestAddress) -> bool {
        a >= self.start && a < self.end()
    }
}

/// The full guest physical RAM map. Cheap to clone (shared regions).
#[derive(Debug, Clone, Default)]
pub struct GuestMemory {
    regions: Arc<Vec<GuestRegion>>,
    /// Bumped when shared RAM gets new objects (reset): whoever maps it
    /// elsewhere must map again.
    generation: Arc<AtomicU64>,
}

impl GuestMemory {
    /// Build guest RAM from `(start, size)` pairs. Ranges must not overlap.
    pub fn new(layout: &[(GuestAddress, u64)]) -> Result<Self> {
        Self::build(layout, false)
    }

    /// Guest RAM in shared memory, for another process (the GPU process)
    /// to map: see [`GuestMemory::shared_fds`].
    pub fn new_shared(layout: &[(GuestAddress, u64)]) -> Result<Self> {
        Self::build(layout, true)
    }

    /// Each region's shared memory object and size, if RAM is shared, and
    /// the generation they belong to.
    pub fn shared_fds(&self) -> Option<(Vec<(std::os::fd::RawFd, u64)>, u64)> {
        let fds = self.regions.iter().map(|r| r.mmap.shared_fd().map(|f| (f, r.len()))).collect::<Option<Vec<_>>>()?;
        Some((fds, self.generation.load(Ordering::Acquire)))
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn build(layout: &[(GuestAddress, u64)], shared: bool) -> Result<Self> {
        let ps = page_size() as u64;
        let mut regions = Vec::with_capacity(layout.len());
        let mut sorted = layout.to_vec();
        sorted.sort_by_key(|(a, _)| *a);
        let mut prev_end = 0u64;
        for (i, &(start, size)) in sorted.iter().enumerate() {
            if start.0 % ps != 0 || size % ps != 0 {
                return Err(Error::Unaligned(ps as usize));
            }
            let end = start.0.checked_add(size).ok_or(Error::Overflow)?;
            if i > 0 && start.0 < prev_end {
                return Err(Error::Overlap(start.0));
            }
            prev_end = end;
            let size = usize::try_from(size).map_err(|_| Error::Overflow)?;
            let mmap = if shared { MmapRegion::new_shared(size)? } else { MmapRegion::new(size)? };
            regions.push(GuestRegion { start, mmap });
        }
        Ok(GuestMemory { regions: Arc::new(regions), generation: Arc::default() })
    }

    pub fn regions(&self) -> impl Iterator<Item = &GuestRegion> {
        self.regions.iter()
    }

    pub fn total_size(&self) -> u64 {
        self.regions.iter().map(|r| r.len()).sum()
    }

    /// Last RAM byte + 1.
    pub fn end_addr(&self) -> GuestAddress {
        self.regions.last().map(|r| r.end()).unwrap_or_default()
    }

    fn find(&self, a: GuestAddress) -> Option<&GuestRegion> {
        // Few regions; linear scan is faster than a tree in practice.
        self.regions.iter().find(|r| r.contains(a))
    }

    /// Is `[addr, addr+len)` entirely backed by RAM?
    pub fn check_range(&self, addr: GuestAddress, len: u64) -> bool {
        self.walk(addr, len, |_, _, _| Ok(())).is_ok()
    }

    /// Host pointers covering `[addr, addr+len)`, split where it crosses
    /// regions (they are not contiguous on the host). For handing guest
    /// buffers to native libraries (GPU renderer). None if any byte is not
    /// guest RAM. Pointers stay valid for the life of this memory.
    pub fn host_ranges(&self, addr: GuestAddress, len: u64) -> Option<Vec<(*mut u8, usize)>> {
        let mut out = Vec::new();
        self.walk(addr, len, |r, off, chunk| {
            out.push((r.host_ptr().wrapping_add(off), chunk.len()));
            Ok(())
        })
        .ok()?;
        Some(out)
    }

    /// Walk the range chunk by chunk (it may span adjacent regions).
    fn walk<F>(&self, addr: GuestAddress, len: u64, mut f: F) -> Result<()>
    where
        F: FnMut(&GuestRegion, usize, std::ops::Range<usize>) -> Result<()>,
    {
        let err = || Error::InvalidRange(addr.0, len);
        addr.0.checked_add(len).ok_or(Error::Overflow)?;
        let mut done = 0u64;
        while done < len {
            let cur = GuestAddress(addr.0 + done);
            let r = self.find(cur).ok_or_else(err)?;
            let off = (cur.0 - r.start.0) as usize;
            let n = std::cmp::min(len - done, r.len() - off as u64) as usize;
            f(r, off, done as usize..done as usize + n)?;
            done += n as u64;
        }
        Ok(())
    }

    pub fn read(&self, addr: GuestAddress, buf: &mut [u8]) -> Result<()> {
        self.walk(addr, buf.len() as u64, |r, off, span| {
            r.mmap.read_at(off, &mut buf[span]);
            Ok(())
        })
    }

    pub fn write(&self, addr: GuestAddress, buf: &[u8]) -> Result<()> {
        self.walk(addr, buf.len() as u64, |r, off, span| {
            r.mmap.write_at(off, &buf[span]);
            Ok(())
        })
    }

    pub fn fill(&self, addr: GuestAddress, len: u64, val: u8) -> Result<()> {
        self.walk(addr, len, |r, off, span| {
            r.mmap.fill_at(off, span.len(), val);
            Ok(())
        })
    }

    pub fn read_obj<T: ByteValued>(&self, addr: GuestAddress) -> Result<T> {
        let mut v = T::default();
        self.read(addr, v.as_bytes_mut())?;
        Ok(v)
    }

    pub fn write_obj<T: ByteValued>(&self, addr: GuestAddress, v: &T) -> Result<()> {
        self.write(addr, v.as_bytes())
    }

    /// Copy from a reader straight into guest RAM (file loading, disk reads).
    pub fn read_from<R: Read>(&self, addr: GuestAddress, src: &mut R, len: u64) -> Result<()> {
        let mut buf = vec![0u8; std::cmp::min(len, 1 << 20) as usize];
        let mut done = 0u64;
        while done < len {
            let n = std::cmp::min(buf.len() as u64, len - done) as usize;
            src.read_exact(&mut buf[..n]).map_err(|e| Error::Io(e.to_string()))?;
            self.write(GuestAddress(addr.0 + done), &buf[..n])?;
            done += n as u64;
        }
        Ok(())
    }

    /// Copy guest RAM out to a writer.
    pub fn write_to<W: Write>(&self, addr: GuestAddress, dst: &mut W, len: u64) -> Result<()> {
        let mut buf = vec![0u8; std::cmp::min(len, 1 << 20) as usize];
        let mut done = 0u64;
        while done < len {
            let n = std::cmp::min(buf.len() as u64, len - done) as usize;
            self.read(GuestAddress(addr.0 + done), &mut buf[..n])?;
            dst.write_all(&buf[..n]).map_err(|e| Error::Io(e.to_string()))?;
            done += n as u64;
        }
        Ok(())
    }

    /// Host iovecs for guest buffers, after checking every byte is RAM.
    fn iovecs(&self, segs: &[(GuestAddress, usize)]) -> std::io::Result<Vec<libc::iovec>> {
        let mut v = Vec::with_capacity(segs.len());
        for &(a, len) in segs {
            self.walk(a, len as u64, |r, off, span| {
                v.push(libc::iovec { iov_base: r.host_ptr().wrapping_add(off).cast(), iov_len: span.len() });
                Ok(())
            })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        }
        Ok(v)
    }

    /// Read `file` at `offset` straight into the guest buffers `segs`, in
    /// order (disk reads without a bounce buffer: the kernel copies into
    /// guest RAM). All of it or an error; reading past the end of the file
    /// is an error.
    pub fn read_file(&self, file: &std::fs::File, offset: u64, segs: &[(GuestAddress, usize)]) -> std::io::Result<()> {
        let mut iov = self.iovecs(segs)?;
        file_io(file, offset, &mut iov, false)
    }

    /// Write the guest buffers `segs`, in order, to `file` at `offset`.
    pub fn write_file(&self, file: &std::fs::File, offset: u64, segs: &[(GuestAddress, usize)]) -> std::io::Result<()> {
        let mut iov = self.iovecs(segs)?;
        file_io(file, offset, &mut iov, true)
    }

    /// Host pointer for an aligned u16 in RAM (for atomics).
    fn aligned_u16_ptr(&self, addr: GuestAddress) -> Result<*mut u16> {
        if !addr.0.is_multiple_of(2) {
            return Err(Error::InvalidRange(addr.0, 2));
        }
        let r = self.find(addr).ok_or(Error::InvalidRange(addr.0, 2))?;
        let off = (addr.0 - r.start.0) as usize;
        if off + 2 > r.len() as usize {
            return Err(Error::InvalidRange(addr.0, 2));
        }
        // SAFETY: off+2 <= region size, checked above.
        Ok(unsafe { r.mmap.as_ptr().add(off) } as *mut u16)
    }

    /// Acquire-load a u16 (virtqueue avail idx). Pairs with guest's write barrier.
    pub fn load_u16_acquire(&self, addr: GuestAddress) -> Result<u16> {
        let p = self.aligned_u16_ptr(addr)?;
        // SAFETY: p is aligned and inside a live mapping; AtomicU16 has the
        // same layout as u16. The guest is the only other writer.
        let v = unsafe { (*(p as *const AtomicU16)).load(Ordering::Acquire) };
        Ok(u16::from_le(v))
    }

    /// Release-store a u16 (virtqueue used idx). Prior ring writes become
    /// visible to the guest before the index.
    pub fn store_u16_release(&self, addr: GuestAddress, v: u16) -> Result<()> {
        let p = self.aligned_u16_ptr(addr)?;
        // SAFETY: as above.
        unsafe { (*(p as *const AtomicU16)).store(v.to_le(), Ordering::Release) };
        Ok(())
    }

    /// Full barrier, for the "check notification suppression" dance.
    pub fn full_fence(&self) {
        fence(Ordering::SeqCst);
    }

    /// Zero all RAM and give pages back to the host (VM reset).
    pub fn discard_all(&self) -> Result<()> {
        for r in self.regions.iter() {
            r.mmap.discard(0, r.mmap.size())?;
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

/// preadv/pwritev until every iovec is done. The iovecs point into guest
/// RAM (checked by the caller); no Rust reference to it is ever made.
fn file_io(file: &std::fs::File, mut offset: u64, iov: &mut [libc::iovec], write: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const IOV_MAX: usize = 1024;
    let mut i = 0;
    while i < iov.len() {
        if iov[i].iov_len == 0 {
            i += 1;
            continue;
        }
        let n = (iov.len() - i).min(IOV_MAX);
        let off = libc::off_t::try_from(offset).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: iov[i..i+n] describe live guest RAM (bounds-checked in
        // iovecs(), mappings live as long as self); the kernel accesses
        // them, not Rust code. n <= IOV_MAX fits a c_int.
        let r = unsafe {
            if write {
                libc::pwritev(file.as_raw_fd(), iov[i..].as_ptr(), n as libc::c_int, off)
            } else {
                libc::preadv(file.as_raw_fd(), iov[i..].as_ptr(), n as libc::c_int, off)
            }
        };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let mut done = r as usize;
        offset += done as u64;
        while done > 0 {
            let v = &mut iov[i];
            let k = done.min(v.iov_len);
            v.iov_base = v.iov_base.cast::<u8>().wrapping_add(k).cast();
            v.iov_len -= k;
            done -= k;
            if v.iov_len == 0 {
                i += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_io_across_regions_and_bounds() {
        use std::io::{Seek, Write};
        let m = mem();
        let boundary = m.regions().next().unwrap().end();
        let mut f = tempfile_in_tmp();
        let data: Vec<u8> = (0..10000u32).map(|i| (i % 251) as u8).collect();
        f.write_all(&data).unwrap();
        f.rewind().unwrap();
        // Two segments, the first crossing the region boundary.
        let segs = [(GuestAddress(boundary.0 - 100), 300), (GuestAddress(boundary.0 + 1000), 5000)];
        m.read_file(&f, 7, &segs).unwrap();
        let mut b = vec![0u8; 300];
        m.read(segs[0].0, &mut b).unwrap();
        assert_eq!(b, data[7..307]);
        let mut b = vec![0u8; 5000];
        m.read(segs[1].0, &mut b).unwrap();
        assert_eq!(b, data[307..5307]);
        // And back out, somewhere else in the file.
        m.write_file(&f, 20000, &segs).unwrap();
        let mut back = vec![0u8; 5300];
        std::os::unix::fs::FileExt::read_exact_at(&f, &mut back, 20000).unwrap();
        assert_eq!(back, data[7..5307]);
        // Past the end of the file, outside RAM: errors, not crashes.
        assert!(m.read_file(&f, 30000, &segs).is_err());
        assert!(m.read_file(&f, 0, &[(m.end_addr(), 1)]).is_err());
        assert!(m.write_file(&f, 0, &[(GuestAddress(boundary.0 - 1), usize::MAX)]).is_err());
    }

    fn tempfile_in_tmp() -> std::fs::File {
        let p = std::env::temp_dir().join(format!("vroom-mem-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        f
    }

    #[test]
    #[allow(unsafe_code)]
    fn shared_ram_is_seen_by_another_mapping_and_reset_gets_a_new_object() {
        let ps = page_size() as u64;
        let m = GuestMemory::new_shared(&[(GuestAddress(0x1000_0000), 4 * ps)]).unwrap();
        let (fds, gen) = m.shared_fds().unwrap();
        assert_eq!((fds.len(), fds[0].1, gen), (1, 4 * ps, 0));
        m.write(GuestAddress(0x1000_0000 + ps), &[7u8; 16]).unwrap();
        // What another process mapping the object sees.
        let other = |fd| {
            // SAFETY: a fresh read-only shared mapping of the object.
            let p = unsafe { libc::mmap(std::ptr::null_mut(), (4 * ps) as usize, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
            assert_ne!(p, libc::MAP_FAILED);
            // SAFETY: inside the mapping just made.
            let b = unsafe { *(p as *const u8).add(ps as usize) };
            // SAFETY: unmapping it.
            unsafe { libc::munmap(p, (4 * ps) as usize) };
            b
        };
        assert_eq!(other(fds[0].0), 7);
        m.discard_all().unwrap();
        let (fds2, gen2) = m.shared_fds().unwrap();
        assert_eq!(gen2, 1);
        let mut b = [1u8; 16];
        m.read(GuestAddress(0x1000_0000 + ps), &mut b).unwrap();
        assert_eq!(b, [0; 16], "zeroed");
        m.write(GuestAddress(0x1000_0000 + ps), &[9u8; 1]).unwrap();
        assert_eq!(other(fds2[0].0), 9, "the new object is what the guest uses");
        // Private RAM has no objects to share.
        assert!(GuestMemory::new(&[(GuestAddress(0), ps)]).unwrap().shared_fds().is_none());
    }

    fn mem() -> GuestMemory {
        let ps = page_size() as u64;
        GuestMemory::new(&[(GuestAddress(0x10000 * ps / 4096), 4 * ps), (GuestAddress(0x10000 * ps / 4096 + 4 * ps), 2 * ps)])
            .unwrap()
    }

    #[test]
    fn rw_roundtrip_across_regions() {
        let m = mem();
        let start = m.regions().next().unwrap().start();
        let boundary = m.regions().next().unwrap().end();
        let at = GuestAddress(boundary.0 - 3);
        m.write(at, &[1, 2, 3, 4, 5, 6]).unwrap();
        let mut b = [0u8; 6];
        m.read(at, &mut b).unwrap();
        assert_eq!(b, [1, 2, 3, 4, 5, 6]);
        m.write_obj(start, &0xdead_beef_u32).unwrap();
        assert_eq!(m.read_obj::<u32>(start).unwrap(), 0xdead_beef);
    }

    #[test]
    fn out_of_range_rejected() {
        let m = mem();
        let end = m.end_addr();
        assert!(m.write(GuestAddress(end.0 - 1), &[0, 0]).is_err());
        assert!(m.read_obj::<u64>(GuestAddress(0)).is_err());
        assert!(m.read(GuestAddress(u64::MAX - 1), &mut [0u8; 8]).is_err());
        assert!(!m.check_range(GuestAddress(end.0), 1));
    }

    #[test]
    fn overlap_rejected() {
        let ps = page_size() as u64;
        let r = GuestMemory::new(&[(GuestAddress(0), 2 * ps), (GuestAddress(ps), ps)]);
        assert!(matches!(r, Err(Error::Overlap(_))));
    }

    #[test]
    fn atomics_and_discard() {
        let m = mem();
        let a = m.regions().next().unwrap().start();
        m.store_u16_release(a, 0x1234).unwrap();
        assert_eq!(m.load_u16_acquire(a).unwrap(), 0x1234);
        assert!(m.load_u16_acquire(GuestAddress(a.0 + 1)).is_err());
        m.discard_all().unwrap();
        assert_eq!(m.read_obj::<u16>(a).unwrap(), 0);
    }
}
