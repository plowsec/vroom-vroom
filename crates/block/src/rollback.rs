//! Throwaway write layer for snapshot fuzzing.
//!
//! [`RollbackDisk`] sits on top of a disk and keeps every write, in
//! `BLOCK`-sized pieces; the disk below is never written. Blocks live in
//! memory up to a limit, then in an unlinked scratch file. [`rollback`]
//! forgets them, so the guest sees the disk as it was when the layer went
//! on, the way an in-place RAM reset puts its page cache back.
//!
//! [`rollback`]: RollbackDisk::rollback

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::{check_range, gather, scatter, segs_len, DiskImage, Error, GuestMemory, GuestSegs, Result};

/// Granularity of the write log (a host page; guests write 4 KiB pieces).
const BLOCK: u64 = 4096;

/// Where a written block's contents are.
#[derive(Debug)]
enum Held {
    Memory(Box<[u8]>),
    /// In the scratch file, at the block's own disk offset (sparse).
    Spilled,
}

#[derive(Debug, Default)]
struct Written {
    blocks: HashMap<u64, Held>,
    /// Bytes of `Held::Memory` blocks.
    memory: u64,
    /// Created on the first block past the memory limit.
    spill: Option<File>,
}

#[derive(Debug)]
pub struct RollbackDisk {
    lower: Arc<dyn DiskImage>,
    written: RwLock<Written>,
    /// Memory the written blocks may take before they go to `spill_dir`.
    memory_limit: u64,
    spill_dir: PathBuf,
}

impl RollbackDisk {
    /// Writes beyond `memory_limit` bytes go to a scratch file in
    /// `spill_dir`, removed as soon as it is open.
    pub fn new(lower: Arc<dyn DiskImage>, spill_dir: PathBuf, memory_limit: u64) -> Self {
        RollbackDisk { lower, written: RwLock::default(), memory_limit, spill_dir }
    }

    /// The disk the writes would have gone to.
    pub fn lower(&self) -> &Arc<dyn DiskImage> {
        &self.lower
    }

    /// Forget every write. No request may be running.
    pub fn rollback(&self) -> Result<()> {
        let mut w = self.written.write().unwrap_or_else(|e| e.into_inner());
        w.blocks.clear();
        w.memory = 0;
        if let Some(f) = &w.spill {
            // Give the spilled blocks' disk space back.
            f.set_len(0)?;
        }
        Ok(())
    }

    /// Bytes the writes since the last rollback take (memory and file).
    pub fn held_bytes(&self) -> u64 {
        self.written.read().unwrap_or_else(|e| e.into_inner()).blocks.len() as u64 * BLOCK
    }

