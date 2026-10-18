//! NVMe 1.4 controller (PCI). Windows boots from it with its inbox driver.
//!
//! Admin commands run on the vCPU that rings the doorbell. So does a lone
//! I/O command (no thread hop: the lowest latency). When the guest keeps
//! several in flight they go to a few I/O threads and run in parallel; a
//! completion thread posts their results. Pausing, resetting and deleting
//! queues first wait for those, so a paused VM never has I/O in flight
//! (snapshots stay consistent). Everything the guest hands us (queue
//! addresses, PRPs, LBAs) is checked.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;

use vroom_block::DiskImage;
use vroom_memory::{GuestAddress, GuestMemory};
use vroom_snapshot::{decode, encode};
use serde::{Deserialize, Serialize};

use crate::pci::{BarSpec, ConfigSpace, Intx, MsiSender, Msix, PciDevice, PciId, VendorIds};
use crate::{get_le, put_le};

const BAR_SIZE: u64 = 0x4000;
const DOORBELL: u64 = 0x1000;
const MSIX_TABLE: u32 = 0x2000;
const MSIX_PBA: u32 = 0x3000;
const MAX_QUEUES: u16 = 64;
const MQES: u16 = 1023;
const PAGE: u64 = 4096;
/// Max data transfer: 2^MDTS pages = 512 KiB.
const MDTS: u8 = 7;
const MAX_XFER: u64 = PAGE << MDTS;

// Status codes (generic command status, SCT 0) unless noted.
const SC_SUCCESS: u16 = 0x0;
const SC_INVALID_OPCODE: u16 = 0x1;
const SC_INVALID_FIELD: u16 = 0x2;
const SC_DATA_XFER_ERROR: u16 = 0x4;
const SC_INVALID_NS: u16 = 0xb;
const SC_PRP_OFFSET: u16 = 0x13;
const SC_LBA_RANGE: u16 = 0x80;
// Command specific (SCT 1).
const SC_INVALID_QID: u16 = 0x101;
const SC_INVALID_QSIZE: u16 = 0x102;
const SC_INVALID_CQ: u16 = 0x100;
const SC_INVALID_VECTOR: u16 = 0x108;
/// Attempted Write to Read Only Range: what a write to a read-only
/// namespace gets. NVMe 1.4's "Namespace Is Write Protected" (0x20) is
/// the newer answer, but Windows' driver retries it forever, Do Not
/// Retry or not (5 writes a second, for good); this one it takes as
/// final, as the SCSI translation's "write protected".
const SC_READ_ONLY: u16 = 0x182;
/// Do Not Retry, kept above the SCT bits until the entry is posted.
const DNR: u16 = 1 << 14;

/// I/O threads per controller, started on first use.
const IO_THREADS: usize = 4;
/// More while commands wait for a thread, up to this: a real SSD needs
/// many reads in flight (with 4, queue depth 32 got a third of QEMU's).
const MAX_IO_THREADS: usize = 32;
/// Commands on the I/O threads at most; past that they run inline.
const MAX_INFLIGHT: usize = 256;
/// Reads and writes this big always go to the I/O threads.
const ASYNC_BYTES: u64 = 64 << 10;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Regs {
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    intms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sq {
    base: u64,
    size: u16,
    head: u16,
    tail: u16,
    cqid: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cq {
    base: u64,
    size: u16,
    head: u16,
    tail: u16,
    phase: bool,
    ien: bool,
    iv: u16,
    /// Slots promised to commands still on the I/O threads (always 0
    /// when paused, so never in a snapshot).
    #[serde(skip)]
    reserved: u16,
}

impl Cq {
    fn full(&self) -> bool {
        let used = (self.tail + self.size - self.head) % self.size;
        used + self.reserved + 1 >= self.size
    }
    fn pending(&self) -> bool {
        self.head != self.tail
    }
}

/// A disk exposed as namespace `index + 1`.
#[derive(Debug)]
pub struct Namespace {
    pub disk: Arc<dyn DiskImage>,
    pub read_only: bool,
    /// 512, or 2048 for CD/DVD images (UDF expects 2 KiB sectors).
    pub block_size: u64,
}

impl Namespace {
    /// Configured read-only, or the image can't be written.
    fn ro(&self) -> bool {
        self.read_only || self.disk.read_only()
    }
}

/// Writes refused on read-only namespaces, all controllers (for the log).
static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
struct Snap {
    cfg: ConfigSpace,
    msix: Msix,
    regs: Regs,
    sqs: BTreeMap<u16, Sq>,
    cqs: BTreeMap<u16, Cq>,
    aer: Vec<u16>,
}

pub struct Nvme {
    id: String,
    cfg: ConfigSpace,
    msix: Msix,
    intx: Arc<Intx>,
    mem: GuestMemory,
    ns: Arc<[Namespace]>,
    serial: String,
    model: String,
    firmware: String,
    regs: Regs,
    sqs: BTreeMap<u16, Sq>,
    cqs: BTreeMap<u16, Cq>,
    /// Outstanding Asynchronous Event Request command ids.
    aer: Vec<u16>,
    /// Ourselves, for the completion thread (set by `into_shared`;
    /// without it every command runs inline).
    me: Weak<Mutex<Nvme>>,
    io: Arc<IoShared>,
    pool: Option<Pool>,
    policy: vroom_block::InlinePolicy,
}

/// A command handed to the I/O threads.
#[derive(Clone, Copy)]
struct Job {
    cmd: Cmd,
    sqid: u16,
    cqid: u16,
    sq_head: u16,
}

/// Results waiting to be posted, and how many are still running.
#[derive(Default)]
struct IoDone {
    list: Vec<(Job, Outcome)>,
    running: usize,
}

#[derive(Default)]
struct IoShared {
    done: Mutex<IoDone>,
    /// Signalled when `running` drops to 0.
    idle: Condvar,
}

/// The I/O threads. The completion thread isn't here: it holds only a
/// weak reference to the controller, ends when the I/O threads do, and is
/// never joined (it may be the one dropping the controller).
struct Pool {
    jobs: Option<Sender<Job>>,
    threads: Vec<JoinHandle<()>>,
    /// Cloned for each new thread.
    template: IoThread,
}

impl Pool {
    /// At least `n` threads (capped), so `n` commands can be in flight.
    fn grow_to(&mut self, n: usize) -> std::io::Result<()> {
        while self.threads.len() < n.min(MAX_IO_THREADS) {
            let t = self.template.clone();
            let name = format!("nvme-io{}", self.threads.len());
            self.threads.push(std::thread::Builder::new().name(name).spawn(move || t.run())?);
        }
        Ok(())
    }
}

impl Drop for Pool {
    /// Commands already handed out still finish (they write guest RAM).
    fn drop(&mut self) {
        self.jobs = None;
        let me = std::thread::current().id();
        for t in self.threads.drain(..) {
            // An I/O thread may hold the last reference to the controller.
            if t.thread().id() != me {
                let _ = t.join();
            }
        }
    }
}

impl std::fmt::Debug for Nvme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nvme").field("id", &self.id).field("namespaces", &self.ns.len()).finish()
    }
}

