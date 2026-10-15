//! Intel/Sharp CFI (cmd set 0001) NOR flash, as on QEMU's virt board:
//! 32-bit bank made of two interleaved 16-bit chips, 256 KiB blocks.
//! Used for the UEFI variable store; contents persist to a file.

use std::fs::File;
use std::os::unix::fs::FileExt;

use vroom_snapshot::{decode, encode, Snapshottable};
use serde::{Deserialize, Serialize};

use crate::{get_le, put_le, BusDevice};

pub const BLOCK_SIZE: u64 = 256 << 10;
const STATUS_READY: u32 = 0x0080_0080;
/// Max words in one buffered program (per chip).
const MAX_BUF_WORDS: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Mode {
    ReadArray,
    Status,
    Id,
    Cfi,
    /// Next write is the data word.
    WordProgram,
    /// 0x20 seen; waiting for 0xD0 at the block address.
    EraseSetup,
    /// 0x60 seen; next write is lock/unlock confirm.
    LockSetup,
    /// 0xE8 seen; next write is the word count.
    BufCount { block: u64 },
    /// Collecting `left` more words into the write buffer.
    BufData { left: u32 },
    /// Data collected; waiting for 0xD0.
    BufConfirm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    mode: Mode,
    buf: Vec<(u64, u32)>,
}

pub struct PFlash {
    base: u64,
    data: Vec<u8>,
    file: Option<File>,
    st: State,
}

impl std::fmt::Debug for PFlash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PFlash").field("base", &self.base).field("size", &self.data.len()).finish()
    }
}

/// CFI query table (per chip, QEMU's values for this geometry).
fn cfi_byte(i: u64, total: u64) -> u8 {
    let chip_size = total / 2;
    let blocks = (total / BLOCK_SIZE) as u32;
    let blk_per_chip = (BLOCK_SIZE / 2) as u32;
    match i {
        0x10 => b'Q',
        0x11 => b'R',
        0x12 => b'Y',
        0x13 => 0x01, // primary cmd set 0001
        0x15 => 0x31,
        0x1b => 0x45,
        0x1c => 0x55,
        0x1f => 0x07,
        0x20 => 0x07,
        0x21 => 0x0a,
        0x23 => 0x04,
        0x24 => 0x04,
        0x25 => 0x04,
        0x27 => chip_size.trailing_zeros() as u8,
        0x28 => 0x02,
        0x2a => 0x0b, // 2^11 = 2048-byte write buffer
        0x2c => 0x01, // one erase region
        0x2d => (blocks - 1) as u8,
        0x2e => ((blocks - 1) >> 8) as u8,
        0x2f => (blk_per_chip >> 8) as u8,
        0x30 => (blk_per_chip >> 16) as u8,
        0x31 => b'P',
        0x32 => b'R',
        0x33 => b'I',
        0x34 => b'1',
        0x35 => b'0',
        _ => 0,
    }
}

fn dual(b: u8) -> u32 {
    ((b as u32) << 16) | b as u32
}

impl PFlash {
    /// `size` must be a multiple of BLOCK_SIZE. `contents` shorter than
    /// size is padded with 0xFF (erased).
    pub fn new(base: u64, size: u64, mut contents: Vec<u8>, file: Option<File>) -> Result<Self, String> {
        if size == 0 || !size.is_multiple_of(BLOCK_SIZE) || contents.len() as u64 > size {
            return Err("bad flash size".into());
        }
        contents.resize(size as usize, 0xff);
        Ok(PFlash { base, data: contents, file, st: State { mode: Mode::ReadArray, buf: Vec::new() } })
    }

    fn persist(&self, off: u64, len: usize) {
        if let Some(f) = &self.file {
            if let Err(e) = f.write_all_at(&self.data[off as usize..off as usize + len], off) {
                log::warn!("pflash: persist: {e}");
            }
        }
    }

    fn store(&mut self, off: u64, v: u32) {
        let o = off as usize & !3;
        if o + 4 <= self.data.len() {
            self.data[o..o + 4].copy_from_slice(&v.to_le_bytes());
            self.persist(o as u64, 4);
        }
    }

    fn erase(&mut self, off: u64) {
        let start = (off & !(BLOCK_SIZE - 1)) as usize;
        if start + BLOCK_SIZE as usize <= self.data.len() {
            self.data[start..start + BLOCK_SIZE as usize].fill(0xff);
            self.persist(start as u64, BLOCK_SIZE as usize);
        }
    }
}

