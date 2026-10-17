//! A USB stick: high speed, mass storage over Bulk-Only Transport (a CBW
//! on bulk OUT, the data, a CSW on bulk IN), SCSI inside, 512-byte
//! packets like a real USB 2 stick. Its blocks come from a [`Store`]: a
//! RAM disk for tests (`VROOM_USB_FAKE=disk`), or a Mac disk the app
//! opened for the VM ("Use as a disk", [`crate::rawdisk`]). Guests drive
//! it with their stock storage driver (Linux `usb-storage`, Windows'
//! `USBSTOR`, UEFI's `UsbMassStorageDxe`), and can partition, format and
//! write it.
//!
//! It goes through the same paths a real stick does: short IN transfers
//! (a reply shorter than asked for, then the CSW with a residue), STALLs
//! on failed commands and the guest's CLEAR_FEATURE(ENDPOINT_HALT), the
//! class requests. `VROOM_USB_FAKE=disk-broken` fails every bulk transfer
//! instead, like a stick the host won't let go of: guests must get past
//! it.
//!
//! The guest is hostile: every block number and length is checked against
//! the store's size before it is used, and a transfer never moves more
//! than the guest's CBW announced.

use std::collections::VecDeque;
use std::io;

use crate::desc::Config;
use crate::helper::{Device, Opener, Sink};
use crate::proto::{Done, Event, IsoSubmit, Speed, Status, Submit, XferType};

const EP_IN: u8 = 0x81;
const EP_OUT: u8 = 0x02;
/// Bulk packet size (high speed).
const PACKET: usize = 512;
/// The test stick: 128 MiB, zero until written (the allocator hands out
/// untouched pages).
pub const DISK_BYTES: usize = 128 << 20;
const CBW_SIG: u32 = 0x4342_5355;
const CSW_SIG: u32 = 0x5342_5355;

// Sense keys and additional sense codes.
const MEDIUM_ERROR: u8 = 0x03;
const ILLEGAL: u8 = 0x05;
const DATA_PROTECT: u8 = 0x07;
const ASC_READ_ERROR: u8 = 0x11;
const ASC_WRITE_ERROR: u8 = 0x0c;
const ASC_BAD_OPCODE: u8 = 0x20;
const ASC_LBA_RANGE: u8 = 0x21;
const ASC_BAD_FIELD: u8 = 0x24;
const ASC_WRITE_PROTECTED: u8 = 0x27;
const ASC_NO_MEDIUM: u8 = 0x3a;
const NOT_READY: u8 = 0x02;

/// Where a stick's blocks live.
pub trait Store: Send {
    /// Bytes per block: a power of two from 512 to 4096.
    fn block_size(&self) -> u32;
    fn blocks(&self) -> u64;
    /// Refuse writes (the disk was opened read-only).
    fn readonly(&self) -> bool;
    /// Whole blocks: `off` and `buf.len()` are multiples of the block
    /// size, and inside the disk (the caller checked).
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, off: u64, data: &[u8]) -> io::Result<()>;
    /// Make what was written stay (the guest's SYNCHRONIZE CACHE, and
    /// before the store is let go).
    fn flush(&mut self) -> io::Result<()>;
    /// The disk is gone (unplugged from the host): the guest sees the
    /// stick unplugged too.
    fn gone(&self) -> bool {
        false
    }
}

/// The test stick's RAM disk.
#[derive(Debug)]
pub struct Ram(pub Vec<u8>);

impl Store for Ram {
    fn block_size(&self) -> u32 {
        512
    }
    fn blocks(&self) -> u64 {
        (self.0.len() / 512) as u64
    }
    fn readonly(&self) -> bool {
        false
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let o = off as usize;
        buf.copy_from_slice(self.0.get(o..o + buf.len()).ok_or_else(|| io::Error::other("past the end"))?);
        Ok(())
    }
    fn write_at(&mut self, off: u64, data: &[u8]) -> io::Result<()> {
        let o = off as usize;
        self.0.get_mut(o..o + data.len()).ok_or_else(|| io::Error::other("past the end"))?.copy_from_slice(data);
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What the stick says it is: its USB ids and strings, and from them its
/// SCSI INQUIRY names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ids {
    pub vendor: u16,
    pub product: u16,
    pub manufacturer: String,
    pub name: String,
    pub serial: String,
}

impl Ids {
    /// The pretend stick's.
    pub fn test(vendor: u16, product: u16) -> Ids {
        Ids { vendor, product, manufacturer: "vroom".into(), name: "Test USB disk".into(), serial: "0001VROOMDISK".into() }
    }