#[derive(Clone, Copy)]
struct Cmd {
    opc: u8,
    cid: u16,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw: [u32; 6], // cdw10..cdw15
}

impl Cmd {
    fn parse(b: &[u8; 64]) -> Cmd {
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u64_at = |o: usize| (u32_at(o) as u64) | ((u32_at(o + 4) as u64) << 32);
        let mut cdw = [0u32; 6];
        for (i, c) in cdw.iter_mut().enumerate() {
            *c = u32_at(40 + 4 * i);
        }
        Cmd { opc: b[0], cid: u16::from_le_bytes([b[2], b[3]]), nsid: u32_at(4), prp1: u64_at(24), prp2: u64_at(32), cdw }
    }
}

/// Command outcome: status code (incl. SCT in bits 8..) and DW0.
type Outcome = (u16, u32);

fn ok(dw0: u32) -> Option<Outcome> {
    Some((SC_SUCCESS, dw0))
}
fn err(sc: u16) -> Option<Outcome> {
    Some((sc, 0))
}

/// ASCII field, space padded (cut to `n`).
fn pad(s: &str, n: usize) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.resize(n, b' ');
    v
}

impl Nvme {
    pub fn new(slot: u8, ns: Vec<Namespace>, mem: GuestMemory, intx: Arc<Intx>, msi: Arc<dyn MsiSender>) -> Self {
        let mut cfg = ConfigSpace::new(
            PciId { vendor: 0x1b36, device: 0x0010, class: 0x01_08_02, revision: 2, subsys_vendor: 0x1b36, subsys: 0 },
            1,
        );
        cfg.add_bar(0, BarSpec { size: BAR_SIZE, is64: true, prefetch: false });
        let msix = Msix::new(&mut cfg, MAX_QUEUES, 0, MSIX_TABLE, MSIX_PBA, msi);
        Nvme {
            id: format!("nvme@{slot:02x}"),
            cfg,
            msix,
            intx,
            mem,
            ns: ns.into(),
            serial: format!("VRM{slot:04}"),
            model: "vroom NVMe".into(),
            firmware: "1.0".into(),
            regs: Regs::default(),
            sqs: BTreeMap::new(),
            cqs: BTreeMap::new(),
            aer: Vec::new(),
            me: Weak::new(),
            io: Arc::default(),
            pool: None,
            policy: Default::default(),
        }
    }

    /// Put it behind its lock, ready for I/O threads.
    pub fn into_shared(self) -> Arc<Mutex<Nvme>> {
        Arc::new_cyclic(|me| {
            let mut n = self;
            n.me = me.clone();
            Mutex::new(n)
        })
    }

    /// Names the guest sees (from the VM's `[identity]`).
    pub fn with_ids(mut self, model: &str, serial: &str, firmware: &str) -> Self {
        self.model = model.to_string();
        self.serial = serial.to_string();
        self.firmware = firmware.to_string();
        self
    }

    /// PCI IDs; Identify reports the same vendor and subsystem vendor.
    pub fn with_pci_ids(mut self, ids: VendorIds) -> Self {
        self.cfg.set_vendor_ids(ids);
        self
    }

    fn cap(&self) -> u64 {
        MQES as u64 | 1 << 16 | 0x20 << 24 | 1 << 37
    }

    fn reset(&mut self) {
        self.drain_io();
        self.sqs.clear();
        self.cqs.clear();
        self.aer.clear();
        self.regs.csts = 0;
        self.intx.set(false);
    }

    fn enable(&mut self) {
        let asqs = (self.regs.aqa & 0xfff) as u16 + 1;
        let acqs = ((self.regs.aqa >> 16) & 0xfff) as u16 + 1;
        let ok = asqs >= 2
            && acqs >= 2
            && self.regs.asq.is_multiple_of(PAGE)
            && self.regs.acq.is_multiple_of(PAGE)
            && self.mem.check_range(GuestAddress(self.regs.asq), asqs as u64 * 64)
            && self.mem.check_range(GuestAddress(self.regs.acq), acqs as u64 * 16);
        if !ok {
            self.regs.csts = 2; // CFS: fatal
            return;
        }
        self.cqs.insert(0, Cq { base: self.regs.acq, size: acqs, head: 0, tail: 0, phase: true, ien: true, iv: 0, reserved: 0 });
        self.sqs.insert(0, Sq { base: self.regs.asq, size: asqs, head: 0, tail: 0, cqid: 0 });
        self.regs.csts = 1;
    }

    fn write_cc(&mut self, v: u32) {
        let old = self.regs.cc;
        self.regs.cc = v;
        if old & 1 == 0 && v & 1 == 1 {
            self.enable();
        } else if old & 1 == 1 && v & 1 == 0 {
            self.reset();
        }
        if (v >> 14) & 3 != 0 {
            // Shutdown notification: make data durable.
            self.drain_io();
            for n in self.ns.iter() {
                let _ = n.disk.flush();
            }
            self.regs.csts = (self.regs.csts & !0xc) | (2 << 2);
        }
    }

    fn doorbell(&mut self, off: u64, v: u32) {
        if self.regs.csts & 1 == 0 {
            return;
        }
        let idx = (off - DOORBELL) / 4;
        let qid = (idx / 2) as u16;
        if idx.is_multiple_of(2) {
            let Some(sq) = self.sqs.get_mut(&qid) else { return };
            if v >= sq.size as u32 {
                return;
            }
            sq.tail = v as u16;
            self.process(qid);
        } else {
            let Some(cq) = self.cqs.get_mut(&qid) else { return };
            if v >= cq.size as u32 {
                return;
            }
            cq.head = v as u16;
            // Room freed: queues feeding this CQ may continue.
            let feeders: Vec<u16> = self.sqs.iter().filter(|(_, s)| s.cqid == qid).map(|(k, _)| *k).collect();
            for q in feeders {
                self.process(q);
            }
            self.update_intx();
        }
    }

    fn process(&mut self, qid: u16) {
        // Completion queues that got entries: one interrupt each at the
        // end, not one per command (the driver reaps them all at once).
        let mut posted = 0u64;
        // Bounded: at most one pass over the queue per doorbell.
        for _ in 0..MQES as usize + 1 {
            let Some(sq) = self.sqs.get(&qid) else { return };
            if sq.head == sq.tail {
                break;
            }
            let cqid = sq.cqid;
            if self.cqs.get(&cqid).is_none_or(|c| c.full()) {
                break;
            }
            let mut raw = [0u8; 64];
            let at = GuestAddress(sq.base + sq.head as u64 * 64);
            let read_ok = self.mem.read(at, &mut raw).is_ok();
            let sq = self.sqs.get_mut(&qid).expect("checked above");
            sq.head = (sq.head + 1) % sq.size;
            let head = sq.head;
            let more = sq.head != sq.tail;
            let cmd = Cmd::parse(&raw);
            let out = if !read_ok {
                Some((SC_DATA_XFER_ERROR, 0))
            } else if qid == 0 {
                self.admin(&cmd)
            } else if self.dispatch(Job { cmd, sqid: qid, cqid, sq_head: head }, more) {
                None
            } else {
                Some(self.io(&cmd))
            };
            if let Some((sc, dw0)) = out {
                self.complete(cqid, qid, head, cmd.cid, sc, dw0);
                // Queue ids are below MAX_QUEUES (64).
                posted |= 1 << (cqid % 64);
            }
        }
        while posted != 0 {
            let cqid = posted.trailing_zeros() as u16;
            posted &= posted - 1;
            self.interrupt(cqid);
        }
    }

