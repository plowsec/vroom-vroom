//! Throwaway write layer for snapshot fuzzing.
//!
//! [`RollbackDisk`] sits on top of a disk and keeps every write in memory,
//! in `BLOCK`-sized pieces; the disk below is never written. [`rollback`]
//! forgets them, so the guest sees the disk as it was when the layer went
//! on, the way an in-place RAM reset puts its page cache back.
//!
//! [`rollback`]: RollbackDisk::rollback

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::{check_range, gather, scatter, segs_len, DiskImage, Error, GuestMemory, GuestSegs, Result};

/// Granularity of the write log (a host page; guests write 4 KiB pieces).
const BLOCK: u64 = 4096;

#[derive(Debug)]
pub struct RollbackDisk {
    lower: Arc<dyn DiskImage>,
    /// Block index -> its contents since the layer went on.
    written: RwLock<HashMap<u64, Box<[u8]>>>,
}

impl RollbackDisk {
    pub fn new(lower: Arc<dyn DiskImage>) -> Self {
        RollbackDisk { lower, written: RwLock::default() }
    }

    /// The disk the writes would have gone to.
    pub fn lower(&self) -> &Arc<dyn DiskImage> {
        &self.lower
    }

    /// Forget every write. No request may be running.
    pub fn rollback(&self) {
        self.written.write().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Memory the writes since the last rollback take.
    pub fn held_bytes(&self) -> u64 {
        self.written.read().unwrap_or_else(|e| e.into_inner()).len() as u64 * BLOCK
    }

    /// Blocks `[offset, offset+len)` touches, each with the part of it.
    fn pieces(offset: u64, len: u64) -> impl Iterator<Item = (u64, usize, usize)> {
        let mut at = offset;
        let end = offset + len;
        std::iter::from_fn(move || {
            if at >= end {
                return None;
            }
            let (b, o) = (at / BLOCK, at % BLOCK);
            let n = (BLOCK - o).min(end - at);
            at += n;
            Some((b, o as usize, n as usize))
        })
    }

    /// Change part of the disk, copying a block up from below the first
    /// time only part of it is written.
    fn update(&self, offset: u64, len: u64, mut fill: impl FnMut(&mut [u8], usize)) -> Result<()> {
        if self.lower.read_only() {
            return Err(Error::ReadOnly);
        }
        check_range(self.size(), offset, len)?;
        let mut w = self.written.write().unwrap_or_else(|e| e.into_inner());
        let mut done = 0;
        for (b, o, n) in Self::pieces(offset, len) {
            let blk = match w.entry(b) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let start = b * BLOCK;
                    let mut data = vec![0u8; (self.size() - start).min(BLOCK) as usize];
                    if n < data.len() {
                        self.lower.read_at(&mut data, start)?;
                    }
                    e.insert(data.into_boxed_slice())
                }
            };
            fill(&mut blk[o..o + n], done);
            done += n;
        }
        Ok(())
    }
}

impl DiskImage for RollbackDisk {
    fn size(&self) -> u64 {
        self.lower.size()
    }

    fn read_only(&self) -> bool {
        self.lower.read_only()
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        check_range(self.size(), offset, buf.len() as u64)?;
        let w = self.written.read().unwrap_or_else(|e| e.into_inner());
        if w.is_empty() {
            return self.lower.read_at(buf, offset);
        }
        // Runs of blocks nobody wrote come from below in one read.
        let mut run: Option<(usize, u64)> = None;
        let mut done = 0;
        for (b, o, n) in Self::pieces(offset, buf.len() as u64) {
            match w.get(&b) {
                Some(blk) => {
                    if let Some((at, off)) = run.take() {
                        self.lower.read_at(&mut buf[at..done], off)?;
                    }
                    buf[done..done + n].copy_from_slice(&blk[o..o + n]);
                }
                None => {
                    run.get_or_insert((done, b * BLOCK + o as u64));
                }
            }
            done += n;
        }
        if let Some((at, off)) = run {
            self.lower.read_at(&mut buf[at..done], off)?;
        }
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        self.update(offset, buf.len() as u64, |dst, at| dst.copy_from_slice(&buf[at..at + dst.len()]))
    }

