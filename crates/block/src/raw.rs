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
}

#[cfg(test)]
mod tests {
    use super::*;

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