    /// Hand an I/O command to the I/O threads if that pays: the guest
    /// keeps several in flight, it's big (the vCPU would wait for it), or
    /// it's a flush (milliseconds). A small one while the guest sends one
    /// at a time runs here: no thread wakeups, about three times the IOPS
    /// at queue depth 1. False: run it here.
    fn dispatch(&mut self, job: Job, more: bool) -> bool {
        let running = self.io.done.lock().unwrap_or_else(|e| e.into_inner()).running;
        let c = &job.cmd;
        let bs = c.nsid.checked_sub(1).and_then(|i| self.ns.get(i as usize)).map_or(512, |n| n.block_size);
        let big = matches!(c.opc, 0x01 | 0x02) && ((c.cdw[2] & 0xffff) as u64 + 1) * bs >= ASYNC_BYTES;
        let inline = self.policy.run_inline(more || running > 0) && !big && c.opc != 0x00;
        if inline || running >= MAX_INFLIGHT || self.me.strong_count() == 0 {
            return false;
        }
        if self.pool.is_none() {
            match self.start_pool() {
                Ok(p) => self.pool = Some(p),
                Err(e) => {
                    log::warn!("{}: no I/O threads ({e}), running commands inline", self.id);
                    return false;
                }
            }
        }
        let Some(tx) = self.pool.as_ref().and_then(|p| p.jobs.clone()) else { return false };
        let Some(cq) = self.cqs.get_mut(&job.cqid) else { return false };
        cq.reserved += 1;
        let running = {
            let mut d = self.io.done.lock().unwrap_or_else(|e| e.into_inner());
            d.running += 1;
            d.running
        };
        if let Some(pool) = self.pool.as_mut() {
            if let Err(e) = pool.grow_to(running) {
                log::debug!("{}: no more I/O threads ({e})", self.id);
            }
        }
        if tx.send(job).is_err() {
            // Can't happen while we hold the pool; undo and run inline.
            self.io.done.lock().unwrap_or_else(|e| e.into_inner()).running -= 1;
            if let Some(cq) = self.cqs.get_mut(&job.cqid) {
                cq.reserved -= 1;
            }
            return false;
        }
        true
    }

