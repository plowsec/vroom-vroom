//! Disk images.
//!
//! [`DiskImage`] is what devices see. Formats:
//! - [`raw::RawImage`]: plain file.
//! - [`qcow2::Qcow2Image`]: sparse, copy-on-write over a backing image.
//!   Disk snapshots = freeze current image, write to a new overlay.
//! - [`vmdk::VmdkImage`]: VMware disks, read-only (for importing).
//! - `rollback::RollbackDisk` (feature `research`): writes kept in memory
//!   and thrown away on demand (snapshot fuzzing).
// One audited exception: plain fsync(2) in `sync_file`.
#![deny(unsafe_code)]

pub mod qcow2;
pub mod raw;
#[cfg(feature = "research")]
pub mod rollback;
pub mod vmdk;

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::Arc;

pub use vroom_memory::{GuestAddress, GuestMemory};

pub const SECTOR_SIZE: u64 = 512;

/// Guest buffers of one request, in order: (address, length).
pub type GuestSegs = [(GuestAddress, usize)];

/// Biggest request the bounce-buffer fallbacks accept (devices cap
/// requests lower than this already).
pub const MAX_BOUNCE: usize = 16 << 20;

fn segs_len(segs: &GuestSegs) -> Result<usize> {
    segs.iter()
        .try_fold(0usize, |a, &(_, l)| a.checked_add(l))
        .filter(|&n| n <= MAX_BOUNCE)
        .ok_or(Error::OutOfRange)
}

fn mem_err(e: vroom_memory::Error) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
}

/// The part `[start, start + len)` of the concatenated buffers.
pub(crate) fn sub_segs(segs: &GuestSegs, mut start: usize, mut len: usize) -> Vec<(GuestAddress, usize)> {
    let mut out = Vec::new();
    for &(a, l) in segs {
        if len == 0 {
            break;
        }
        if start >= l {
            start -= l;
            continue;
        }
        let n = (l - start).min(len);
        out.push((GuestAddress(a.0 + start as u64), n));
        start = 0;
        len -= n;
    }
    out
}

/// Copy guest buffers out (bounce-buffer fallback).
pub(crate) fn gather(mem: &GuestMemory, segs: &GuestSegs) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; segs_len(segs)?];
    let mut o = 0;
    for &(a, l) in segs {
        mem.read(a, &mut buf[o..o + l]).map_err(mem_err)?;
        o += l;
    }
    Ok(buf)
}

/// Copy into guest buffers (bounce-buffer fallback).
pub(crate) fn scatter(mem: &GuestMemory, segs: &GuestSegs, buf: &[u8]) -> Result<()> {
    let mut o = 0;
    for &(a, l) in segs {
        let end = o + l;
        mem.write(a, buf.get(o..end).ok_or(Error::OutOfRange)?).map_err(mem_err)?;
        o = end;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    #[error("bad image: {0}")]
    Format(String),
    #[error("access out of range")]
    OutOfRange,
    #[error("image is read-only")]
    ReadOnly,
}

pub type Result<T> = std::result::Result<T, Error>;

/// A virtual disk. Thread-safe; offsets in bytes.
pub trait DiskImage: Send + Sync + std::fmt::Debug {
    fn size(&self) -> u64;
    fn read_only(&self) -> bool;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()>;
    fn flush(&self) -> Result<()>;
    /// Read `offset..` straight into guest buffers. Formats that can
    /// override this to skip the bounce buffer (raw, plain qcow2 clusters).
    fn read_to_guest(&self, mem: &GuestMemory, offset: u64, segs: &GuestSegs) -> Result<()> {
        let mut buf = vec![0u8; segs_len(segs)?];
        self.read_at(&mut buf, offset)?;
        scatter(mem, segs, &buf)
    }
    /// Write guest buffers at `offset`.
    fn write_from_guest(&self, mem: &GuestMemory, offset: u64, segs: &GuestSegs) -> Result<()> {
        let buf = gather(mem, segs)?;
        self.write_at(&buf, offset)
    }
    /// Hint that a range is unused. May be a no-op.
    fn discard(&self, _offset: u64, _len: u64) -> Result<()> {
        Ok(())
    }
    /// This image just became a read-only layer under a new overlay
    /// (snapshot): let others open it for reading.
    fn freeze(&self) {}
    /// Write zeroes (default: plain writes).
    fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        let zero = vec![0u8; 64 << 10];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(zero.len() as u64) as usize;
            self.write_at(&zero[..n], offset + done)?;
            done += n as u64;
        }
        Ok(())
    }
}

