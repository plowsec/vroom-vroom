//! virtio-blk.
//!
//! The device worker pops requests. A lone request runs right there; when
//! the guest has several in flight they go to a few I/O threads and run
//! in parallel, and completions come back to the worker, which alone
//! touches the rings. Data moves between the image and guest RAM without
//! a bounce buffer where the format allows (raw, plain qcow2 clusters).
//! Pausing waits for the I/O threads, so a paused device never has I/O in
//! flight (snapshots stay consistent).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use vroom_block::{DiskImage, SECTOR_SIZE};

use crate::worker::{Poker, QueueCtx, QueueHandler, WorkerSlot, POKE_TOKEN};
use crate::{chain::DescriptorChain, types, ActivateContext, DeviceState, Result, VirtioDevice};

const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_MQ: u64 = 1 << 12;
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
/// I/O threads per disk, started on first use...
const IO_THREADS: usize = 4;
/// ...and more while requests wait for one, up to this. A real SSD only
/// gets busy with many requests in flight: with 4 threads, 4k random
/// reads at queue depth 32 ran at a third of QEMU's speed.
const MAX_IO_THREADS: usize = 32;
/// Requests handed to the I/O threads and not back yet. Bounded anyway
/// by the queue size; this keeps the backlog short.
const MAX_INFLIGHT: usize = 64;
/// A lone read or write up to this size runs on the vCPU that kicked.
const INLINE_MAX: usize = 64 << 10;
/// Most request queues we offer (one per vCPU up to this).
pub const MAX_QUEUES: u16 = 16;

#[derive(Debug)]
pub struct Block {
    disk: Arc<dyn DiskImage>,
    id: String,
    serial: [u8; 20],
    queues: u16,
    worker: WorkerSlot<BlockHandler>,
}

impl Block {
    /// `id` must be unique per VM ("vda", "vdb"...); used in snapshots.
    pub fn new(id: &str, disk: Arc<dyn DiskImage>) -> Self {
        Block { disk, id: id.to_string(), serial: [0u8; 20], queues: 1, worker: WorkerSlot::default() }
            .with_serial(&format!("VRM-{id}"))
    }

    /// Request queues (one per vCPU is best: each gets its own interrupt,
    /// handled on the vCPU that asked). 1 to MAX_QUEUES.
    pub fn with_queues(mut self, n: u16) -> Self {
        self.queues = n.clamp(1, MAX_QUEUES);
        self
    }

    /// Serial the guest reads (GET_ID, up to 20 bytes; from `[identity]`).
    pub fn with_serial(mut self, s: &str) -> Self {
        self.serial = [0u8; 20];
        let n = s.len().min(20);
        self.serial[..n].copy_from_slice(&s.as_bytes()[..n]);
        self
    }

    fn handler(&self) -> BlockHandler {
        BlockHandler::new(Arc::new(Exec { disk: self.disk.clone(), serial: self.serial }))
    }

    fn config(&self) -> [u8; 60] {
        let mut c = [0u8; 60];
        let sectors = self.disk.size() / SECTOR_SIZE;
        c[0..8].copy_from_slice(&sectors.to_le_bytes());
        c[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        c[20..24].copy_from_slice(&512u32.to_le_bytes());
        c[34..36].copy_from_slice(&self.queues.to_le_bytes()); // num_queues
        c[36..40].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
        c[40..44].copy_from_slice(&1u32.to_le_bytes()); // max_discard_seg
        c[44..48].copy_from_slice(&8u32.to_le_bytes()); // discard alignment (sectors)
        c[48..52].copy_from_slice(&MAX_DISCARD_SECTORS.to_le_bytes());
        c[52..56].copy_from_slice(&1u32.to_le_bytes());
        c
    }
}

/// Runs requests; shared by the worker and the I/O threads.
#[derive(Debug)]
struct Exec {
    disk: Arc<dyn DiskImage>,
    serial: [u8; 20],
}

impl Exec {
    /// A quick read or write (or GET_ID)? Peeks at a copy of the chain.
    fn is_quick(chain: &DescriptorChain) -> bool {
        let Ok((mut r, w)) = chain.clone().into_rw() else { return true };
        let Ok(ty) = r.read_obj::<u32>() else { return true };
        match u32::from_le(ty) {
            T_IN => w.available() <= INLINE_MAX + 1,
            T_OUT => r.available() <= INLINE_MAX + 12,
            T_GET_ID => true,
            _ => false,
        }
    }

