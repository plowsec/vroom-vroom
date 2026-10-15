//! Raw disk image: byte N of the disk is byte N of the file.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::{check_range, open_file, DiskImage, Error, Result, SyncMode};

#[derive(Debug)]
pub struct RawImage {
    file: File,
    size: u64,
    read_only: bool,
    sync: SyncMode,
}

impl RawImage {
    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let file = open_file(path, read_only)?;
        Self::from_file(file, read_only)
    }

    pub fn from_file(file: File, read_only: bool) -> Result<Self> {
        let size = file.metadata()?.len();
        Ok(RawImage { file, size, read_only, sync: SyncMode::default() })
    }

    pub fn with_sync(mut self, sync: SyncMode) -> Self {
        self.sync = sync;
        self
    }
}

impl DiskImage for RawImage {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        check_range(self.size, offset, buf.len() as u64)?;
        self.file.read_exact_at(buf, offset)?;
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_range(self.size, offset, buf.len() as u64)?;
        self.file.write_all_at(buf, offset)?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        if !self.read_only {
            crate::sync_file(&self.file, self.sync)?;
        }
        Ok(())
    }

    fn freeze(&self) {
        let _ = crate::lock_file(&self.file, false);
    }

    /// Give the blocks back to the host (sparse file).
    fn discard(&self, offset: u64, len: u64) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_range(self.size, offset, len)?;
        let (s, e) = crate::aligned_middle(offset, len);
        // Advisory: a file system without hole support keeps the data.
        let _ = crate::punch_hole(&self.file, s, e - s);
        Ok(())
    }

    fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_range(self.size, offset, len)?;
        let (s, e) = crate::aligned_middle(offset, len);
        if e > s && crate::punch_hole(&self.file, s, e - s).is_ok() {
            self.zero_fill(offset, s - offset)?;
            return self.zero_fill(e, offset + len - e);
        }
        self.zero_fill(offset, len)
    }
}

impl RawImage {
    fn zero_fill(&self, mut off: u64, mut len: u64) -> Result<()> {
        let z = vec![0u8; len.min(1 << 20) as usize];
        while len > 0 {
            let n = len.min(z.len() as u64) as usize;
            self.file.write_all_at(&z[..n], off)?;
            off += n as u64;
            len -= n as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discard_punches_holes() {
        use std::os::unix::fs::MetadataExt;
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("r.raw");
        std::fs::write(&p, vec![3u8; 1 << 20]).unwrap();
        let d = RawImage::open(&p, false).unwrap();
        let before = std::fs::metadata(&p).unwrap().blocks();
        d.discard(100, (1 << 20) - 200).unwrap();
        d.write_zeroes(0, 50).unwrap();
        let mut b = vec![0u8; 1 << 20];
        d.read_at(&mut b, 0).unwrap();
        assert!(b[..50].iter().all(|&x| x == 0));
        assert!(b[50..100].iter().all(|&x| x == 3)); // partial block kept
        assert!(b[4096..(1 << 20) - 4096].iter().all(|&x| x == 0));
        assert!(std::fs::metadata(&p).unwrap().blocks() < before);
    }

    #[test]
    fn rw_and_bounds() {
        let f = tempfile::tempfile().unwrap();
        f.set_len(4096).unwrap();
        let d = RawImage::from_file(f, false).unwrap();
        d.write_at(b"hello", 4091).unwrap();
        let mut b = [0u8; 5];
        d.read_at(&mut b, 4091).unwrap();
        assert_eq!(&b, b"hello");
        assert!(d.write_at(b"x", 4096).is_err());
        assert!(d.read_at(&mut b, u64::MAX - 1).is_err());
    }
}
