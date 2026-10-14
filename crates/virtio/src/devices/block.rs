//! virtio-blk.

use std::sync::Arc;

use bulwark_block::{DiskImage, SECTOR_SIZE};

use crate::worker::{QueueCtx, QueueHandler, WorkerSlot};
use crate::{chain::DescriptorChain, types, ActivateContext, DeviceState, Result, VirtioDevice};

const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;
const F_DISCARD: u64 = 1 << 13;
const F_WRITE_ZEROES: u64 = 1 << 14;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;
const T_WRITE_ZEROES: u32 = 13;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const QUEUE_SIZE: u16 = 256;
const SEG_MAX: u32 = 126;
/// Biggest single read/write we accept (Linux default max is far below).
const MAX_IO: usize = 4 << 20;
const MAX_DISCARD_SECTORS: u32 = (1 << 30) / 512;

#[derive(Debug)]
pub struct Block {
    disk: Arc<dyn DiskImage>,
    id: String,
    serial: [u8; 20],
    worker: WorkerSlot<BlockHandler>,
}

impl Block {
    /// `id` must be unique per VM ("vda", "vdb"...); used in snapshots.
    pub fn new(id: &str, disk: Arc<dyn DiskImage>) -> Self {
        let mut serial = [0u8; 20];
        let s = format!("bulwark-{id}");
        let n = s.len().min(20);
        serial[..n].copy_from_slice(&s.as_bytes()[..n]);
        Block { disk, id: id.to_string(), serial, worker: WorkerSlot::default() }
    }

    fn handler(&self) -> BlockHandler {
        BlockHandler { disk: self.disk.clone(), serial: self.serial }
    }

    fn config(&self) -> [u8; 60] {
        let mut c = [0u8; 60];
        let sectors = self.disk.size() / SECTOR_SIZE;
        c[0..8].copy_from_slice(&sectors.to_le_bytes());
        c[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        c[20..24].copy_from_slice(&512u32.to_le_bytes());
        c[34..36].copy_from_slice(&1u16.to_le_bytes()); // num_queues
        c[36..40].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
        c[40..44].copy_from_slice(&1u32.to_le_bytes()); // max_discard_seg
        c[44..48].copy_from_slice(&8u32.to_le_bytes()); // discard alignment (sectors)
        c[48..52].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
        c[52..56].copy_from_slice(&1u32.to_le_bytes());
        c
    }
}

#[derive(Debug)]
pub struct BlockHandler {
    disk: Arc<dyn DiskImage>,
    serial: [u8; 20],
}

impl BlockHandler {
    /// Run one request; returns bytes written to the guest (incl. status).
    fn request(&self, chain: DescriptorChain) -> u32 {
        let Ok((mut r, mut w)) = chain.into_rw() else { return 0 };
        let Ok(mut status) = w.split_tail(1) else { return 0 };
        let st = match self.execute(&mut r, &mut w) {
            Ok(()) => S_OK,
            Err(s) => s,
        };
        let _ = status.write_all(&[st]);
        (w.written() + 1) as u32
    }