impl BusDevice for PFlash {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match self.st.mode {
            Mode::ReadArray => {
                let o = offset as usize;
                if o < self.data.len() {
                    let n = data.len().min(self.data.len() - o);
                    data[..n].copy_from_slice(&self.data[o..o + n]);
                    data[n..].fill(0);
                } else {
                    data.fill(0);
                }
            }
            Mode::Id => {
                let v = match offset / 4 {
                    0 => dual(0x89),
                    1 => dual(0x18),
                    _ => 0,
                };
                put_le(data, v as u64);
            }
            Mode::Cfi => put_le(data, dual(cfi_byte(offset / 4, self.data.len() as u64)) as u64),
            _ => put_le(data, STATUS_READY as u64),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let v = get_le(data) as u32;
        let cmd = v as u8;
        let mode = self.st.mode;
        self.st.mode = match mode {
            Mode::WordProgram => {
                self.store(offset, v);
                Mode::Status
            }
            Mode::EraseSetup => {
                if cmd == 0xd0 {
                    self.erase(offset);
                }
                Mode::Status
            }
            Mode::LockSetup => Mode::Status,
            Mode::BufCount { .. } => {
                let n = (v & 0xffff) + 1;
                if n > MAX_BUF_WORDS {
                    Mode::Status
                } else {
                    self.st.buf.clear();
                    Mode::BufData { left: n }
                }
            }
            Mode::BufData { left } => {
                self.st.buf.push((offset, v));
                if left > 1 {
                    Mode::BufData { left: left - 1 }
                } else {
                    Mode::BufConfirm
                }
            }
            Mode::BufConfirm => {
                if cmd == 0xd0 {
                    for (o, w) in std::mem::take(&mut self.st.buf) {
                        self.store(o, w);
                    }
                }
                self.st.buf.clear();
                Mode::Status
            }
            _ => match cmd {
                0xff => Mode::ReadArray,
                0x70 | 0x50 => Mode::Status,
                0x90 => Mode::Id,
                0x98 => Mode::Cfi,
                0x10 | 0x40 => Mode::WordProgram,
                0x20 => Mode::EraseSetup,
                0x60 => Mode::LockSetup,
                0xe8 => Mode::BufCount { block: offset & !(BLOCK_SIZE - 1) },
                _ => mode,
            },
        };
    }
}

#[derive(Serialize, Deserialize)]
struct Snap {
    st: State,
    /// Contents up to the last non-erased byte.
    data: Vec<u8>,
}

const MAGIC: &[u8; 8] = b"VRPFL\x001\0";
const PAGE: usize = 4096;
const PAGE_ZERO: u8 = 0;
const PAGE_ERASED: u8 = 1;
const PAGE_RAW: u8 = 2;