    fn start_pool(&self) -> std::io::Result<Pool> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (posted, posted_rx) = mpsc::channel::<()>();
        let rx = Arc::new(Mutex::new(rx));
        let template = IoThread { rx, io: self.io.clone(), mem: self.mem.clone(), ns: self.ns.clone(), me: self.me.clone(), posted };
        let mut pool = Pool { jobs: Some(tx), threads: Vec::new(), template };
        pool.grow_to(IO_THREADS)?;
        let me = self.me.clone();
        std::thread::Builder::new().name("nvme-done".into()).spawn(move || {
            while posted_rx.recv().is_ok() {
                while posted_rx.try_recv().is_ok() {}
                let Some(n) = me.upgrade() else { return };
                n.lock().unwrap_or_else(|e| e.into_inner()).post_done();
            }
        })?;
        Ok(pool)
    }

    /// Post the results the I/O threads finished.
    fn post_done(&mut self) {
        let list = std::mem::take(&mut self.io.done.lock().unwrap_or_else(|e| e.into_inner()).list);
        let mut posted = 0u64;
        for (j, (sc, dw0)) in list {
            let Some(cq) = self.cqs.get_mut(&j.cqid) else { continue };
            cq.reserved = cq.reserved.saturating_sub(1);
            self.complete(j.cqid, j.sqid, j.sq_head, j.cmd.cid, sc, dw0);
            posted |= 1 << (j.cqid % 64);
        }
        while posted != 0 {
            let cqid = posted.trailing_zeros() as u16;
            posted &= posted - 1;
            self.interrupt(cqid);
        }
    }

    /// Wait for the I/O threads and post what they did.
    fn drain_io(&mut self) {
        if self.pool.is_none() {
            return;
        }
        let mut d = self.io.done.lock().unwrap_or_else(|e| e.into_inner());
        while d.running > 0 {
            d = self.io.idle.wait(d).unwrap_or_else(|e| e.into_inner());
        }
        drop(d);
        self.post_done();
    }

    /// Signal that completion queue `cqid` has new entries.
    fn interrupt(&mut self, cqid: u16) {
        let Some(cq) = self.cqs.get(&cqid) else { return };
        let (ien, iv) = (cq.ien, cq.iv);
        if ien {
            if self.msix.enabled() {
                self.msix.notify(iv);
            } else {
                self.update_intx();
            }
        }
    }

    /// Post a completion entry (the interrupt comes separately).
    fn complete(&mut self, cqid: u16, sqid: u16, sq_head: u16, cid: u16, sc: u16, dw0: u32) {
        // The SQ's head now, not when this command was fetched: commands
        // done on the I/O threads finish out of order, and a head going
        // back makes the driver think its queue is full (Windows then
        // waits for room forever).
        let sq_head = self.sqs.get(&sqid).map_or(sq_head, |s| s.head);
        let Some(cq) = self.cqs.get_mut(&cqid) else { return };
        let mut e = [0u8; 16];
        e[0..4].copy_from_slice(&dw0.to_le_bytes());
        e[8..10].copy_from_slice(&sq_head.to_le_bytes());
        e[10..12].copy_from_slice(&sqid.to_le_bytes());
        e[12..14].copy_from_slice(&cid.to_le_bytes());
        // Status field: SC bits 1..9, SCT bits 9..12, DNR bit 15 (of the
        // upper half). Every error but a host I/O error comes back the
        // same if the driver tries again: say so, or it retries forever
        // (Windows does, on a write-protected namespace).
        let dnr = sc != SC_SUCCESS && sc & !DNR != SC_DATA_XFER_ERROR;
        let status = ((sc & 0xff) << 1) | ((sc >> 8) & 7) << 9 | (dnr as u16) << 15 | cq.phase as u16;
        e[14..16].copy_from_slice(&status.to_le_bytes());
        let _ = self.mem.write(GuestAddress(cq.base + cq.tail as u64 * 16), &e);
        cq.tail = (cq.tail + 1) % cq.size;
        if cq.tail == 0 {
            cq.phase = !cq.phase;
        }
    }

    fn update_intx(&self) {
        if self.msix.enabled() || self.cfg.intx_disabled() {
            self.intx.set(false);
            return;
        }
        let pending = self.cqs.values().any(|c| c.ien && c.pending()) && self.regs.intms & 1 == 0;
        self.intx.set(pending);
    }

    fn to_guest(&self, prp1: u64, prp2: u64, data: &[u8]) -> Result<(), u16> {
        let mut off = 0usize;
        for (a, n) in prps(&self.mem, prp1, prp2, data.len() as u64)? {
            self.mem.write(GuestAddress(a), &data[off..off + n as usize]).map_err(|_| SC_DATA_XFER_ERROR)?;
            off += n as usize;
        }
        Ok(())
    }

    fn admin(&mut self, c: &Cmd) -> Option<Outcome> {
        match c.opc {
            // Queue deletion: let commands still running finish first.
            0x00 | 0x04 => self.drain_io(),
            _ => {}
        }
        self.admin_cmd(c)
    }

    fn admin_cmd(&mut self, c: &Cmd) -> Option<Outcome> {
            match c.opc {
            0x00 => self.delete_sq(c.cdw[0] as u16),
            0x04 => self.delete_cq(c.cdw[0] as u16),
            0x01 => self.create_sq(c),
            0x05 => self.create_cq(c),
            0x02 => self.get_log(c),
            0x06 => self.identify(c),
            0x08 => ok(1), // abort: "not aborted"
            0x09 | 0x0a => self.features(c),
            0x0c => {
                if self.aer.len() >= 4 {
                    return err(0x105); // AER limit exceeded
                }
                self.aer.push(c.cid);
                None
            }
            0x18 => ok(0), // keep alive
            _ => err(SC_INVALID_OPCODE),
        }
    }

    fn create_cq(&mut self, c: &Cmd) -> Option<Outcome> {
        let qid = c.cdw[0] as u16;
        let size = (c.cdw[0] >> 16) as u16 as u32 + 1;
        let (pc, ien, iv) = (c.cdw[1] & 1, c.cdw[1] & 2 != 0, (c.cdw[1] >> 16) as u16);
        if qid == 0 || qid >= MAX_QUEUES || self.cqs.contains_key(&qid) {
            return err(SC_INVALID_QID);
        }
        if size < 2 || size > MQES as u32 + 1 || pc == 0 {
            return err(SC_INVALID_QSIZE);
        }
        if iv as usize >= self.msix.vectors() {
            return err(SC_INVALID_VECTOR);
        }
        if !c.prp1.is_multiple_of(PAGE) || !self.mem.check_range(GuestAddress(c.prp1), size as u64 * 16) {
            return err(SC_INVALID_FIELD);
        }
        self.cqs.insert(qid, Cq { base: c.prp1, size: size as u16, head: 0, tail: 0, phase: true, ien, iv, reserved: 0 });
        ok(0)
    }

    fn create_sq(&mut self, c: &Cmd) -> Option<Outcome> {
        let qid = c.cdw[0] as u16;
        let size = (c.cdw[0] >> 16) as u16 as u32 + 1;
        let cqid = (c.cdw[1] >> 16) as u16;
        if qid == 0 || qid >= MAX_QUEUES || self.sqs.contains_key(&qid) {
            return err(SC_INVALID_QID);
        }
        if size < 2 || size > MQES as u32 + 1 || c.cdw[1] & 1 == 0 {
            return err(SC_INVALID_QSIZE);
        }
        if cqid == 0 || !self.cqs.contains_key(&cqid) {
            return err(SC_INVALID_CQ);
        }
        if !c.prp1.is_multiple_of(PAGE) || !self.mem.check_range(GuestAddress(c.prp1), size as u64 * 64) {
            return err(SC_INVALID_FIELD);
        }
        self.sqs.insert(qid, Sq { base: c.prp1, size: size as u16, head: 0, tail: 0, cqid });
        ok(0)
    }

    fn delete_sq(&mut self, qid: u16) -> Option<Outcome> {
        if qid == 0 || self.sqs.remove(&qid).is_none() {
            return err(SC_INVALID_QID);
        }
        ok(0)
    }

    fn delete_cq(&mut self, qid: u16) -> Option<Outcome> {
        if qid == 0 || !self.cqs.contains_key(&qid) {
            return err(SC_INVALID_QID);
        }
        if self.sqs.values().any(|s| s.cqid == qid) {
            return err(0x10c); // invalid queue deletion
        }
        self.cqs.remove(&qid);
        ok(0)
    }

    fn features(&mut self, c: &Cmd) -> Option<Outcome> {
        let fid = c.cdw[0] as u8;
        let set = c.opc == 0x09;
        match fid {
            // Number of queues: grant what we have (zero-based counts).
            0x07 => {
                let n = (MAX_QUEUES - 2) as u32;
                ok(n | n << 16)
            }
            0x06 => ok(1), // volatile write cache enabled
            0x01..=0x0b | 0x0f if set => ok(0),
            0x01..=0x0b | 0x0f => ok(0),
            _ => err(SC_INVALID_FIELD),
        }
    }

    fn get_log(&mut self, c: &Cmd) -> Option<Outcome> {
        let lid = c.cdw[0] as u8;
        let numd = (((c.cdw[0] >> 16) & 0xfff) | (c.cdw[1] & 0xffff) << 12) as u64 + 1;
        let len = (numd * 4).min(4096);
        let mut page = vec![0u8; 4096];
        match lid {
            0x01 => {}
            0x02 => {
                page[1..3].copy_from_slice(&313u16.to_le_bytes()); // 40C in Kelvin
                page[3] = 100; // available spare
                page[4] = 10;
            }
            0x03 => {
                page[0] = 1;
                page[8..16].copy_from_slice(&pad(&self.firmware, 8));
            }
            _ => return err(SC_INVALID_FIELD),
        }
        match self.to_guest(c.prp1, c.prp2, &page[..len as usize]) {
            Ok(()) => ok(0),
            Err(sc) => err(sc),
        }
    }

    fn identify(&mut self, c: &Cmd) -> Option<Outcome> {
        let cns = c.cdw[0] as u8;
        let mut d = vec![0u8; 4096];
        match cns {
            0x00 => {
                let Some(n) = c.nsid.checked_sub(1).and_then(|i| self.ns.get(i as usize)) else {
                    return err(SC_INVALID_NS);
                };
                let blocks = n.disk.size() / n.block_size;
                d[0..8].copy_from_slice(&blocks.to_le_bytes());
                d[8..16].copy_from_slice(&blocks.to_le_bytes());
                d[16..24].copy_from_slice(&blocks.to_le_bytes());
                d[24] = if n.ro() { 0 } else { 1 }; // thin provisioning (deallocate)
                d[25] = 0; // one LBA format
                d[26] = 0;
                // NSATTR: write protected. Linux mounts it read-only;
                // Windows doesn't look (nor at namespace write protection).
                d[99] = n.ro() as u8;
                d[128 + 2] = n.block_size.trailing_zeros() as u8; // LBAF0: block size
            }
            0x01 => {
                let ids = self.cfg.vendor_ids();
                d[0..2].copy_from_slice(&ids.vendor.to_le_bytes());
                d[2..4].copy_from_slice(&ids.subsys_vendor.to_le_bytes());
                d[4..24].copy_from_slice(&pad(&self.serial, 20));
                d[24..64].copy_from_slice(&pad(&self.model, 40));
                d[64..72].copy_from_slice(&pad(&self.firmware, 8));
                d[72] = 6;
                d[77] = MDTS;
                d[80..84].copy_from_slice(&0x0001_0400u32.to_le_bytes());
                d[258] = 3; // abort limit
                d[259] = 3; // AER limit
                d[260] = 0x02; // firmware: 1 slot, read-only
                d[261] = 0x02; // log page attributes
                d[512] = 0x66; // SQ entry 64 bytes
                d[513] = 0x44; // CQ entry 16 bytes
                d[516..520].copy_from_slice(&(self.ns.len() as u32).to_le_bytes());
                d[520..522].copy_from_slice(&0x000cu16.to_le_bytes()); // DSM + write zeroes
                d[525] = 1; // volatile write cache
                let nqn = format!("nqn.2014-08.org.nvmexpress:{}", self.serial);
                d[768..768 + nqn.len()].copy_from_slice(nqn.as_bytes());
            }
            0x02 => {
                let mut o = 0;
                for i in 0..self.ns.len() as u32 {
                    if i + 1 > c.nsid && o < 4096 {
                        d[o..o + 4].copy_from_slice(&(i + 1).to_le_bytes());
                        o += 4;
                    }
                }
            }
            0x03 => {
                if c.nsid == 0 || c.nsid as usize > self.ns.len() {
                    return err(SC_INVALID_NS);
                }
                // NIDT 3 = UUID, derived from serial + nsid (stable).
                d[0] = 3;
                d[1] = 16;
                let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                for b in self.serial.bytes().chain(c.nsid.to_le_bytes()) {
                    h ^= b as u64;
                    h = h.wrapping_mul(0x100_0000_01b3);
                }
                d[4..12].copy_from_slice(&h.to_le_bytes());
                d[12..20].copy_from_slice(&h.rotate_left(17).to_le_bytes());
            }
            _ => return err(SC_INVALID_FIELD),
        }
        match self.to_guest(c.prp1, c.prp2, &d) {
            Ok(()) => ok(0),
            Err(sc) => err(sc),
        }
    }

    fn io(&mut self, c: &Cmd) -> Outcome {
        exec_io(&self.mem, &self.ns, c)
    }

    fn reg_read(&self, off: u64) -> u64 {
        match off {
            0x00 => self.cap(),
            0x08 => 0x0001_0400,
            0x0c | 0x10 => self.regs.intms as u64,
            0x14 => self.regs.cc as u64,
            0x1c => self.regs.csts as u64,
            0x24 => self.regs.aqa as u64,
            0x28 => self.regs.asq,
            0x30 => self.regs.acq,
            _ => 0,
        }
    }

    fn reg_write(&mut self, off: u64, v: u64, len: usize) {
        match off {
            0x0c => {
                self.regs.intms |= v as u32;
                self.update_intx();
            }
            0x10 => {
                self.regs.intms &= !(v as u32);
                self.update_intx();
            }
            0x14 => self.write_cc(v as u32),
            0x24 => self.regs.aqa = v as u32 & 0x0fff_0fff,
            0x28 => set_part(&mut self.regs.asq, v, false, len),
            0x2c => set_part(&mut self.regs.asq, v, true, len),
            0x30 => set_part(&mut self.regs.acq, v, false, len),
            0x34 => set_part(&mut self.regs.acq, v, true, len),
            _ => {}
        }
    }
}