    fn execute(&self, r: &mut crate::Reader, w: &mut crate::Writer) -> std::result::Result<(), u8> {
        let ty: u32 = u32::from_le(r.read_obj().map_err(|_| S_IOERR)?);
        let _reserved: u32 = r.read_obj().map_err(|_| S_IOERR)?;
        let sector: u64 = u64::from_le(r.read_obj().map_err(|_| S_IOERR)?);
        let offset = sector.checked_mul(SECTOR_SIZE).ok_or(S_IOERR)?;
        let ro = self.disk.read_only();
        match ty {
            T_IN => {
                let len = w.available();
                if !len.is_multiple_of(SECTOR_SIZE as usize) || len > MAX_IO {
                    return Err(S_IOERR);
                }
                let mut buf = vec![0u8; len];
                self.disk.read_at(&mut buf, offset).map_err(|_| S_IOERR)?;
                w.write_all(&buf).map_err(|_| S_IOERR)
            }
            T_OUT => {
                if ro {
                    return Err(S_IOERR);
                }
                let len = r.available();
                if !len.is_multiple_of(SECTOR_SIZE as usize) || len > MAX_IO {
                    return Err(S_IOERR);
                }
                let data = r.read_to_vec().map_err(|_| S_IOERR)?;
                self.disk.write_at(&data, offset).map_err(|_| S_IOERR)
            }
            T_FLUSH => self.disk.flush().map_err(|_| S_IOERR),
            T_GET_ID => {
                let n = w.available().min(20);
                w.write_all(&self.serial[..n]).map_err(|_| S_IOERR)
            }
            T_DISCARD | T_WRITE_ZEROES => {
                if ro {
                    return Err(S_IOERR);
                }
                // struct virtio_blk_discard_write_zeroes { u64 sector; u32 num; u32 flags; }
                let s: u64 = u64::from_le(r.read_obj().map_err(|_| S_IOERR)?);
                let n: u32 = u32::from_le(r.read_obj().map_err(|_| S_IOERR)?);
                let _flags: u32 = r.read_obj().map_err(|_| S_IOERR)?;
                if n > MAX_DISCARD_SECTORS {
                    return Err(S_IOERR);
                }
                let off = s.checked_mul(SECTOR_SIZE).ok_or(S_IOERR)?;
                let len = n as u64 * SECTOR_SIZE;
                bulwark_block::check_range(self.disk.size(), off, len).map_err(|_| S_IOERR)?;
                if ty == T_DISCARD {
                    self.disk.discard(off, len).map_err(|_| S_IOERR)
                } else {
                    self.disk.write_zeroes(off, len).map_err(|_| S_IOERR)
                }
            }
            _ => Err(S_UNSUPP),
        }
    }
}

impl QueueHandler for BlockHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        loop {
            let chain = match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("blk: {e}");
                    break;
                }
            };
            let head = chain.head();
            let len = self.request(chain);
            ctx.add_used(q, head, len);
        }
    }
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        types::BLOCK
    }
    fn name(&self) -> String {
        format!("blk-{}", self.id)
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE]
    }
    fn device_features(&self) -> u64 {
        let mut f = F_SEG_MAX | F_BLK_SIZE | F_FLUSH | crate::features::RING_EVENT_IDX
            | crate::features::RING_INDIRECT_DESC;
        if self.disk.read_only() {
            f |= F_RO;
        } else {
            f |= F_DISCARD | F_WRITE_ZEROES;
        }
        f
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let c = self.config();
        let o = offset as usize;
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
            data[n..].fill(0);
        } else {
            data.fill(0);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        let h = self.handler();
        self.worker.start(&self.name(), ctx, h, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.worker.stop();
        let _ = self.disk.flush();
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        // Snapshot must match disk contents: push writes down first.
        self.disk.flush().map_err(|e| crate::Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data: Vec::new() })
    }
    fn restore_active(&mut self, ctx: ActivateContext, _data: &[u8]) -> Result<()> {
        let h = self.handler();
        self.worker.start(&self.name(), ctx, h, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::tests::*;
    use bulwark_block::raw::RawImage;
    use bulwark_memory::GuestAddress;

    fn disk() -> Arc<dyn DiskImage> {
        let f = tempfile::tempfile().unwrap();
        f.set_len(1 << 20).unwrap();
        Arc::new(RawImage::from_file(f, false).unwrap())
    }

    fn header(r: &TestRing, at: u64, ty: u32, sector: u64) {
        r.mem.write_obj(GuestAddress(at), &ty).unwrap();
        r.mem.write_obj(GuestAddress(at + 8), &sector).unwrap();
    }

    #[test]
    fn write_then_read() {
        let d = disk();
        let h = BlockHandler { disk: d.clone(), serial: [0; 20] };
        let mut ring = TestRing::new(8);
        // write: hdr(16, ro) data(512, ro) status(1, w)
        let head = ring.add_chain(&[(16, false), (512, false), (1, true)]);
        header(&ring, DATA, T_OUT, 3);
        ring.mem.write(GuestAddress(DATA + 0x1000), &[0xab; 512]).unwrap();
        let mut q = ring.q.clone();
        let c = q.pop(&ring.mem).unwrap().unwrap();
        assert_eq!(h.request(c), 1);
        assert_eq!(ring.mem.read_obj::<u8>(GuestAddress(DATA + 0x2000)).unwrap(), S_OK);
        let mut b = [0u8; 4];
        d.read_at(&mut b, 3 * 512).unwrap();
        assert_eq!(b, [0xab; 4]);
        let _ = head;

        // read it back: hdr(16) data(512, w) status(1, w)
        ring.add_chain(&[(16, false), (512, true), (1, true)]);
        header(&ring, DATA + 0x3000, T_IN, 3);
        let c = q.pop(&ring.mem).unwrap().unwrap();
        assert_eq!(h.request(c), 513);
        assert_eq!(ring.mem.read_obj::<u8>(GuestAddress(DATA + 0x4000)).unwrap(), 0xab);
        assert_eq!(ring.mem.read_obj::<u8>(GuestAddress(DATA + 0x5000)).unwrap(), S_OK);
    }

    #[test]
    fn out_of_range_and_overflow() {
        let h = BlockHandler { disk: disk(), serial: [0; 20] };
        for sector in [(1u64 << 20) / 512, u64::MAX / 256] {
            let mut ring = TestRing::new(8);
            ring.add_chain(&[(16, false), (512, true), (1, true)]);
            header(&ring, DATA, T_IN, sector);
            let c = ring.q.clone().pop(&ring.mem).unwrap().unwrap();
            h.request(c);
            assert_eq!(ring.mem.read_obj::<u8>(GuestAddress(DATA + 0x2000)).unwrap(), S_IOERR);
        }
    }
}