    /// Bytes of them in memory.
    pub fn memory_bytes(&self) -> u64 {
        self.written.read().unwrap_or_else(|e| e.into_inner()).memory
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

    fn block_len(&self, b: u64) -> usize {
        (self.size() - b * BLOCK).min(BLOCK) as usize
    }

    fn spill_file(&self, w: &mut Written) -> Result<File> {
        if let Some(f) = &w.spill {
            return Ok(f.try_clone()?);
        }
        let path = self.spill_dir.join(format!("rollback-{}-{:p}.tmp", std::process::id(), self));
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        // Unlinked: it goes away with the last handle, even on a crash.
        std::fs::remove_file(&path)?;
        let clone = f.try_clone()?;
        w.spill = Some(f);
        Ok(clone)
    }

    /// Change part of the disk, copying a block up from below the first
    /// time only part of it is written.
    fn update(&self, offset: u64, len: u64, mut fill: impl FnMut(&mut [u8], usize)) -> Result<()> {
        if self.lower.read_only() {
            return Err(Error::ReadOnly);
        }
        check_range(self.size(), offset, len)?;
        let mut guard = self.written.write().unwrap_or_else(|e| e.into_inner());
        let w = &mut *guard;
        let mut done = 0;
        for (b, o, n) in Self::pieces(offset, len) {
            let size = self.block_len(b);
            match w.blocks.get_mut(&b) {
                Some(Held::Memory(blk)) => fill(&mut blk[o..o + n], done),
                Some(Held::Spilled) => {
                    let f = w.spill.as_ref().expect("spilled blocks have a file");
                    let mut data = vec![0u8; size];
                    if n < size {
                        f.read_exact_at(&mut data, b * BLOCK)?;
                    }
                    fill(&mut data[o..o + n], done);
                    f.write_all_at(&data, b * BLOCK)?;
                }
                None => {
                    let mut data = vec![0u8; size];
                    if n < size {
                        self.lower.read_at(&mut data, b * BLOCK)?;
                    }
                    fill(&mut data[o..o + n], done);
                    if w.memory + size as u64 <= self.memory_limit {
                        w.memory += size as u64;
                        w.blocks.insert(b, Held::Memory(data.into_boxed_slice()));
                    } else {
                        self.spill_file(w)?.write_all_at(&data, b * BLOCK)?;
                        w.blocks.insert(b, Held::Spilled);
                    }
                }
            }
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
        if w.blocks.is_empty() {
            return self.lower.read_at(buf, offset);
        }
        // Runs of blocks nobody wrote come from below in one read.
        let mut run: Option<(usize, u64)> = None;
        let mut done = 0;
        for (b, o, n) in Self::pieces(offset, buf.len() as u64) {
            match w.blocks.get(&b) {
                Some(held) => {
                    if let Some((at, off)) = run.take() {
                        self.lower.read_at(&mut buf[at..done], off)?;
                    }
                    match held {
                        Held::Memory(blk) => buf[done..done + n].copy_from_slice(&blk[o..o + n]),
                        Held::Spilled => w
                            .spill
                            .as_ref()
                            .expect("spilled blocks have a file")
                            .read_exact_at(&mut buf[done..done + n], b * BLOCK + o as u64)?,
                    }
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
            if !Self::pieces(offset, len).any(|(b, _, _)| w.blocks.contains_key(&b)) {
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
        let rb = RollbackDisk::new(lower.clone(), std::env::temp_dir(), u64::MAX);

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

        rb.rollback().unwrap();
        assert_eq!(read(&rb, 0, size), before);
        assert_eq!(rb.held_bytes(), 0);
    }

    #[test]
    fn guest_buffers_go_through_the_layer() {
        let size = 4 * BLOCK as usize;
        let (_f, lower) = disk(size);
        let before = read(&*lower, 0, size);
        let rb = RollbackDisk::new(lower, std::env::temp_dir(), u64::MAX);
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

        rb.rollback().unwrap();
        rb.read_to_guest(&mem, 0, &segs).unwrap();
        mem.read(crate::GuestAddress(0x2000), &mut got[..1000]).unwrap();
        assert_eq!(got[..1000], before[..1000]);
    }

    #[test]
    fn blocks_past_the_memory_limit_go_to_a_file() {
        let size = 8 * BLOCK as usize;
        let (_f, lower) = disk(size);
        let before = read(&*lower, 0, size);
        let dir = tempfile::tempdir().unwrap();
        // Two blocks in memory, the rest spilled.
        let rb = RollbackDisk::new(lower.clone(), dir.path().to_path_buf(), 2 * BLOCK);
        for tc in [0x11u8, 0x22] {
            rb.write_at(&vec![tc; 5 * BLOCK as usize], 100).unwrap();
            // Partly rewrite a spilled block, and one block below.
            rb.write_at(&[tc ^ 0xff; 10], 4 * BLOCK + 7).unwrap();
            rb.write_zeroes(7 * BLOCK + 5, 20).unwrap();
            let mut want = before.clone();
            want[100..100 + 5 * BLOCK as usize].fill(tc);
            want[4 * BLOCK as usize + 7..4 * BLOCK as usize + 17].fill(tc ^ 0xff);
            want[7 * BLOCK as usize + 5..7 * BLOCK as usize + 25].fill(0);
            assert_eq!(read(&rb, 0, size), want);
            assert_eq!(read(&rb, 3 * BLOCK + 50, 3000), want[3 * BLOCK as usize + 50..3 * BLOCK as usize + 3050]);
            assert_eq!((rb.held_bytes(), rb.memory_bytes()), (7 * BLOCK, 2 * BLOCK));
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "the scratch file is unlinked");
            rb.rollback().unwrap();
            assert_eq!(read(&rb, 0, size), before);
            assert_eq!((rb.held_bytes(), rb.memory_bytes()), (0, 0));
        }
        assert_eq!(read(&*lower, 0, size), before, "the disk below was written");
    }
}