    /// Nothing to push down: the writes only ever live here.
    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn read_to_guest(&self, mem: &GuestMemory, offset: u64, segs: &GuestSegs) -> Result<()> {
        let len = segs_len(segs)? as u64;
        check_range(self.size(), offset, len)?;
        {
            let w = self.written.read().unwrap_or_else(|e| e.into_inner());
            if !Self::pieces(offset, len).any(|(b, _, _)| w.contains_key(&b)) {
                // Untouched: let the disk below skip the bounce buffer.
                return self.lower.read_to_guest(mem, offset, segs);
            }
        }
        let mut buf = vec![0u8; len as usize];
        self.read_at(&mut buf, offset)?;
        scatter(mem, segs, &buf)
    }

    fn write_from_guest(&self, mem: &GuestMemory, offset: u64, segs: &GuestSegs) -> Result<()> {
        let buf = gather(mem, segs)?;
        self.write_at(&buf, offset)
    }

    /// Discarded data may read back as anything: keep what is there.
    fn discard(&self, _offset: u64, _len: u64) -> Result<()> {
        Ok(())
    }

    fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        self.update(offset, len, |dst, _| dst.fill(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawImage;
    use std::io::Write;

    fn disk(size: usize) -> (tempfile::NamedTempFile, Arc<dyn DiskImage>) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        f.write_all(&data).unwrap();
        let img = RawImage::open(f.path(), false).unwrap();
        (f, Arc::new(img))
    }

    fn read(d: &dyn DiskImage, off: u64, len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        d.read_at(&mut b, off).unwrap();
        b
    }

    #[test]
    fn writes_stay_above_and_roll_back() {
        // Not a multiple of BLOCK: the last block is short.
        let size = 5 * BLOCK as usize + 1000;
        let (_f, lower) = disk(size);
        let before = read(&*lower, 0, size);
        let rb = RollbackDisk::new(lower.clone());

        // Unaligned, across blocks, into the short last block.
        rb.write_at(&[0xaa; 5000], 3000).unwrap();
        rb.write_zeroes(5 * BLOCK + 10, 990).unwrap();
        let mut want = before.clone();
        want[3000..8000].fill(0xaa);
        want[5 * BLOCK as usize + 10..].fill(0);
        assert_eq!(read(&rb, 0, size), want);
        // Pieces read alone mix written and untouched blocks right.
        assert_eq!(read(&rb, 2000, 9000), want[2000..11000]);
        assert_eq!(read(&*lower, 0, size), before, "the disk below was written");
        assert_eq!(rb.held_bytes(), 3 * BLOCK);
        assert!(rb.write_at(&[0], size as u64).is_err());

        rb.rollback();
        assert_eq!(read(&rb, 0, size), before);
        assert_eq!(rb.held_bytes(), 0);
    }

    #[test]
    fn guest_buffers_go_through_the_layer() {
        let size = 4 * BLOCK as usize;
        let (_f, lower) = disk(size);
        let before = read(&*lower, 0, size);
        let rb = RollbackDisk::new(lower);
        let mem = GuestMemory::new(&[(crate::GuestAddress(0), 1 << 16)]).unwrap();
        mem.write(crate::GuestAddress(0x100), &[0x55; 600]).unwrap();
        rb.write_from_guest(&mem, 512, &[(crate::GuestAddress(0x100), 600)]).unwrap();

        let segs = [(crate::GuestAddress(0x2000), 1000), (crate::GuestAddress(0x4000), 2 * BLOCK as usize)];
        rb.read_to_guest(&mem, 0, &segs).unwrap();
        let mut got = vec![0u8; 1000 + 2 * BLOCK as usize];
        mem.read(crate::GuestAddress(0x2000), &mut got[..1000]).unwrap();
        mem.read(crate::GuestAddress(0x4000), &mut got[1000..]).unwrap();
        let mut want = before[..got.len()].to_vec();
        want[512..1112].fill(0x55);
        assert_eq!(got, want);

        rb.rollback();
        rb.read_to_guest(&mem, 0, &segs).unwrap();
        mem.read(crate::GuestAddress(0x2000), &mut got[..1000]).unwrap();
        assert_eq!(got[..1000], before[..1000]);
    }
}