    /// `s` as a fixed-size SCSI field: printable ASCII, padded with spaces.
    fn field(s: &str, n: usize) -> Vec<u8> {
        let mut v: Vec<u8> = s.bytes().filter(|b| (0x20..0x7f).contains(b)).take(n).collect();
        v.resize(n, b' ');
        v
    }
}

/// What the next bulk transfer is for.
#[derive(Debug)]
enum Phase {
    /// Waiting for a command (bulk OUT).
    Command,
    /// Sending a reply.
    Reply { data: Vec<u8>, at: usize },
    /// Sending disk bytes from `off`, `left` of them.
    Read { off: u64, left: u64 },
    /// Taking disk bytes to `off`, `left` of them; `keep`: false when the
    /// command failed (the data is dropped).
    Write { off: u64, left: u64, keep: bool },
    /// The status (CSW) is next on bulk IN.
    Status,
}

pub struct Disk {
    sink: Sink,
    ids: Ids,
    store: Box<dyn Store>,
    bs: u64,
    config: u8,
    phase: Phase,
    tag: u32,
    /// What the host asked to move, and what did move (for the residue).
    asked: u32,
    moved: u32,
    failed: bool,
    /// Sense data for REQUEST SENSE: key, additional code.
    sense: (u8, u8),
    /// Bulk IN transfers waiting for something to send: (id, length).
    reads: VecDeque<(u32, u32)>,
    halted_in: bool,
    broken: bool,
    /// Written bytes not yet a whole block, starting at `wbuf_at`.
    wbuf: Vec<u8>,
    wbuf_at: u64,
}

impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disk").field("ids", &self.ids).field("blocks", &self.store.blocks()).finish_non_exhaustive()
    }
}

impl Disk {
    /// The pretend stick: a RAM disk.
    pub fn new(sink: Sink, vendor: u16, product: u16, broken: bool) -> Disk {
        let mut d = Disk::with_store(sink, Ids::test(vendor, product), Box::new(Ram(vec![0; DISK_BYTES])));
        d.broken = broken;
        d
    }

    pub fn with_store(sink: Sink, ids: Ids, store: Box<dyn Store>) -> Disk {
        let bs = store.block_size() as u64;
        Disk {
            sink,
            ids,
            store,
            bs,
            config: 0,
            phase: Phase::Command,
            tag: 0,
            asked: 0,
            moved: 0,
            failed: false,
            sense: (0, 0),
            reads: VecDeque::new(),
            halted_in: false,
            broken: false,
            wbuf: Vec::new(),
            wbuf_at: 0,
        }
    }

    fn device_descriptor(&self) -> Vec<u8> {
        let mut d = vec![18, 1, 0x00, 0x02, 0, 0, 0, 64];
        d.extend_from_slice(&self.ids.vendor.to_le_bytes());
        d.extend_from_slice(&self.ids.product.to_le_bytes());
        d.extend_from_slice(&[0x00, 0x01, 1, 2, 3, 1]);
        d
    }