    /// Run one request; returns bytes written to the guest (incl. status).
    fn request(&self, chain: DescriptorChain) -> u32 {
        let mem = chain.memory().clone();
        let Ok((mut r, mut w)) = chain.into_rw() else { return 0 };
        let Ok(mut status) = w.split_tail(1) else { return 0 };
        let st = match self.execute(&mem, &mut r, &mut w) {
            Ok(()) => S_OK,
            Err(s) => s,
        };
        let _ = status.write_all(&[st]);
        (w.written() + 1) as u32
    }

    fn execute(&self, mem: &vroom_block::GuestMemory, r: &mut crate::Reader, w: &mut crate::Writer) -> std::result::Result<(), u8> {
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
                let segs = w.take_segments();
                self.disk.read_to_guest(mem, offset, &segs).map_err(|_| S_IOERR)
            }
            T_OUT => {
                if ro {
                    return Err(S_IOERR);
                }
                let len = r.available();
                if !len.is_multiple_of(SECTOR_SIZE as usize) || len > MAX_IO {
                    return Err(S_IOERR);
                }
                let segs = r.take_segments();
                self.disk.write_from_guest(mem, offset, &segs).map_err(|_| S_IOERR)
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
                vroom_block::check_range(self.disk.size(), off, len).map_err(|_| S_IOERR)?;
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

/// A request for the I/O threads: its queue and its chain.
type Job = (usize, DescriptorChain);

/// Finished requests waiting for the worker: (queue, head, used length).
#[derive(Debug, Default)]
struct Done {
    list: Vec<(usize, u16, u32)>,
    /// Handed to the threads and not finished yet.
    running: usize,
}

#[derive(Debug, Default)]
struct PoolShared {
    done: Mutex<Done>,
    /// Signalled when `running` drops to 0 (pause waits on it).
    idle: Condvar,
}

/// The I/O threads of one disk.
#[derive(Debug)]
struct Pool {
    jobs: Option<Sender<Job>>,
    threads: Vec<JoinHandle<()>>,
    /// What a new thread needs.
    spawn: Spawn,
}

#[derive(Debug, Clone)]
struct Spawn {
    exec: Arc<Exec>,
    shared: Arc<PoolShared>,
    poker: Poker,
    rx: Arc<Mutex<Receiver<Job>>>,
}

impl Pool {
    fn start(exec: &Arc<Exec>, shared: &Arc<PoolShared>, poker: &Poker) -> std::io::Result<Pool> {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let spawn = Spawn { exec: exec.clone(), shared: shared.clone(), poker: poker.clone(), rx };
        let mut pool = Pool { jobs: Some(tx), threads: Vec::new(), spawn };
        pool.grow_to(IO_THREADS)?;
        Ok(pool)
    }

    /// At least `n` threads (capped), so `n` requests can be in flight.
    fn grow_to(&mut self, n: usize) -> std::io::Result<()> {
        while self.threads.len() < n.min(MAX_IO_THREADS) {
            let sp = self.spawn.clone();
            let name = format!("blk-io{}", self.threads.len());
            self.threads.push(std::thread::Builder::new().name(name).spawn(move || io_thread(&sp.exec, &sp.shared, &sp.poker, &sp.rx))?);
        }
        Ok(())
    }
}

impl Drop for Pool {
    /// Requests already handed out still finish (they write guest RAM):
    /// after this, nothing does.
    fn drop(&mut self) {
        self.jobs = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn io_thread(exec: &Exec, shared: &PoolShared, poker: &Poker, rx: &Mutex<Receiver<Job>>) {
    loop {
        let job = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
        let Ok((q, chain)) = job else { return };
        let head = chain.head();
        let len = exec.request(chain);
        let mut d = shared.done.lock().unwrap_or_else(|e| e.into_inner());
        d.list.push((q, head, len));
        d.running -= 1;
        if d.running == 0 {
            shared.idle.notify_all();
        }
        drop(d);
        poker.poke();
    }
}

#[derive(Debug)]
pub struct BlockHandler {
    exec: Arc<Exec>,
    shared: Arc<PoolShared>,
    pool: Option<Pool>,
    poker: Option<Poker>,
    /// Requests handed out (worker's count; the threads keep their own).
    inflight: Arc<AtomicUsize>,
    policy: vroom_block::InlinePolicy,
}

impl BlockHandler {
    fn new(exec: Arc<Exec>) -> Self {
        BlockHandler { exec, shared: Arc::default(), pool: None, poker: None, inflight: Arc::default(), policy: Default::default() }
    }

    /// Run one request; returns bytes written to the guest (incl. status).
    #[cfg(test)]
    fn request(&self, chain: DescriptorChain) -> u32 {
        self.exec.request(chain)
    }

    /// Hand requests to the I/O threads. False if they can't run there.
    fn dispatch(&mut self, q: usize, chains: &mut Vec<DescriptorChain>) -> bool {
        if self.pool.is_none() {
            let Some(p) = &self.poker else { return false };
            match Pool::start(&self.exec, &self.shared, p) {
                Ok(pool) => self.pool = Some(pool),
                Err(e) => {
                    log::warn!("blk: no I/O threads ({e}), running requests inline");
                    return false;
                }
            }
        }
        let running = {
            let mut d = self.shared.done.lock().unwrap_or_else(|e| e.into_inner());
            d.running += chains.len();
            d.running
        };
        if let Some(pool) = self.pool.as_mut() {
            if let Err(e) = pool.grow_to(running) {
                log::debug!("blk: no more I/O threads ({e})");
            }
        }
        let Some(tx) = self.pool.as_ref().and_then(|p| p.jobs.as_ref()) else { return false };
        self.inflight.fetch_add(chains.len(), Ordering::Relaxed);
        for c in chains.drain(..) {
            if let Err(mpsc::SendError((q, c))) = tx.send((q, c)) {
                // Threads gone (can't happen while we hold the pool): finish here.
                let head = c.head();
                let len = self.exec.request(c);
                let mut d = self.shared.done.lock().unwrap_or_else(|e| e.into_inner());
                d.list.push((q, head, len));
                d.running -= 1;
            }
        }
        true
    }

    /// Give finished requests back to the driver.
    fn reap(&mut self, ctx: &mut QueueCtx) {
        let list = std::mem::take(&mut self.shared.done.lock().unwrap_or_else(|e| e.into_inner()).list);
        self.inflight.fetch_sub(list.len(), Ordering::Relaxed);
        for (q, head, len) in list {
            ctx.add_used(q, head, len);
        }
    }
}

impl QueueHandler for BlockHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        self.reap(ctx);
        let mut batch = Vec::new();
        while self.inflight.load(Ordering::Relaxed) + batch.len() < MAX_INFLIGHT {
            match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => batch.push(c),
                Ok(None) => break,
                Err(e) => {
                    log::warn!("blk: {e}");
                    break;
                }
            }
        }
        // One request and nothing else running: no thread hop.
        let others = batch.len() > 1 || self.inflight.load(Ordering::Relaxed) > 0;
        if !batch.is_empty() {
            // Only to follow the guest's depth here.
            let _ = self.policy.run_inline(others);
        }
        if others {
            self.dispatch(q, &mut batch);
        }
        for c in batch {
            let head = c.head();
            let len = self.exec.request(c);
            ctx.add_used(q, head, len);
        }
    }

    /// On the vCPU: a lone quick request runs right here (no thread
    /// wakeups at all) while the guest sends one at a time; anything else
    /// goes to the I/O threads, so the vCPU never waits for long.
    fn process_queue_inline(&mut self, ctx: &mut QueueCtx, q: usize) -> bool {
        // A guest with many in flight: the worker takes them in batches
        // (and the guest skips kicks while it is busy).
        if self.policy.threaded() {
            return false;
        }
        self.reap(ctx);
        let mut batch = Vec::new();
        while self.inflight.load(Ordering::Relaxed) + batch.len() < MAX_INFLIGHT {
            match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => batch.push(c),
                Ok(None) => break,
                Err(e) => {
                    log::warn!("blk: {e}");
                    break;
                }
            }
        }
        let others = batch.len() > 1 || self.inflight.load(Ordering::Relaxed) > 0;
        let inline = self.policy.run_inline(others) && batch.first().is_some_and(Exec::is_quick);
        if !inline {
            self.dispatch(q, &mut batch);
        }
        for c in batch {
            let head = c.head();
            let len = self.exec.request(c);
            ctx.add_used(q, head, len);
        }
        true
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, token: usize) {
        if token == POKE_TOKEN {
            self.reap(ctx);
            // Room again: requests the cap held back.
            for q in 0..ctx.queues.len() {
                if self.inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT {
                    break;
                }
                self.process_queue(ctx, q);
            }
        }
    }

