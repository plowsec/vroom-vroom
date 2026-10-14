//! Disk images.
//!
//! [`DiskImage`] is what devices see. Formats:
//! - [`raw::RawImage`]: plain file.
//! - [`qcow2::Qcow2Image`]: sparse, copy-on-write over a backing image.
//!   Disk snapshots = freeze current image, write to a new overlay.
#![forbid(unsafe_code)]

pub mod qcow2;
pub mod raw;

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::Arc;

pub const SECTOR_SIZE: u64 = 512;

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
    /// Hint that a range is unused. May be a no-op.
    fn discard(&self, _offset: u64, _len: u64) -> Result<()> {
        Ok(())
    }
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
        Format::Raw => Arc::new(raw::RawImage::open(path, read_only)?),
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
    fn discard(&self, offset: u64, len: u64) -> Result<()> {
        self.current().discard(offset, len)
    }
    fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        self.current().write_zeroes(offset, len)
    }
}

pub(crate) fn open_file(path: &Path, read_only: bool) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(!read_only);
    o.open(path)
}