    /// One interface: mass storage, SCSI, Bulk-Only; bulk IN and OUT of
    /// 512 bytes (high speed).
    pub fn config_descriptor() -> Vec<u8> {
        let mut c = vec![9, 2, 32, 0, 1, 1, 0, 0x80, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 2, 0x08, 0x06, 0x50, 0]);
        c.extend_from_slice(&[7, 5, EP_IN, 2, 0x00, 0x02, 0]);
        c.extend_from_slice(&[7, 5, EP_OUT, 2, 0x00, 0x02, 0]);
        c
    }

    fn send(&self, id: u32, status: Status, data: Vec<u8>, actual: u32) {
        let _ = self.sink.send(Event::Done(Done { id, status, actual, data }));
    }

    /// A USB string: printable, short (a descriptor holds 126 characters).
    fn string(s: &str) -> Vec<u8> {
        let t: String = s.chars().filter(|c| !c.is_control()).take(64).collect();
        super::fake::string_desc(if t.is_empty() { "-" } else { &t })
    }

    fn control(&mut self, s: &Submit) -> Result<Vec<u8>, Status> {
        let (rt, req) = (s.setup[0], s.setup[1]);
        let value = u16::from_le_bytes([s.setup[2], s.setup[3]]);
        Ok(match (rt, req) {
            (0x80, 6) => match (value >> 8) as u8 {
                1 => self.device_descriptor(),
                2 if value & 0xff == 0 => Self::config_descriptor(),
                3 => match value as u8 {
                    0 => vec![4, 3, 0x09, 0x04],
                    1 => Self::string(&self.ids.manufacturer),
                    2 => Self::string(&self.ids.name),
                    3 => Self::string(&self.ids.serial),
                    _ => return Err(Status::Stall),
                },
                _ => return Err(Status::Stall),
            },
            (0x80..=0x82, 0) => vec![0, 0],
            (0x80, 8) => vec![self.config],
            (0x81, 10) => vec![0],
            // Bulk-Only: GET MAX LUN (one LUN), mass storage reset.
            (0xa1, 0xfe) => vec![0],
            (0x21, 0xff) => {
                self.end_write();
                self.phase = Phase::Command;
                vec![]
            }
            _ => return Err(Status::Stall),
        })
    }

    /// A command block from the host.
    fn command(&mut self, cbw: &[u8]) -> Result<(), Status> {
        if cbw.len() != 31 || u32::from_le_bytes([cbw[0], cbw[1], cbw[2], cbw[3]]) != CBW_SIG {
            // Not a valid CBW: the spec has the device stall until a
            // reset; stalling this transfer is enough.
            return Err(Status::Stall);
        }
        self.tag = u32::from_le_bytes([cbw[4], cbw[5], cbw[6], cbw[7]]);
        let len = u32::from_le_bytes([cbw[8], cbw[9], cbw[10], cbw[11]]);
        let dev_to_host = cbw[12] & 0x80 != 0;
        let n = (cbw[14] & 0x1f).clamp(1, 16) as usize;
        let cdb = cbw[15..15 + n].to_vec();
        (self.asked, self.moved, self.failed) = (len, 0, false);
        let r = self.scsi(&cdb, len, dev_to_host);
        self.phase = match r {
            Ok(p) => p,
            Err(sense) => {
                self.sense = sense;
                self.failed = true;
                match (len, dev_to_host) {
                    (0, _) => Phase::Status,
                    // Nothing to send: the IN pipe stalls, the host
                    // clears it and reads the status.
                    (_, true) => {
                        self.halted_in = true;
                        Phase::Status
                    }
                    (l, false) => Phase::Write { off: 0, left: l as u64, keep: false },
                }
            }
        };
        if let Phase::Write { off, keep: true, .. } = self.phase {
            self.wbuf.clear();
            self.wbuf_at = off;
        }
        if !self.failed {
            self.sense = (0, 0);
        }
        Ok(())
    }

    /// Run one SCSI command: the phase that follows, or the sense data of
    /// a failure (key, additional sense code).
    fn scsi(&mut self, cdb: &[u8], len: u32, dev_to_host: bool) -> Result<Phase, (u8, u8)> {
        let (blocks, bs) = (self.store.blocks(), self.bs);
        let wp = self.store.readonly();
        if self.store.gone() && cdb[0] != 0x03 && cdb[0] != 0x12 {
            return Err((NOT_READY, ASC_NO_MEDIUM));
        }
        let reply = |v: Vec<u8>| {
            if len == 0 {
                Ok(Phase::Status)
            } else if !dev_to_host {
                Err((ILLEGAL, ASC_BAD_FIELD))
            } else {
                Ok(Phase::Reply { data: v, at: 0 })
            }
        };
        // Bytes at block `lba`, `count` blocks, as far as the host takes.
        let span = |lba: u64, count: u64| -> Result<(u64, u64), (u8, u8)> {
            if lba.checked_add(count).is_none_or(|end| end > blocks) {
                return Err((ILLEGAL, ASC_LBA_RANGE));
            }
            Ok((lba * bs, (count * bs).min(len as u64)))
        };
        let be = |b: &[u8]| b.iter().fold(0u64, |a, &x| a << 8 | x as u64);
        let rw = |op_read: bool, lba: u64, count: u64| -> Result<Phase, (u8, u8)> {
            let (off, n) = span(lba, count)?;
            if op_read {
                if !dev_to_host && len != 0 {
                    return Err((ILLEGAL, ASC_BAD_FIELD));
                }
                Ok(if n == 0 { Phase::Status } else { Phase::Read { off, left: n } })
            } else {
                if dev_to_host && len != 0 {
                    return Err((ILLEGAL, ASC_BAD_FIELD));
                }
                if wp {
                    return Err((DATA_PROTECT, ASC_WRITE_PROTECTED));
                }
                Ok(if n == 0 { Phase::Status } else { Phase::Write { off, left: n, keep: true } })
            }
        };
        match cdb[0] {
            // TEST UNIT READY, START STOP UNIT, PREVENT ALLOW MEDIUM
            // REMOVAL, VERIFY(10).
            0x00 | 0x1b | 0x1e | 0x2f => reply(Vec::new()),
            // SYNCHRONIZE CACHE (10), (16).
            0x35 | 0x91 => match self.store.flush() {
                Ok(()) => reply(Vec::new()),
                Err(e) => {
                    log::warn!("usb disk: flush: {e}");
                    Err((MEDIUM_ERROR, ASC_WRITE_ERROR))
                }
            },
            // REQUEST SENSE (fixed format).
            0x03 => {
                let (key, asc) = std::mem::take(&mut self.sense);
                reply(vec![0x70, 0, key, 0, 0, 0, 0, 10, 0, 0, 0, 0, asc, 0, 0, 0, 0, 0])
            }
            // INQUIRY: standard data only. Removable medium, like a stick.
            0x12 if cdb.get(1).is_some_and(|b| b & 1 != 0) => Err((ILLEGAL, ASC_BAD_FIELD)),
            0x12 => {
                let mut v = vec![0x00, 0x80, 0x04, 0x02, 31, 0, 0, 0];
                v.extend_from_slice(&Ids::field(&self.ids.manufacturer, 8));
                v.extend_from_slice(&Ids::field(&self.ids.name, 16));
                v.extend_from_slice(b"1.00");
                reply(v)
            }
            // MODE SENSE(6) and (10): the header with the write protect
            // bit, and the caching page: a write cache, so guests send
            // SYNCHRONIZE CACHE when they need the data to stay (on
            // unmount and shutdown), and we pass it to the disk.
            op @ (0x1a | 0x5a) => {
                let page = cdb.get(2).map_or(0, |b| b & 0x3f);
                let mut pages = Vec::new();
                if page == 0x08 || page == 0x3f {
                    pages.extend_from_slice(&[0x08, 0x12, 0x04]);
                    pages.resize(20, 0);
                }
                let wpb = (wp as u8) << 7;
                let mut v = if op == 0x1a { vec![0, 0, wpb, 0] } else { vec![0, 0, 0, wpb, 0, 0, 0, 0] };
                v.extend_from_slice(&pages);
                if op == 0x1a {
                    v[0] = (v.len() - 1) as u8;
                } else {
                    v[1] = (v.len() - 2) as u8;
                }
                reply(v)
            }
            // READ FORMAT CAPACITIES: one formatted capacity.
            0x23 => {
                let mut v = vec![0, 0, 0, 8];
                v.extend_from_slice(&(blocks.min(u32::MAX as u64) as u32).to_be_bytes());
                v.push(0x02);
                v.extend_from_slice(&(bs as u32).to_be_bytes()[1..]);
                reply(v)
            }
            // READ CAPACITY(10): the last block (all ones: ask (16)).
            0x25 => {
                let mut v = (blocks.saturating_sub(1).min(u32::MAX as u64) as u32).to_be_bytes().to_vec();
                v.extend_from_slice(&(bs as u32).to_be_bytes());
                reply(v)
            }
            // READ CAPACITY(16).
            0x9e if cdb.get(1).is_some_and(|b| b & 0x1f == 0x10) => {
                let mut v = blocks.saturating_sub(1).to_be_bytes().to_vec();
                v.extend_from_slice(&(bs as u32).to_be_bytes());
                v.resize(32, 0);
                reply(v)
            }
            // READ(10), WRITE(10).
            op @ (0x28 | 0x2a) if cdb.len() >= 10 => rw(op == 0x28, be(&cdb[2..6]), be(&cdb[7..9])),
            // READ(16), WRITE(16).
            op @ (0x88 | 0x8a) if cdb.len() >= 16 => rw(op == 0x88, be(&cdb[2..10]), be(&cdb[10..14])),
            _ => Err((ILLEGAL, ASC_BAD_OPCODE)),
        }
    }

    fn csw(&mut self) -> Vec<u8> {
        let mut v = CSW_SIG.to_le_bytes().to_vec();
        v.extend_from_slice(&self.tag.to_le_bytes());
        v.extend_from_slice(&self.asked.saturating_sub(self.moved).to_le_bytes());
        v.push(self.failed as u8);
        self.phase = Phase::Command;
        v
    }

    /// `n` disk bytes from `off`, read as the whole blocks around them.
    fn read_bytes(&mut self, off: u64, n: usize) -> io::Result<Vec<u8>> {
        let bs = self.bs;
        let start = off / bs * bs;
        let end = (off + n as u64).div_ceil(bs) * bs;
        let mut buf = vec![0u8; (end - start) as usize];
        self.store.read_at(start, &mut buf)?;
        let skip = (off - start) as usize;
        buf.drain(..skip);
        buf.truncate(n);
        Ok(buf)
    }

    /// Answer bulk IN transfers waiting, as far as there is something
    /// to send.
    fn serve(&mut self) {
        while let Some(&(id, want)) = self.reads.front() {
            if self.halted_in {
                self.reads.pop_front();
                self.send(id, Status::Stall, Vec::new(), 0);
                continue;
            }
            let want = want as usize;
            let data = match &mut self.phase {
                Phase::Reply { data, at } => {
                    // Never more than the host asked for in the CBW.
                    let room = (self.asked as usize).saturating_sub(*at);
                    let n = want.min(data.len() - *at).min(room);
                    let v = data[*at..*at + n].to_vec();
                    *at += n;
                    // A short transfer ends the data stage.
                    if *at == data.len() || *at == self.asked as usize || n < want {
                        self.phase = Phase::Status;
                    }
                    v
                }
                &mut Phase::Read { off, left } => {
                    let n = (want as u64).min(left) as usize;
                    match self.read_bytes(off, n) {
                        Ok(v) => {
                            let left = left - n as u64;
                            self.phase = if left == 0 || n < want { Phase::Status } else { Phase::Read { off: off + n as u64, left } };
                            v
                        }
                        Err(e) => {
                            log::warn!("usb disk: read at {off}: {e}");
                            self.sense = (MEDIUM_ERROR, ASC_READ_ERROR);
                            self.failed = true;
                            self.halted_in = true;
                            self.phase = Phase::Status;
                            continue;
                        }
                    }
                }
                Phase::Status if want >= 13 => self.csw(),
                // The host wants something else: a real device would
                // NAK; this one waits for the command.
                _ => return,
            };
            self.reads.pop_front();
            if !matches!(self.phase, Phase::Command) {
                self.moved += data.len() as u32;
            }
            let n = data.len() as u32;
            self.send(id, Status::Ok, data, n);
        }
    }

    /// Write out the whole blocks gathered; `all`: the rest too (the
    /// data stage is over), merged into what the disk has.
    fn put(&mut self, all: bool) -> io::Result<()> {
        let bs = self.bs as usize;
        let whole = self.wbuf.len() / bs * bs;
        if whole > 0 {
            let at = self.wbuf_at;
            let r = self.store.write_at(at, &self.wbuf[..whole]);
            self.wbuf.drain(..whole);
            self.wbuf_at += whole as u64;
            r?;
        }
        if all && !self.wbuf.is_empty() {
            let mut block = vec![0u8; bs];
            let at = self.wbuf_at;
            let tail = std::mem::take(&mut self.wbuf);
            self.store.read_at(at, &mut block)?;
            block[..tail.len()].copy_from_slice(&tail);
            self.store.write_at(at, &block)?;
        }
        Ok(())
    }

    /// The data stage of a write ended early (a reset): keep what came.
    fn end_write(&mut self) {
        if let Phase::Write { keep: true, .. } = self.phase {
            if let Err(e) = self.put(true) {
                log::warn!("usb disk: write: {e}");
            }
        }
        self.wbuf.clear();
    }

    fn bulk_out(&mut self, s: &Submit) -> Result<(), Status> {
        match &mut self.phase {
            Phase::Command => self.command(&s.data)?,
            &mut Phase::Write { off, left, keep } => {
                let n = (s.data.len() as u64).min(left);
                let left = left - n;
                let done = left == 0 || n < s.data.len() as u64 || !s.data.len().is_multiple_of(PACKET);
                let mut keep = keep;
                if keep {
                    self.wbuf.extend_from_slice(&s.data[..n as usize]);
                    if let Err(e) = self.put(done) {
                        log::warn!("usb disk: write at {off}: {e}");
                        self.sense = (MEDIUM_ERROR, ASC_WRITE_ERROR);
                        self.failed = true;
                        self.wbuf.clear();
                        keep = false;
                    }
                }
                self.moved += n as u32;
                self.phase = if done { Phase::Status } else { Phase::Write { off: off + n, left, keep } };
            }
            _ => return Err(Status::Stall),
        }
        self.send(s.id, Status::Ok, Vec::new(), s.len);
        self.serve();
        Ok(())
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        self.end_write();
        if let Err(e) = self.store.flush() {
            log::warn!("usb disk: flush: {e}");
        }
    }
}

