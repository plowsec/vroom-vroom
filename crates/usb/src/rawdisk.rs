//! A Mac disk as a stick's store ("Use as a disk"): the raw disk
//! (`/dev/rdiskN`) the app opened with macOS's `authopen` and passed on,
//! or, for tests, a plain file standing in for one. We never open
//! anything by name here: the fd comes ready, and is checked for what it
//! is (a character device or a regular file) and how big it is.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt};

use crate::msd::Store;

/// Biggest disk we serve: 16 TiB (more is no USB stick).
const MAX_BYTES: u64 = 16 << 40;

#[derive(Debug)]
pub struct FdStore {
    file: File,
    bs: u32,
    blocks: u64,
    readonly: bool,
    device: bool,
    gone: bool,
}

impl FdStore {
    /// `file` as a disk. Read-only if asked, or if it was opened
    /// read-only. Err: not a disk or a file, or a size we don't take.
    pub fn new(file: File, readonly: bool) -> io::Result<FdStore> {
        let meta = file.metadata()?;
        let ft = meta.file_type();
        let (bs, blocks, device) = if ft.is_file() {
            (512, meta.len() / 512, false)
        } else if ft.is_char_device() {
            let (bs, blocks) = sys::geometry(&file)?;
            (bs, blocks, true)
        } else {
            return Err(io::Error::other("not a disk"));
        };
        if !(512..=4096).contains(&bs) || !bs.is_power_of_two() {
            return Err(io::Error::other(format!("{bs}-byte blocks")));
        }
        if blocks == 0 || blocks.saturating_mul(bs as u64) > MAX_BYTES {
            return Err(io::Error::other("empty disk, or too big"));
        }
        let writable = sys::writable(&file)?;
        Ok(FdStore { file, bs, blocks, readonly: readonly || !writable, device, gone: false })
    }

    pub fn bytes(&self) -> u64 {
        self.blocks * self.bs as u64
    }

    /// The disk went away under us (unplugged from the Mac).
    fn check(&mut self, r: io::Result<()>) -> io::Result<()> {
        if let Err(e) = &r {
            if matches!(e.raw_os_error(), Some(libc::ENXIO) | Some(libc::ENODEV)) {
                self.gone = true;
            }
        }
        r
    }
}

impl Store for FdStore {
    fn block_size(&self) -> u32 {
        self.bs
    }
    fn blocks(&self) -> u64 {
        self.blocks
    }
    fn readonly(&self) -> bool {
        self.readonly
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let r = self.file.read_exact_at(buf, off);
        self.check(r)
    }
    fn write_at(&mut self, off: u64, data: &[u8]) -> io::Result<()> {
        if self.readonly {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        let r = self.file.write_all_at(data, off);
        self.check(r)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.readonly || self.gone {
            return Ok(());
        }
        let r = if self.device { sys::sync_device(&self.file) } else { self.file.sync_data() };
        self.check(r)
    }
    fn gone(&self) -> bool {
        self.gone
    }
}

#[allow(unsafe_code)]
mod sys {
    use super::*;

    /// Opened for writing?
    pub fn writable(f: &File) -> io::Result<bool> {
        // SAFETY: F_GETFL on an fd we own.
        let fl = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
        if fl < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fl & libc::O_ACCMODE != libc::O_RDONLY)
    }

    #[cfg(target_os = "macos")]
    mod dk {
        // <sys/disk.h>: _IOR('d', 24, uint32_t), _IOR('d', 25, uint64_t),
        // _IO('d', 22).
        pub const GETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
        pub const GETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;
        pub const SYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416;
    }

    /// Block size and count of a raw disk.
    #[cfg(target_os = "macos")]
    pub fn geometry(f: &File) -> io::Result<(u32, u64)> {
        let (mut bs, mut n) = (0u32, 0u64);
        // SAFETY: these ioctls write one u32 and one u64 to the pointers.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), dk::GETBLOCKSIZE, &mut bs as *mut u32) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        let r = unsafe { libc::ioctl(f.as_raw_fd(), dk::GETBLOCKCOUNT, &mut n as *mut u64) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((bs, n))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn geometry(_f: &File) -> io::Result<(u32, u64)> {
        Err(io::Error::other("raw disks are a macOS thing"))
    }

    /// Have the disk write out its cache.
    #[cfg(target_os = "macos")]
    pub fn sync_device(f: &File) -> io::Result<()> {
        // SAFETY: an ioctl without an argument on an fd we own.
        if unsafe { libc::ioctl(f.as_raw_fd(), dk::SYNCHRONIZECACHE) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    pub fn sync_device(f: &File) -> io::Result<()> {
        f.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_stands_in_for_a_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("disk.img");
        std::fs::write(&p, vec![7u8; 4096 + 100]).unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap();
        let mut s = FdStore::new(f, false).unwrap();
        assert_eq!((s.block_size(), s.blocks(), s.readonly()), (512, 8, false), "the odd tail is no block");
        s.write_at(512, &[1; 512]).unwrap();
        s.flush().unwrap();
        let mut b = vec![0; 1024];
        s.read_at(0, &mut b).unwrap();
        assert!(b[..512].iter().all(|&x| x == 7) && b[512..].iter().all(|&x| x == 1));
        // Opened read-only: write protected, whatever was asked.
        let ro = FdStore::new(File::open(&p).unwrap(), false).unwrap();
        assert!(ro.readonly());
        let mut ro = FdStore::new(std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap(), true).unwrap();
        assert!(ro.write_at(0, &[0; 512]).is_err());
        // Not disks.
        assert!(FdStore::new(File::open(dir.path()).unwrap(), false).is_err(), "a directory");
        std::fs::write(dir.path().join("tiny"), b"abc").unwrap();
        assert!(FdStore::new(File::open(dir.path().join("tiny")).unwrap(), false).is_err(), "no whole block");
    }
}