    fn attach(&mut self, poker: Poker) {
        self.poker = Some(poker);
    }

    fn quiesce(&mut self, ctx: &mut QueueCtx) {
        let mut d = self.shared.done.lock().unwrap_or_else(|e| e.into_inner());
        while d.running > 0 {
            d = self.shared.idle.wait(d).unwrap_or_else(|e| e.into_inner());
        }
        drop(d);
        self.reap(ctx);
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
        vec![QUEUE_SIZE; self.queues as usize]
    }
    fn device_features(&self) -> u64 {
        let mut f = F_SEG_MAX | F_BLK_SIZE | F_FLUSH | crate::features::RING_EVENT_IDX
            | crate::features::RING_INDIRECT_DESC;
        if self.queues > 1 {
            f |= F_MQ;
        }
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
        self.worker.notify_inline(q);
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
    use vroom_block::raw::RawImage;
    use vroom_memory::GuestAddress;

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
        let h = BlockHandler::new(Arc::new(Exec { disk: d.clone(), serial: [0; 20] }));
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
        let h = BlockHandler::new(Arc::new(Exec { disk: disk(), serial: [0; 20] }));
        for sector in [(1u64 << 20) / 512, u64::MAX / 256] {
            let mut ring = TestRing::new(8);
            ring.add_chain(&[(16, false), (512, true), (1, true)]);
            header(&ring, DATA, T_IN, sector);
            let c = ring.q.clone().pop(&ring.mem).unwrap().unwrap();
            h.request(c);
            assert_eq!(ring.mem.read_obj::<u8>(GuestAddress(DATA + 0x2000)).unwrap(), S_IOERR);
        }
    }