impl Device for Disk {
    fn submit(&mut self, s: Submit) -> Result<(), Status> {
        match (s.kind, s.ep) {
            (XferType::Control, _) => {
                let mut r = self.control(&s)?;
                if s.is_in() {
                    r.truncate(s.len as usize);
                    let n = r.len() as u32;
                    self.send(s.id, Status::Ok, r, n);
                } else {
                    self.send(s.id, Status::Ok, Vec::new(), s.len);
                }
            }
            (XferType::Bulk, _) if self.broken => return Err(Status::Error),
            (XferType::Bulk, EP_OUT) => self.bulk_out(&s)?,
            (XferType::Bulk, EP_IN) => {
                self.reads.push_back((s.id, s.len));
                self.serve();
            }
            _ => return Err(Status::Stall),
        }
        Ok(())
    }

    fn submit_iso(&mut self, _s: IsoSubmit) -> Result<(), Status> {
        Err(Status::Stall)
    }

    fn cancel(&mut self, id: u32) {
        let before = self.reads.len();
        self.reads.retain(|&(i, _)| i != id);
        if self.reads.len() != before {
            self.send(id, Status::Cancelled, Vec::new(), 0);
        }
    }

    fn in_flight(&self) -> usize {
        self.reads.len()
    }

    fn busy(&self, id: u32) -> bool {
        self.reads.iter().any(|&(i, _)| i == id)
    }