/// Guest buffer described by PRP1/PRP2 for `len` bytes.
fn prps(mem: &GuestMemory, prp1: u64, prp2: u64, len: u64) -> Result<Vec<(u64, u64)>, u16> {
    if len == 0 || len > MAX_XFER {
        return Err(SC_INVALID_FIELD);
    }
    let mut segs = Vec::new();
    let first = (PAGE - prp1 % PAGE).min(len);
    segs.push((prp1, first));
    let mut left = len - first;
    if left == 0 {
        return Ok(segs);
    }
    if left <= PAGE {
        if !prp2.is_multiple_of(PAGE) {
            return Err(SC_PRP_OFFSET);
        }
        segs.push((prp2, left));
        return Ok(segs);
    }
    // PRP list (may chain via its last entry).
    let mut list = prp2;
    if !list.is_multiple_of(8) {
        return Err(SC_PRP_OFFSET);
    }
    let per_page = (PAGE - list % PAGE) / 8;
    let mut i = 0;
    let mut entries_left_on_page = per_page;
    while left > 0 {
        if segs.len() > (MAX_XFER / PAGE) as usize + 2 {
            return Err(SC_INVALID_FIELD);
        }
        let e: u64 = mem.read_obj(GuestAddress(list + i * 8)).map_err(|_| SC_DATA_XFER_ERROR)?;
        let e = u64::from_le(e);
        if entries_left_on_page == 1 && left > PAGE {
            // Last slot points at the next list page.
            if !e.is_multiple_of(PAGE) {
                return Err(SC_PRP_OFFSET);
            }
            list = e;
            i = 0;
            entries_left_on_page = PAGE / 8;
            continue;
        }
        if !e.is_multiple_of(PAGE) {
            return Err(SC_PRP_OFFSET);
        }
        let n = left.min(PAGE);
        segs.push((e, n));
        left -= n;
        i += 1;
        entries_left_on_page -= 1;
    }
    Ok(segs)
}

/// The PRPs as guest buffers for the disk (adjacent pages merged).
/// `prps` caps `len` at MAX_XFER.
fn guest_segs(mem: &GuestMemory, prp1: u64, prp2: u64, len: u64) -> Result<Vec<(GuestAddress, usize)>, u16> {
    let mut out: Vec<(GuestAddress, usize)> = Vec::new();
    for (a, n) in prps(mem, prp1, prp2, len)? {
        match out.last_mut() {
            Some((last, l)) if last.0 + *l as u64 == a => *l += n as usize,
            _ => out.push((GuestAddress(a), n as usize)),
        }
    }
    Ok(out)
}

fn read_prps(mem: &GuestMemory, prp1: u64, prp2: u64, len: u64) -> Result<Vec<u8>, u16> {
    let mut v = vec![0u8; len as usize];
    let mut off = 0usize;
    for (a, n) in prps(mem, prp1, prp2, len)? {
        mem.read(GuestAddress(a), &mut v[off..off + n as usize]).map_err(|_| SC_DATA_XFER_ERROR)?;
        off += n as usize;
    }
    Ok(v)
}

#[derive(Clone)]
struct IoThread {
    rx: Arc<Mutex<Receiver<Job>>>,
    io: Arc<IoShared>,
    mem: GuestMemory,
    ns: Arc<[Namespace]>,
    me: Weak<Mutex<Nvme>>,
    posted: Sender<()>,
}