    struct NoIrq;
    impl crate::VirtioInterrupt for NoIrq {
        fn signal_used_queue(&self, _q: u16) {}
        fn signal_config_changed(&self) {}
    }

    fn wait_used(ring: &TestRing, n: u16) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ring.used_idx() != n {
            assert!(std::time::Instant::now() < end, "only {} of {n} requests done", ring.used_idx());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A lone small request kicked from a vCPU is done before the kick
    /// returns; a big one goes to the I/O threads.
    #[test]
    fn lone_quick_request_runs_on_the_kicking_thread() {
        let d = disk();
        let mut ring = TestRing::new(16);
        let ctx = crate::ActivateContext { mem: ring.mem.clone(), queues: vec![ring.q.clone()], irq: Arc::new(NoIrq), features: 0 };
        let h = BlockHandler::new(Arc::new(Exec { disk: d, serial: [0; 20] }));
        let w = crate::worker::Worker::spawn("blk-test", QueueCtx::new(ctx), h, false).unwrap();
        // Let the worker's start-up pass finish first.
        w.with(|_, _| ());
        ring.add_chain(&[(16, false), (4096, true), (1, true)]);
        header(&ring, DATA, T_IN, 0);
        let mut done = false;
        for _ in 0..1000 {
            if w.try_inline(0) {
                done = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(done);
        assert_eq!(ring.used_idx(), 1, "done inline");
        assert_eq!(ring.used(0).1, 4097);
        // 512 KiB: handed to the I/O threads, completes a bit later.
        ring.add_chain(&[(16, false), (512 << 10, true), (1, true)]);
        header(&ring, DATA + 0x3000, T_IN, 0);
        assert!(w.try_inline(0) || { w.kick(0); true });
        wait_used(&ring, 2);
    }

    /// Many requests at once go through the I/O threads; pausing waits
    /// for them, so a paused device has nothing in flight.
    #[test]
    fn parallel_requests_and_pause() {
        const N: u16 = 40;
        let d = disk();
        let mut ring = TestRing::new(128);
        for i in 0..N {
            ring.add_chain(&[(16, false), (512, false), (1, true)]);
            let base = DATA + 0x1000 * (3 * i as u64);
            header(&ring, base, T_OUT, 8 * i as u64);
            ring.mem.write(GuestAddress(base + 0x1000), &[i as u8 + 1; 512]).unwrap();
        }
        let ctx = crate::ActivateContext { mem: ring.mem.clone(), queues: vec![ring.q.clone()], irq: Arc::new(NoIrq), features: 0 };
        let h = BlockHandler::new(Arc::new(Exec { disk: d.clone(), serial: [0; 20] }));
        let w = crate::worker::Worker::spawn("blk-test", QueueCtx::new(ctx), h, false).unwrap();
        wait_used(&ring, N);
        for i in 0..N {
            let mut b = [0u8; 512];
            d.read_at(&mut b, 8 * 512 * i as u64).unwrap();
            assert_eq!(b, [i as u8 + 1; 512]);
            let status = DATA + 0x1000 * (3 * i as u64 + 2);
            assert_eq!(ring.mem.read_obj::<u8>(GuestAddress(status)).unwrap(), S_OK);
        }
        // Reads this time; pause right after the start.
        let mut ring2 = TestRing::new(128);
        for i in 0..N {
            ring2.add_chain(&[(16, false), (512, true), (1, true)]);
            header(&ring2, DATA + 0x1000 * (3 * i as u64), T_IN, 8 * i as u64);
        }
        drop(w);
        let ctx = crate::ActivateContext { mem: ring2.mem.clone(), queues: vec![ring2.q.clone()], irq: Arc::new(NoIrq), features: 0 };
        let h = BlockHandler::new(Arc::new(Exec { disk: d, serial: [0; 20] }));
        let w = crate::worker::Worker::spawn("blk-test", QueueCtx::new(ctx), h, false).unwrap();
        // Pause and resume while the requests run: every pause must find
        // everything taken from the ring already given back.
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ring2.used_idx() != N {
            assert!(std::time::Instant::now() < end);
            w.pause();
            w.with(|ctx, h| {
                assert_eq!(ctx.queues[0].next_avail, ctx.queues[0].next_used, "requests in flight while paused");
                assert_eq!(h.inflight.load(Ordering::Relaxed), 0);
            });
            w.resume();
            std::thread::yield_now();
        }
        for i in 0..N {
            let data = DATA + 0x1000 * (3 * i as u64 + 1);
            assert_eq!(ring2.mem.read_obj::<u8>(GuestAddress(data + 511)).unwrap(), i as u8 + 1);
        }
    }
}