    fn reset(&mut self) -> Result<(), Status> {
        for (id, _) in std::mem::take(&mut self.reads) {
            self.send(id, Status::Cancelled, Vec::new(), 0);
        }
        self.end_write();
        self.config = 0;
        self.phase = Phase::Command;
        self.halted_in = false;
        Ok(())
    }

    fn config_descriptor(&mut self, value: u8) -> Option<Vec<u8>> {
        (value == 1).then(Self::config_descriptor)
    }

    fn set_configuration(&mut self, value: u8, _cfg: Option<&Config>) -> Result<(), Status> {
        self.config = value;
        Ok(())
    }

    fn set_interface(&mut self, _iface: u8, alt: u8) -> Result<(), Status> {
        if alt == 0 {
            Ok(())
        } else {
            Err(Status::Stall)
        }
    }

    fn clear_halt(&mut self, ep: u8) -> Result<(), Status> {
        if ep == EP_IN {
            self.halted_in = false;
            self.serve();
        }
        Ok(())
    }

    fn gone(&self) -> bool {
        self.store.gone()
    }
}

/// Serves one store as a stick, once: when it is gone, it stays gone.
pub struct StoreOpener {
    store: Option<(Ids, Box<dyn Store>)>,
}

impl std::fmt::Debug for StoreOpener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreOpener").field("ids", &self.store.as_ref().map(|s| &s.0)).finish()
    }
}