impl IoThread {
    fn run(self) {
        loop {
            let job = self.rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
            let Ok(job) = job else { return };
            let out = exec_io(&self.mem, &self.ns, &job.cmd);
            let mut d = self.io.done.lock().unwrap_or_else(|e| e.into_inner());
            d.list.push((job, out));
            d.running -= 1;
            if d.running == 0 {
                self.io.idle.notify_all();
            }
            drop(d);
            // Post it ourselves if the controller is free (saves a thread
            // wakeup); else the completion thread does, once it's free.
            if let Some(n) = self.me.upgrade() {
                if let Ok(mut g) = n.try_lock() {
                    g.post_done();
                    continue;
                }
            }
            let _ = self.posted.send(());
        }
    }
}

/// Run an NVM command set command (read, write, flush...).
fn exec_io(mem: &GuestMemory, ns: &[Namespace], c: &Cmd) -> Outcome {

    let Some(n) = c.nsid.checked_sub(1).and_then(|i| ns.get(i as usize)) else {
        return (SC_INVALID_NS, 0);
    };
    let disk = n.disk.clone();
    let bs = n.block_size;
    let ro = n.ro();
    let slba = c.cdw[0] as u64 | (c.cdw[1] as u64) << 32;
    let nlb = (c.cdw[2] & 0xffff) as u64 + 1;
    let range = || -> Result<(u64, u64), u16> {
        let off = slba.checked_mul(bs).ok_or(SC_LBA_RANGE)?;
        let len = nlb * bs;
        vroom_block::check_range(disk.size(), off, len).map_err(|_| SC_LBA_RANGE)?;
        Ok((off, len))
    };
    let r: Result<(), u16> = match c.opc {
        0x00 => disk.flush().map_err(|_| SC_DATA_XFER_ERROR),
        0x02 => range().and_then(|(off, len)| {
            let segs = guest_segs(mem, c.prp1, c.prp2, len)?;
            disk.read_to_guest(mem, off, &segs).map_err(|_| SC_DATA_XFER_ERROR)
        }),
        0x01 if ro => Err(SC_READ_ONLY),
        0x01 => range().and_then(|(off, len)| {
            let segs = guest_segs(mem, c.prp1, c.prp2, len)?;
            disk.write_from_guest(mem, off, &segs).map_err(|_| SC_DATA_XFER_ERROR)
        }),
        0x08 if ro => Err(SC_READ_ONLY),
        0x08 => range().and_then(|(off, len)| disk.write_zeroes(off, len).map_err(|_| SC_DATA_XFER_ERROR)),
        0x09 if ro => Err(SC_READ_ONLY),
        0x09 => dsm(mem, c, &disk, bs),
        _ => Err(SC_INVALID_OPCODE),
    };
    match r {
        Ok(()) => (SC_SUCCESS, 0),
        Err(SC_READ_ONLY) => {
            let k = REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if k.is_power_of_two() {
                log::info!("nvme: write to read-only namespace {} refused ({k} so far)", c.nsid);
            }
            (SC_READ_ONLY, 0)
        }
        Err(sc) => (sc, 0),
    }
}

/// Dataset management: only "deallocate" does anything.
fn dsm(mem: &GuestMemory, c: &Cmd, disk: &Arc<dyn DiskImage>, bs: u64) -> Result<(), u16> {
    if c.cdw[1] & 4 == 0 {
        return Ok(());
    }
    let nr = (c.cdw[0] & 0xff) as u64 + 1;
    let ranges = read_prps(mem, c.prp1, c.prp2, nr * 16)?;
    for r in ranges.as_chunks::<16>().0 {
        let len = u32::from_le_bytes([r[4], r[5], r[6], r[7]]) as u64 * bs;
        let slba = u64::from_le_bytes(r[8..16].try_into().unwrap_or([0; 8]));
        let off = slba.checked_mul(bs).ok_or(SC_LBA_RANGE)?;
        vroom_block::check_range(disk.size(), off, len).map_err(|_| SC_LBA_RANGE)?;
        let _ = disk.discard(off, len);
    }
    Ok(())
}

fn set_part(field: &mut u64, v: u64, high: bool, len: usize) {
    *field = match (len, high) {
        (8, _) => v,
        (_, false) => (*field & !0xffff_ffff) | (v & 0xffff_ffff),
        (_, true) => (*field & 0xffff_ffff) | ((v & 0xffff_ffff) << 32),
    };
}

impl PciDevice for Nvme {
    fn config(&self) -> &ConfigSpace {
        &self.cfg
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.cfg
    }

    fn config_written(&mut self, _off: usize, _len: usize) {
        self.msix.update(&self.cfg);
        self.update_intx();
    }

    fn bar_read(&mut self, _bar: usize, off: u64, data: &mut [u8]) {
        match off {
            o if o < DOORBELL => {
                // CAP, ASQ, ACQ are 64-bit: also readable as two halves.
                let v = match o {
                    0x04 | 0x2c | 0x34 => self.reg_read(o - 4) >> 32,
                    _ => self.reg_read(o),
                };
                put_le(data, v);
            }
            o if o >= MSIX_TABLE as u64 && o < MSIX_PBA as u64 => self.msix.table_read(o - MSIX_TABLE as u64, data),
            o if o >= MSIX_PBA as u64 => self.msix.pba_read(o - MSIX_PBA as u64, data),
            _ => data.fill(0),
        }
    }

    fn bar_write(&mut self, _bar: usize, off: u64, data: &[u8]) {
        let v = get_le(data);
        match off {
            o if o < DOORBELL => self.reg_write(o, v, data.len()),
            o if o < DOORBELL + 2 * MAX_QUEUES as u64 * 4 => self.doorbell(o, v as u32),
            o if o >= MSIX_TABLE as u64 && o < MSIX_PBA as u64 => self.msix.table_write(o - MSIX_TABLE as u64, data),
            _ => {}
        }
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn pause(&mut self) {
        self.drain_io();
    }

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        self.drain_io();
        for n in self.ns.iter() {
            let _ = n.disk.flush();
        }
        encode(&Snap {
            cfg: self.cfg.clone(),
            msix: self.msix.clone(),
            regs: self.regs.clone(),
            sqs: self.sqs.clone(),
            cqs: self.cqs.clone(),
            aer: self.aer.clone(),
        })
    }

    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        let s: Snap = decode(data)?;
        let bad = |m: &str| vroom_snapshot::Error::Invalid(format!("nvme: {m}"));
        let mut cfg = s.cfg;
        if !cfg.rebuild_masks(&self.cfg) || s.msix.vectors() != self.msix.vectors() {
            return Err(bad("layout"));
        }
        for q in s.sqs.values() {
            if q.size == 0 || q.head >= q.size || q.tail >= q.size || !s.cqs.contains_key(&q.cqid) {
                return Err(bad("sq"));
            }
        }
        for q in s.cqs.values() {
            if q.size == 0 || q.head >= q.size || q.tail >= q.size || q.iv as usize >= self.msix.vectors() {
                return Err(bad("cq"));
            }
        }
        let sender = self.msix.sender();
        self.cfg = cfg;
        self.msix = s.msix;
        if let Some(sn) = sender {
            self.msix.set_sender(sn);
        }
        self.msix.update(&self.cfg);
        self.regs = s.regs;
        self.sqs = s.sqs;
        self.cqs = s.cqs;
        self.aer = s.aer;
        self.update_intx();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::IntxRouter;
    use crate::NoIrq;
    use vroom_block::raw::RawImage;
    use vroom_block::SECTOR_SIZE;