/// Check `[offset, offset+len)` lies inside a disk of `size` bytes.
pub fn check_range(size: u64, offset: u64, len: u64) -> Result<()> {
    match offset.checked_add(len) {
        Some(end) if end <= size => Ok(()),
        _ => Err(Error::OutOfRange),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Raw,
    Qcow2,
}

/// Sniff the format from the first bytes.
pub fn detect(path: &Path) -> Result<Format> {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let mut f = File::open(path)?;
    let n = f.read(&mut magic)?;
    Ok(if n == 4 && magic == qcow2::MAGIC.to_be_bytes() { Format::Qcow2 } else { Format::Raw })
}

/// Open a disk image. `format = None` means auto-detect.
///
/// Auto-detection on a writable image is a classic hole (a guest writes a
/// qcow2 header into a raw disk, then on next boot points the "backing
/// file" at host files). So: auto-detect only for read-only opens, or when
/// the caller already pinned the format.
/// What a guest flush does on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Through the drive's own cache (F_FULLFSYNC on macOS). Survives
    /// power loss. Slow on macOS: milliseconds per flush.
    Full,
    /// fsync(2). Survives a host crash; on macOS maybe not a power cut.
    #[default]
    Fsync,
    /// Ignore guest flushes. Scratch disks only.
    None,
}

pub(crate) fn sync_file(f: &std::fs::File, mode: SyncMode) -> std::io::Result<()> {
    match mode {
        SyncMode::Full => f.sync_all(),
        SyncMode::Fsync => fsync(f),
        SyncMode::None => Ok(()),
    }
}

#[allow(unsafe_code)]
fn fsync(f: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fsync on an fd we own for the duration of the call.
    if unsafe { libc::fsync(f.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Unit for hole punching (host file system blocks).
pub(crate) const HOLE_ALIGN: u64 = 4096;

/// Free the host blocks under `[off, off+len)`; they read back as zeros.
/// Both ends must be `HOLE_ALIGN`-aligned. Errors if the file system
/// can't (callers fall back or ignore).
#[allow(unsafe_code)]
pub(crate) fn punch_hole(f: &std::fs::File, off: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    if len == 0 {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let arg = libc::fpunchhole_t { fp_flags: 0, reserved: 0, fp_offset: off as libc::off_t, fp_length: len as libc::off_t };
        // SAFETY: valid fd and argument struct for F_PUNCHHOLE.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PUNCHHOLE, &arg) } == 0 {
            return Ok(());
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        // SAFETY: valid fd; plain integer arguments.
        if unsafe { libc::fallocate(f.as_raw_fd(), mode, off as libc::off_t, len as libc::off_t) } == 0 {
            return Ok(());
        }
    }
    Err(std::io::Error::last_os_error())
}

/// The `HOLE_ALIGN`-aligned middle of a range: (start, end), maybe empty.
pub(crate) fn aligned_middle(off: u64, len: u64) -> (u64, u64) {
    let start = off.div_ceil(HOLE_ALIGN) * HOLE_ALIGN;
    let end = (off + len) / HOLE_ALIGN * HOLE_ALIGN;
    (start, end.max(start))
}

pub fn open(path: &Path, format: Option<Format>, read_only: bool) -> Result<Arc<dyn DiskImage>> {
    open_with(path, format, read_only, &qcow2::OpenPolicy::default())
}

/// Like [`open`], with extra directories backing files may come from.
pub fn open_with(path: &Path, format: Option<Format>, read_only: bool, policy: &qcow2::OpenPolicy) -> Result<Arc<dyn DiskImage>> {
    let fmt = match format {
        Some(f) => f,
        None if read_only => detect(path)?,
        None => {
            let f = detect(path)?;
            if f == Format::Qcow2 {
                return Err(Error::Format(format!(
                    "{}: looks like qcow2; set format = \"qcow2\" explicitly",
                    path.display()
                )));
            }
            f
        }
    };
    Ok(match fmt {
        Format::Raw => Arc::new(raw::RawImage::open(path, read_only)?.with_sync(policy.sync)),
        Format::Qcow2 => Arc::new(qcow2::Qcow2Image::open_with(path, read_only, policy, 0)?),
    })
}

/// A disk whose backing image can be replaced while the VM is paused
/// (snapshot layering). Devices hold this; size never changes.
#[derive(Debug)]
pub struct SwapDisk {
    inner: std::sync::RwLock<Arc<dyn DiskImage>>,
}

impl SwapDisk {
    pub fn new(d: Arc<dyn DiskImage>) -> Arc<Self> {
        Arc::new(SwapDisk { inner: std::sync::RwLock::new(d) })
    }

    pub fn current(&self) -> Arc<dyn DiskImage> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Swap in a new top layer of the same size.
    pub fn swap(&self, d: Arc<dyn DiskImage>) -> Result<()> {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if d.size() != g.size() {
            return Err(Error::Format("replacement disk has a different size".into()));
        }
        *g = d;
        Ok(())
    }
}

impl DiskImage for SwapDisk {
    fn size(&self) -> u64 {
        self.current().size()
    }
    fn read_only(&self) -> bool {
        self.current().read_only()
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.current().read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        self.current().write_at(buf, offset)
    }
    fn flush(&self) -> Result<()> {
        self.current().flush()
    }
    fn read_to_guest(&self, mem: &GuestMemory, offset: u64, segs: &GuestSegs) -> Result<()> {
        self.current().read_to_guest(mem, offset, segs)
    }
    fn write_from_guest(&self, mem: &GuestMemory, offset: u64, segs: &GuestSegs) -> Result<()> {
        self.current().write_from_guest(mem, offset, segs)
    }
    fn discard(&self, offset: u64, len: u64) -> Result<()> {
        self.current().discard(offset, len)
    }
    fn freeze(&self) {
        self.current().freeze()
    }
    fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        self.current().write_zeroes(offset, len)
    }
}

/// Open an image file and lock it: exclusive for writing, shared for
/// reading. Two VMs writing one image, or one writing a file another uses
/// as a backing layer, corrupt it; the second open fails instead.
pub(crate) fn open_file(path: &Path, read_only: bool) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(!read_only);
    let f = o.open(path)?;
    // A lock can outlive its owner by a moment: a child forked by another
    // thread holds copies of our fds until it execs. Retry briefly.
    let mut tries = 0;
    loop {
        match lock_file(&f, !read_only) {
            Ok(()) => return Ok(f),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(e.kind(), format!("{} is in use by another VM", path.display())))
            }
            Err(e) => return Err(e),
        }
    }
}