impl StoreOpener {
    pub fn new(ids: Ids, store: Box<dyn Store>) -> StoreOpener {
        StoreOpener { store: Some((ids, store)) }
    }
}

impl Opener for StoreOpener {
    fn open(&mut self, sink: &Sink) -> Option<(Box<dyn Device>, Speed, u16, u16)> {
        let (ids, store) = self.store.take()?;
        let (v, p) = (ids.vendor, ids.product);
        Some((Box::new(Disk::with_store(sink.clone(), ids, store)), Speed::High, v, p))
    }

    fn present(&mut self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::Session;
    use crate::proto::Request;
    use std::sync::mpsc;

    fn cbw(tag: u32, len: u32, dir_in: bool, cdb: &[u8]) -> Vec<u8> {
        let mut v = CBW_SIG.to_le_bytes().to_vec();
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(&[if dir_in { 0x80 } else { 0 }, 0, cdb.len() as u8]);
        v.extend_from_slice(cdb);
        v.resize(31, 0);
        v
    }

    struct T {
        s: Session,
        dev: Disk,
        rx: mpsc::Receiver<Event>,
        id: u32,
    }

    impl T {
        fn new() -> T {
            let (tx, _) = mpsc::channel();
            T::with(Disk::new(tx, 0x1209, 0x0003, false))
        }

        fn with(dev: Disk) -> T {
            let (tx, rx) = mpsc::channel();
            let mut dev = dev;
            dev.sink = tx.clone();
            let mut t = T { s: Session::new(tx), dev, rx, id: 0 };
            let d = t.req(Submit::control(0, [0, 9, 1, 0, 0, 0, 0, 0], vec![]));
            assert_eq!(d.status, Status::Ok);
            t
        }

        fn req(&mut self, mut s: Submit) -> Done {
            self.id += 1;
            s.id = self.id;
            self.s.request(Some(&mut self.dev), Request::Submit(s));
            match self.rx.try_recv() {
                Ok(Event::Done(d)) => d,
                other => panic!("no completion: {other:?}"),
            }
        }

        fn bulk(&mut self, ep: u8, len: u32, data: Vec<u8>) -> Done {
            self.req(Submit { id: 0, ep, kind: XferType::Bulk, setup: [0; 8], len, data, zero_packet: false })
        }

        /// A whole command: (data back, CSW residue, CSW status).
        fn cmd(&mut self, len: u32, dir_in: bool, cdb: &[u8], out: Vec<u8>) -> (Vec<u8>, u32, u8) {
            self.cmd_in(len, dir_in, cdb, vec![out], len)
        }

        /// The same, the data in transfers of `chunk` (IN) or as `outs` (OUT).
        fn cmd_in(&mut self, len: u32, dir_in: bool, cdb: &[u8], outs: Vec<Vec<u8>>, chunk: u32) -> (Vec<u8>, u32, u8) {
            assert_eq!(self.bulk(EP_OUT, 31, cbw(7, len, dir_in, cdb)).status, Status::Ok);
            let mut data = Vec::new();
            if len > 0 && dir_in {
                while (data.len() as u32) < len {
                    let d = self.bulk(EP_IN, chunk.min(len - data.len() as u32), vec![]);
                    if d.status == Status::Stall {
                        let clear = self.req(Submit::control(0, [0x02, 1, 0, 0, EP_IN, 0, 0, 0], vec![]));
                        assert_eq!(clear.status, Status::Ok);
                        break;
                    }
                    let short = (d.data.len() as u32) < chunk.min(len - data.len() as u32);
                    data.extend_from_slice(&d.data);
                    if short {
                        break;
                    }
                }
            } else if len > 0 {
                for o in outs {
                    let n = o.len() as u32;
                    assert_eq!(self.bulk(EP_OUT, n, o).status, Status::Ok);
                }
            }
            let csw = self.bulk(EP_IN, 13, vec![]);
            assert_eq!((csw.status, csw.data.len()), (Status::Ok, 13));
            assert_eq!(&csw.data[..8], &[0x55, 0x53, 0x42, 0x53, 7, 0, 0, 0]);
            (data, u32::from_le_bytes([csw.data[8], csw.data[9], csw.data[10], csw.data[11]]), csw.data[12])
        }
    }

    #[test]
    fn bulk_only_transport() {
        let mut t = T::new();
        // INQUIRY asked for 96: 36 come (a short transfer), residue 60.
        let (d, residue, st) = t.cmd(96, true, &[0x12, 0, 0, 0, 96, 0], vec![]);
        assert_eq!((d.len(), residue, st), (36, 60, 0));
        assert_eq!(&d[8..16], b"vroom   ");
        let (d, _, st) = t.cmd(8, true, &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], vec![]);
        assert_eq!((st, &d[..]), (0, &((DISK_BYTES / 512 - 1) as u32).to_be_bytes().iter().chain(&[0, 0, 2, 0]).copied().collect::<Vec<u8>>()[..]));
        // Write two blocks at LBA 5, read them back.
        let data: Vec<u8> = (0..1024u32).map(|i| (i * 7) as u8).collect();
        let (_, residue, st) = t.cmd(1024, false, &[0x2a, 0, 0, 0, 0, 5, 0, 0, 2, 0], data.clone());
        assert_eq!((residue, st), (0, 0));
        let (d, residue, st) = t.cmd(1024, true, &[0x28, 0, 0, 0, 0, 5, 0, 0, 2, 0], vec![]);
        assert_eq!((d, residue, st), (data, 0, 0));
        // An unknown command with data: the IN pipe stalls, the status
        // says failed, REQUEST SENSE says why.
        let (_, residue, st) = t.cmd(64, true, &[0xd7, 0, 0, 0, 0, 0], vec![]);
        assert_eq!((residue, st), (64, 1));
        let (d, _, st) = t.cmd(18, true, &[0x03, 0, 0, 0, 18, 0], vec![]);
        assert_eq!((st, d[2], d[12]), (0, 0x05, 0x20));
        // Past the end, and a block count that would wrap.
        let (_, _, st) = t.cmd(512, true, &[0x28, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 1, 0], vec![]);
        assert_eq!(st, 1);
        let mut c16 = [0u8; 16];
        c16[0] = 0x88;
        c16[2..10].copy_from_slice(&u64::MAX.to_be_bytes());
        c16[13] = 1;
        let (_, _, st) = t.cmd(512, true, &c16, vec![]);
        assert_eq!(st, 1);
        let (d, _, _) = t.cmd(18, true, &[0x03, 0, 0, 0, 18, 0], vec![]);
        assert_eq!((d[2], d[12]), (ILLEGAL, ASC_LBA_RANGE));
        // GET MAX LUN.
        let d = t.req(Submit::control(0, [0xa1, 0xfe, 0, 0, 0, 0, 1, 0], vec![]));
        assert_eq!((d.status, d.data), (Status::Ok, vec![0]));
    }