    struct NoMsi;
    impl MsiSender for NoMsi {
        fn send(&self, _a: u64, _d: u32) {}
    }

    const RAM: u64 = 0x4000_0000;
    const ASQ: u64 = RAM;
    const ACQ: u64 = RAM + 0x1000;
    const BUF: u64 = RAM + 0x10000;

    fn ctrl() -> (Nvme, GuestMemory) {
        ctrl_with(|n| n)
    }

    fn ctrl_with(f: impl FnOnce(Nvme) -> Nvme) -> (Nvme, GuestMemory) {
        ctrl_ro(false, f)
    }

    fn ctrl_ro(read_only: bool, f: impl FnOnce(Nvme) -> Nvme) -> (Nvme, GuestMemory) {
        let mem = GuestMemory::new(&[(GuestAddress(RAM), 4 << 20)]).unwrap();
        let file = tempfile::tempfile().unwrap();
        file.set_len(1 << 20).unwrap();
        let disk: Arc<dyn DiskImage> = Arc::new(RawImage::from_file(file, false).unwrap());
        let r = IntxRouter::new((0..4).map(|_| Arc::new(NoIrq) as Arc<dyn crate::IrqLine>).collect());
        let mut n = f(Nvme::new(3, vec![Namespace { disk, read_only, block_size: SECTOR_SIZE }], mem.clone(), r.pin(3, 1), Arc::new(NoMsi)));
        n.reg_write(0x24, (3 << 16) | 3, 4); // 4-entry admin queues
        n.reg_write(0x28, ASQ, 8);
        n.reg_write(0x30, ACQ, 8);
        n.reg_write(0x14, 1, 4);
        assert_eq!(n.regs.csts & 1, 1);
        (n, mem)
    }

    fn submit(n: &mut Nvme, mem: &GuestMemory, sq: (u16, u64, u16), slot: u16, cmd: [u32; 16]) {
        let mut b = [0u8; 64];
        for (i, w) in cmd.iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        mem.write(GuestAddress(sq.1 + slot as u64 * 64), &b).unwrap();
        n.doorbell(DOORBELL + sq.0 as u64 * 8, ((slot + 1) % sq.2) as u32);
    }

    /// Status code and type of a completion (DNR left out).
    fn status(mem: &GuestMemory, cq: u64, slot: u16) -> u16 {
        let s: u16 = mem.read_obj(GuestAddress(cq + slot as u64 * 16 + 14)).unwrap();
        (s >> 1) & 0x7ff
    }

    fn dnr(mem: &GuestMemory, cq: u64, slot: u16) -> bool {
        let s: u16 = mem.read_obj(GuestAddress(cq + slot as u64 * 16 + 14)).unwrap();
        s & 1 << 15 != 0
    }

    #[test]
    fn identify_shows_configured_ids() {
        let ids = VendorIds { vendor: 0x144d, device: 0xa808, subsys_vendor: 0x144d, subsys: 0xa801 };
        let (mut n, mem) = ctrl_with(|n| n.with_ids("Samsung SSD 970 EVO Plus 1TB", "S4EWNX0R1234", "2B2QEXM7").with_pci_ids(ids));
        let mut id = [0u8; 4];
        n.config().read(0, &mut id);
        assert_eq!(id, [0x4d, 0x14, 0x08, 0xa8]);
        n.config().read(0x2c, &mut id);
        assert_eq!(id, [0x4d, 0x14, 0x01, 0xa8]);
        submit(&mut n, &mem, (0, ASQ, 4), 0, [6 | 1 << 16, 0, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, ACQ, 0), 0);
        let mut d = [0u8; 72];
        mem.read(GuestAddress(BUF), &mut d).unwrap();
        assert_eq!(&d[0..4], &[0x4d, 0x14, 0x4d, 0x14], "VID, SSVID");
        assert_eq!(&d[4..24], b"S4EWNX0R1234        ");
        assert!(d[24..64].starts_with(b"Samsung SSD 970 EVO Plus 1TB "));
        assert_eq!(&d[64..72], b"2B2QEXM7");
        // Firmware slot log (lid 3): slot 1 revision.
        submit(&mut n, &mem, (0, ASQ, 4), 1, [2 | 2 << 16, 0, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 3 | 127 << 16, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, ACQ, 1), 0);
        let mut fr = [0u8; 8];
        mem.read(GuestAddress(BUF + 8), &mut fr).unwrap();
        assert_eq!(&fr, b"2B2QEXM7");
    }

    #[test]
    fn identify_and_io() {
        let (mut n, mem) = ctrl();
        // Identify controller (cid 1, opc 6, cns 1).
        submit(&mut n, &mem, (0, ASQ, 4), 0, [6 | 1 << 16, 0, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, ACQ, 0), 0);
        let mut mn = [0u8; 10];
        mem.read(GuestAddress(BUF + 24), &mut mn).unwrap();
        assert_eq!(&mn, b"vroom NVMe");
        // Create CQ 1 and SQ 1 (4 entries each).
        let (cq1, sq1) = (RAM + 0x2000, RAM + 0x3000);
        submit(&mut n, &mem, (0, ASQ, 4), 1, [5 | 2 << 16, 0, 0, 0, 0, 0, cq1 as u32, 0, 0, 0, 1 | 3 << 16, 1, 0, 0, 0, 0]);
        submit(&mut n, &mem, (0, ASQ, 4), 2, [1 | 3 << 16, 0, 0, 0, 0, 0, sq1 as u32, 0, 0, 0, 1 | 3 << 16, 1 | 1 << 16, 0, 0, 0, 0]);
        assert_eq!((status(&mem, ACQ, 1), status(&mem, ACQ, 2)), (0, 0));
        // Write 8 sectors at LBA 2 from BUF, read back to BUF+0x2000.
        mem.write(GuestAddress(BUF), &[0x5a; 4096]).unwrap();
        submit(&mut n, &mem, (1, sq1, 4), 0, [1 | 10 << 16, 1, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 2, 0, 7, 0, 0, 0]);
        submit(&mut n, &mem, (1, sq1, 4), 1, [2 | 11 << 16, 1, 0, 0, 0, 0, (BUF + 0x2000) as u32, 0, 0, 0, 2, 0, 7, 0, 0, 0]);
        assert_eq!((status(&mem, cq1, 0), status(&mem, cq1, 1)), (0, 0));
        assert_eq!(mem.read_obj::<u8>(GuestAddress(BUF + 0x2000 + 4095)).unwrap(), 0x5a);
        // Out of range LBA -> LBA range error.
        submit(&mut n, &mem, (1, sq1, 4), 2, [2 | 12 << 16, 1, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 4000, 0, 7, 0, 0, 0]);
        assert_eq!(status(&mem, cq1, 2), SC_LBA_RANGE);
        // Bad namespace.
        n.cqs.get_mut(&1).unwrap().head = 3;
        submit(&mut n, &mem, (1, sq1, 4), 3, [2 | 13 << 16, 9, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, cq1, 3), SC_INVALID_NS);
    }