impl Snapshottable for PFlash {
    fn snapshot_id(&self) -> String {
        format!("pflash@{:x}", self.base)
    }

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        // Binary: magic, state JSON, then 4 KiB pages tagged all-0x00 /
        // all-0xff / raw. A 64 MiB variable store is mostly one or the other.
        let st = encode(&self.st)?;
        let mut out = Vec::with_capacity(64 << 10);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(st.len() as u32).to_le_bytes());
        out.extend_from_slice(&st);
        out.extend_from_slice(&(self.data.len() as u64).to_le_bytes());
        for page in self.data.chunks(PAGE) {
            match page {
                p if p.iter().all(|&b| b == 0) => out.push(PAGE_ZERO),
                p if p.iter().all(|&b| b == 0xff) => out.push(PAGE_ERASED),
                p => {
                    out.push(PAGE_RAW);
                    out.extend_from_slice(p);
                }
            }
        }
        Ok(out)
    }

    fn restore(&mut self, d: &[u8]) -> vroom_snapshot::Result<()> {
        let bad = |m: &str| vroom_snapshot::Error::Invalid(format!("pflash snapshot: {m}"));
        let (st, data) = if d.starts_with(MAGIC) {
            let mut r = &d[MAGIC.len()..];
            let mut take = |n: usize| -> vroom_snapshot::Result<&[u8]> {
                let (h, t) = r.split_at_checked(n).ok_or_else(|| bad("truncated"))?;
                r = t;
                Ok(h)
            };
            let st_len = u32::from_le_bytes(take(4)?.try_into().unwrap_or_default()) as usize;
            let st: State = decode(take(st_len)?)?;
            let len = u64::from_le_bytes(take(8)?.try_into().unwrap_or_default());
            if len > self.data.len() as u64 {
                return Err(bad("too large"));
            }
            let mut data = Vec::with_capacity(len as usize);
            while data.len() < len as usize {
                let n = PAGE.min(len as usize - data.len());
                match take(1)?[0] {
                    PAGE_ZERO => data.resize(data.len() + n, 0),
                    PAGE_ERASED => data.resize(data.len() + n, 0xff),
                    PAGE_RAW => data.extend_from_slice(take(n)?),
                    _ => return Err(bad("bad page tag")),
                }
            }
            (st, data)
        } else {
            // Older snapshots: JSON with the trimmed contents.
            let s: Snap = decode(d)?;
            (s.st, s.data)
        };
        if data.len() > self.data.len() || st.buf.len() > MAX_BUF_WORDS as usize {
            return Err(bad("too large"));
        }
        self.data.fill(0xff);
        self.data[..data.len()].copy_from_slice(&data);
        self.persist(0, self.data.len());
        self.st = st;
        Ok(())
    }

    fn reset(&mut self, _pristine: &[u8]) -> vroom_snapshot::Result<()> {
        // Flash is non-volatile: keep the data, drop any half-done command.
        self.st = State { mode: Mode::ReadArray, buf: Vec::new() };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(f: &mut PFlash, off: u64, v: u32) {
        f.write(off, &v.to_le_bytes());
    }
    fn r(f: &mut PFlash, off: u64) -> u32 {
        let mut d = [0u8; 4];
        f.read(off, &mut d);
        u32::from_le_bytes(d)
    }

    #[test]
    fn program_erase_buffered() {
        let mut f = PFlash::new(0, 4 * BLOCK_SIZE, vec![], None).unwrap();
        assert_eq!(r(&mut f, 0), 0xffff_ffff);
        w(&mut f, 0x100, 0x0040_0040);
        w(&mut f, 0x100, 0x1234_5678);
        assert_eq!(r(&mut f, 0x100), STATUS_READY);
        w(&mut f, 0, 0x00ff_00ff);
        assert_eq!(r(&mut f, 0x100), 0x1234_5678);
        // buffered: 2 words
        w(&mut f, BLOCK_SIZE, 0x00e8_00e8);
        w(&mut f, BLOCK_SIZE, 0x0001_0001);
        w(&mut f, BLOCK_SIZE, 0xaaaa_aaaa);
        w(&mut f, BLOCK_SIZE + 4, 0xbbbb_bbbb);
        w(&mut f, BLOCK_SIZE, 0x00d0_00d0);
        w(&mut f, 0, 0x00ff_00ff);
        assert_eq!((r(&mut f, BLOCK_SIZE), r(&mut f, BLOCK_SIZE + 4)), (0xaaaa_aaaa, 0xbbbb_bbbb));
        // erase block 0
        w(&mut f, 0x0, 0x0020_0020);
        w(&mut f, 0x0, 0x00d0_00d0);
        w(&mut f, 0, 0x00ff_00ff);
        assert_eq!(r(&mut f, 0x100), 0xffff_ffff);
        assert_eq!(r(&mut f, BLOCK_SIZE), 0xaaaa_aaaa);
        // CFI
        w(&mut f, 0, 0x0098_0098);
        assert_eq!(r(&mut f, 0x10 * 4), dual(b'Q'));
        // out of range is harmless
        w(&mut f, 0, 0x0040_0040);
        w(&mut f, 1 << 40, 1);
    }

    #[test]
    fn reset_keeps_data_and_leaves_command_mode() {
        let mut f = PFlash::new(0, 4 * BLOCK_SIZE, vec![], None).unwrap();
        let pristine = f.save().unwrap();
        w(&mut f, 0x100, 0x0040_0040);
        w(&mut f, 0x100, 0x1234_5678);
        w(&mut f, 0, 0x0070_0070); // left in status mode
        f.reset(&pristine).unwrap();
        assert_eq!(r(&mut f, 0x100), 0x1234_5678);
    }

    #[test]
    fn snapshot_roundtrip_is_compact() {
        let mut f = PFlash::new(0, 4 * BLOCK_SIZE, vec![0; 2 * BLOCK_SIZE as usize], None).unwrap();
        w(&mut f, 0x100, 0x0040_0040);
        w(&mut f, 0x100, 0x1234_5678);
        let blob = f.save().unwrap();
        assert!(blob.len() < 16 << 10, "blob is {} bytes", blob.len());
        let mut g = PFlash::new(0, 4 * BLOCK_SIZE, vec![], None).unwrap();
        g.restore(&blob).unwrap();
        w(&mut g, 0, 0x00ff_00ff);
        assert_eq!(r(&mut g, 0x100), 0x1234_5678);
        assert_eq!(r(&mut g, 0x200), 0);
        assert_eq!(r(&mut g, 3 * BLOCK_SIZE), 0xffff_ffff);
        assert!(g.restore(&blob[..blob.len() - 1]).is_err());
    }
}