    #[test]
    fn broken_fails_bulk_only() {
        let (tx, rx) = mpsc::channel();
        let mut dev = Disk::new(tx.clone(), 0x1209, 0x0003, true);
        let mut s = Session::new(tx);
        s.request(Some(&mut dev), Request::Submit(Submit::control(1, [0, 9, 1, 0, 0, 0, 0, 0], vec![])));
        assert!(matches!(rx.try_recv(), Ok(Event::Done(Done { status: Status::Ok, .. }))));
        let w = Submit { id: 2, ep: EP_OUT, kind: XferType::Bulk, setup: [0; 8], len: 31, data: cbw(1, 0, false, &[0; 6]), zero_packet: false };
        s.request(Some(&mut dev), Request::Submit(w));
        assert!(matches!(rx.try_recv(), Ok(Event::Done(Done { status: Status::Error, .. }))));
    }

    /// A store with 4 KiB blocks that counts flushes and can fail.
    struct Big {
        ram: Vec<u8>,
        ro: bool,
        flushes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_reads: bool,
    }

    impl Store for Big {
        fn block_size(&self) -> u32 {
            4096
        }
        fn blocks(&self) -> u64 {
            (self.ram.len() / 4096) as u64
        }
        fn readonly(&self) -> bool {
            self.ro
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
            assert!(off.is_multiple_of(4096) && buf.len().is_multiple_of(4096), "whole blocks only: {off} {}", buf.len());
            if self.fail_reads {
                return Err(io::Error::other("bad sector"));
            }
            buf.copy_from_slice(&self.ram[off as usize..off as usize + buf.len()]);
            Ok(())
        }
        fn write_at(&mut self, off: u64, data: &[u8]) -> io::Result<()> {
            assert!(off.is_multiple_of(4096) && data.len().is_multiple_of(4096), "whole blocks only: {off} {}", data.len());
            self.ram[off as usize..off as usize + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    fn big(ro: bool) -> (T, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let flushes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store = Big { ram: (0..64 * 4096).map(|i| (i / 4096) as u8).collect(), ro, flushes: flushes.clone(), fail_reads: false };
        let (tx, _) = mpsc::channel();
        let ids = Ids { vendor: 0x1209, product: 0x00ab, manufacturer: "Verbatim".into(), name: "STORE N GO (vroom disk)".into(), serial: "S1".into() };
        (T::with(Disk::with_store(tx, ids, Box::new(store))), flushes)
    }

    #[test]
    fn a_store_with_big_blocks() {
        let (mut t, flushes) = big(false);
        // Capacity: 64 blocks of 4096.
        let (d, _, st) = t.cmd(8, true, &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], vec![]);
        assert_eq!((st, &d[..]), (0, &[0, 0, 0, 63, 0, 0, 0x10, 0][..]));
        let (d, _, _) = t.cmd(36, true, &[0x12, 0, 0, 0, 36, 0], vec![]);
        assert_eq!(&d[8..32], b"Verbatim" .iter().chain(b"STORE N GO (vroo").copied().collect::<Vec<u8>>().as_slice());
        // Read block 3 in 512-byte transfers: each comes from whole blocks.
        let (d, residue, st) = t.cmd_in(4096, true, &[0x28, 0, 0, 0, 0, 3, 0, 0, 1, 0], vec![], 512);
        assert_eq!((d.len(), residue, st), (4096, 0, 0));
        assert!(d.iter().all(|&b| b == 3));
        // Write two blocks at 10 in odd-sized pieces (packet multiples).
        let data: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        let outs = vec![data[..1536].to_vec(), data[1536..5120].to_vec(), data[5120..].to_vec()];
        let (_, residue, st) = t.cmd_in(8192, false, &[0x2a, 0, 0, 0, 0, 10, 0, 0, 2, 0], outs, 0);
        assert_eq!((residue, st), (0, 0));
        let (d, _, _) = t.cmd(8192, true, &[0x28, 0, 0, 0, 0, 10, 0, 0, 2, 0], vec![]);
        assert_eq!(d, data);
        // The host asked for less than the blocks: the rest of the block
        // stays as it was.
        let (_, _, st) = t.cmd(1024, false, &[0x2a, 0, 0, 0, 0, 20, 0, 0, 1, 0], vec![9; 1024]);
        assert_eq!(st, 0);
        let (d, _, _) = t.cmd(4096, true, &[0x28, 0, 0, 0, 0, 20, 0, 0, 1, 0], vec![]);
        assert!(d[..1024].iter().all(|&b| b == 9) && d[1024..].iter().all(|&b| b == 20));
        // SYNCHRONIZE CACHE reaches the store; so does letting go.
        let (_, _, st) = t.cmd(0, false, &[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], vec![]);
        assert_eq!((st, flushes.load(std::sync::atomic::Ordering::Relaxed)), (0, 1));
        drop(t);
        assert_eq!(flushes.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn read_only_refuses_writes() {
        let (mut t, _) = big(true);
        let (d, _, _) = t.cmd(4, true, &[0x1a, 0, 0x3f, 0, 4, 0], vec![]);
        assert_eq!(d[2] & 0x80, 0x80, "write protected");
        // All pages: the caching page says there is a write cache.
        let (d, _, _) = t.cmd(192, true, &[0x1a, 0, 0x3f, 0, 192, 0], vec![]);
        assert_eq!((d.len(), d[0], d[4], d[6] & 4), (24, 23, 0x08, 4));
        let (d, _, _) = t.cmd(192, true, &[0x5a, 0, 0x08, 0, 0, 0, 0, 0, 192, 0], vec![]);
        assert_eq!((d.len(), d[1], d[8]), (28, 26, 0x08));
        let (d, _, _) = t.cmd(192, true, &[0x1a, 0, 0x1c, 0, 192, 0], vec![]);
        assert_eq!(d.len(), 4, "another page: none");
        let (_, residue, st) = t.cmd(4096, false, &[0x2a, 0, 0, 0, 0, 1, 0, 0, 1, 0], vec![0xee; 4096]);
        assert_eq!((residue, st), (0, 1), "data taken and dropped, failed");
        let (d, _, _) = t.cmd(18, true, &[0x03, 0, 0, 0, 18, 0], vec![]);
        assert_eq!((d[2], d[12]), (DATA_PROTECT, ASC_WRITE_PROTECTED));
        let (d, _, _) = t.cmd(4096, true, &[0x28, 0, 0, 0, 0, 1, 0, 0, 1, 0], vec![]);
        assert!(d.iter().all(|&b| b == 1), "unchanged");
    }

    #[test]
    fn read_errors_fail_the_command() {
        let flushes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store = Big { ram: vec![0; 8 * 4096], ro: false, flushes, fail_reads: true };
        let (tx, _) = mpsc::channel();
        let mut t = T::with(Disk::with_store(tx, Ids::test(1, 2), Box::new(store)));
        let (d, residue, st) = t.cmd(4096, true, &[0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0], vec![]);
        assert_eq!((d.len(), residue, st), (0, 4096, 1));
        let (d, _, _) = t.cmd(18, true, &[0x03, 0, 0, 0, 18, 0], vec![]);
        assert_eq!((d[2], d[12]), (MEDIUM_ERROR, ASC_READ_ERROR));
    }
}