    /// A read-only namespace says so (NSATTR) and refuses writes for
    /// good: with a status Windows doesn't retry, and Do Not Retry (as
    /// every error but a host I/O error has).
    #[test]
    fn read_only_namespace() {
        let (mut n, mem) = ctrl_ro(true, |n| n);
        // Identify namespace 1: NSATTR bit 0.
        submit(&mut n, &mem, (0, ASQ, 4), 0, [6 | 1 << 16, 1, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, ACQ, 0), 0);
        assert!(!dnr(&mem, ACQ, 0));
        assert_eq!(mem.read_obj::<u8>(GuestAddress(BUF + 99)).unwrap() & 1, 1);
        // A feature we don't have: an error, final.
        submit(&mut n, &mem, (0, ASQ, 4), 1, [0x0a | 2 << 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0x84, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, ACQ, 1), SC_INVALID_FIELD);
        assert!(dnr(&mem, ACQ, 1));
        // A namespace that isn't there.
        submit(&mut n, &mem, (0, ASQ, 4), 2, [6 | 3 << 16, 2, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, ACQ, 2), SC_INVALID_NS);
        n.cqs.get_mut(&0).unwrap().head = 3;
        // A write: refused, and not to be retried. A read works.
        let (cq1, sq1) = (RAM + 0x2000, RAM + 0x3000);
        submit(&mut n, &mem, (0, ASQ, 4), 3, [5 | 4 << 16, 0, 0, 0, 0, 0, cq1 as u32, 0, 0, 0, 1 | 3 << 16, 1, 0, 0, 0, 0]);
        submit(&mut n, &mem, (0, ASQ, 4), 0, [1 | 5 << 16, 0, 0, 0, 0, 0, sq1 as u32, 0, 0, 0, 1 | 3 << 16, 1 | 1 << 16, 0, 0, 0, 0]);
        assert_eq!((status(&mem, ACQ, 3), status(&mem, ACQ, 0)), (0, 0));
        submit(&mut n, &mem, (1, sq1, 4), 0, [1 | 10 << 16, 1, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, cq1, 0), SC_READ_ONLY);
        assert!(dnr(&mem, cq1, 0));
        submit(&mut n, &mem, (1, sq1, 4), 1, [2 | 11 << 16, 1, 0, 0, 0, 0, BUF as u32, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(status(&mem, cq1, 1), 0);
        assert!(!dnr(&mem, cq1, 1));
    }

    /// Several commands per doorbell run on the I/O threads; pausing
    /// waits for them and posts every completion.
    #[test]
    fn parallel_io_then_pause() {
        let (n, mem) = ctrl();
        let n = n.into_shared();
        let (cq1, sq1) = (RAM + 0x2000, RAM + 0x3000);
        {
            let mut g = n.lock().unwrap();
            submit(&mut g, &mem, (0, ASQ, 4), 0, [5 | 2 << 16, 0, 0, 0, 0, 0, cq1 as u32, 0, 0, 0, 1 | 63 << 16, 1, 0, 0, 0, 0]);
            submit(&mut g, &mem, (0, ASQ, 4), 1, [1 | 3 << 16, 0, 0, 0, 0, 0, sq1 as u32, 0, 0, 0, 1 | 63 << 16, 1 | 1 << 16, 0, 0, 0, 0]);
            assert_eq!((status(&mem, ACQ, 0), status(&mem, ACQ, 1)), (0, 0));
        }
        let ring = |opc: u32, base: u16, data: u64| {
            for i in 0..32u16 {
                let slot = base + i;
                let cmd = [opc | (100 + slot as u32) << 16, 1, 0, 0, 0, 0, (data + 0x1000 * i as u64) as u32, 0, 0, 0, 8 * i as u32, 0, 7, 0, 0, 0];
                let mut b = [0u8; 64];
                for (k, w) in cmd.iter().enumerate() {
                    b[k * 4..k * 4 + 4].copy_from_slice(&w.to_le_bytes());
                }
                mem.write(GuestAddress(sq1 + slot as u64 * 64), &b).unwrap();
            }
            n.lock().unwrap().doorbell(DOORBELL + 8, ((base + 32) % 64) as u32);
        };
        let phase_set = |slot: u16| mem.read_obj::<u16>(GuestAddress(cq1 + slot as u64 * 16 + 14)).unwrap() & 1 == 1;
        for i in 0..32u64 {
            mem.write(GuestAddress(BUF + 0x1000 * i), &[i as u8 + 1; 4096]).unwrap();
        }
        ring(1, 0, BUF);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !(0..32).all(phase_set) {
            assert!(std::time::Instant::now() < end, "completions missing");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!((0..32).all(|s| status(&mem, cq1, s) == 0));
        // Reads, paused right away: everything is posted by then.
        n.lock().unwrap().doorbell(DOORBELL + 12, 32); // the guest consumed the CQ
        ring(2, 32, BUF + 0x40000);
        let mut g = n.lock().unwrap();
        g.pause();
        assert!((32..64).all(phase_set), "a paused controller has nothing in flight");
        assert_eq!(g.cqs[&1].reserved, 0);
        drop(g);
        for i in 0..32u64 {
            assert_eq!(mem.read_obj::<u8>(GuestAddress(BUF + 0x40000 + 0x1000 * i + 4095)).unwrap(), i as u8 + 1);
        }
    }

    #[test]
    fn prp_list_rules() {
        let (n, mem) = ctrl();
        // 3 pages starting mid-page: PRP1 + list with 3 entries.
        let list = RAM + 0x8000;
        for (i, p) in [0x20000u64, 0x21000, 0x22000].iter().enumerate() {
            mem.write_obj(GuestAddress(list + 8 * i as u64), &(RAM + p)).unwrap();
        }
        let segs = prps(&n.mem, RAM + 0x1f800, list, 3 * 4096).unwrap();
        assert_eq!(segs.len(), 4);
        assert_eq!(segs.iter().map(|s| s.1).sum::<u64>(), 3 * 4096);
        // Misaligned PRP2 data pointer rejected.
        assert_eq!(prps(&n.mem, RAM, RAM + 0x10, 8192), Err(SC_PRP_OFFSET));
        // Oversized transfer rejected.
        assert!(prps(&n.mem, RAM, list, MAX_XFER + 4096).is_err());
    }
}