/// Take, or convert to, a shared or exclusive lock on the whole file.
#[allow(unsafe_code)]
pub(crate) fn lock_file(f: &File, exclusive: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let op = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
    // SAFETY: flock on an fd we own; no memory involved.
    if unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether a lone request runs on the vCPU that submitted it (no thread
/// wakeups: the lowest latency at queue depth 1) or goes to I/O threads
/// (several requests overlap: needed at higher depths). While requests
/// run inline each one ends before the guest can submit the next, so the
/// guest's depth can't be seen: every `PROBE_EVERY`th lone request goes
/// to the threads anyway, and if the guest submits more while it runs, we
/// stay on the threads until it goes back to one request at a time.
#[derive(Debug, Default)]
pub struct InlinePolicy {
    threaded: bool,
    count: u32,
}

impl InlinePolicy {
    const PROBE_EVERY: u32 = 64;
    const BACK_TO_INLINE: u32 = 32;

    /// The guest keeps several requests in flight (for now).
    pub fn threaded(&self) -> bool {
        self.threaded
    }

    /// A request arrives. `others`: requests are running or came with it.
    /// True: run it inline.
    pub fn run_inline(&mut self, others: bool) -> bool {
        if others {
            self.threaded = true;
            self.count = 0;
            return false;
        }
        self.count += 1;
        if self.threaded {
            if self.count >= Self::BACK_TO_INLINE {
                self.threaded = false;
                self.count = 0;
            }
            return false;
        }
        if self.count >= Self::PROBE_EVERY {
            self.count = 0;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod policy_tests {
    use super::InlinePolicy;

    #[test]
    fn inline_until_the_guest_overlaps_requests() {
        let mut p = InlinePolicy::default();
        // One at a time: inline, but for a probe now and then.
        let inline = (0..128).filter(|_| p.run_inline(false)).count();
        assert_eq!(inline, 126);
        // The guest submitted more while one ran: threads from now on...
        assert!(!p.run_inline(true));
        assert!((0..31).all(|_| !p.run_inline(false)));
        // ...until it has been one at a time for a while.
        assert!(!p.run_inline(false));
        assert!(p.run_inline(false));
    }
}
