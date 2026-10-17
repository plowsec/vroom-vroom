//! A pretend USB stick for tests without hardware (`VROOM_USB_FAKE=disk`):
//! high speed, mass storage over Bulk-Only Transport (a CBW on bulk OUT,
//! the data, a CSW on bulk IN), SCSI inside, 512-byte packets like a real
//! USB 2 stick. A RAM disk the guest can partition, format and write.
//! Guests drive it with their stock storage driver (Linux `usb-storage`,
//! UEFI's `UsbMassStorageDxe`).
//!
//! It goes through the same paths a real stick does: short IN transfers
//! (a reply shorter than asked for, then the CSW with a residue), STALLs
//! on failed commands and the guest's CLEAR_FEATURE(ENDPOINT_HALT), the
//! class requests. `VROOM_USB_FAKE=disk-broken` fails every bulk transfer
//! instead, like a stick the host won't let go of: guests must get past
//! it.

use std::collections::VecDeque;

use crate::desc::Config;
use crate::helper::{Device, Sink};
use crate::proto::{Done, Event, IsoSubmit, Status, Submit, XferType};

const EP_IN: u8 = 0x81;
const EP_OUT: u8 = 0x02;
const BLOCK: usize = 512;
/// 128 MiB, zero until written (the allocator hands out untouched pages).
pub const DISK_BYTES: usize = 128 << 20;
const CBW_SIG: u32 = 0x4342_5355;
const CSW_SIG: u32 = 0x5342_5355;

/// What the next bulk transfer is for.
#[derive(Debug)]
enum Phase {
    /// Waiting for a command (bulk OUT).
    Command,
    /// Sending a reply: `left` bytes the host still expects at most.
    Reply { data: Vec<u8>, at: usize },
    /// Sending disk blocks from `off`, `left` bytes.
    Read { off: usize, left: usize },
    /// Taking disk blocks to `off`, `left` bytes; `keep`: false when the
    /// command failed (the data is dropped).
    Write { off: usize, left: usize, keep: bool },
    /// The status (CSW) is next on bulk IN.
    Status,
}

#[derive(Debug)]
pub struct Disk {
    sink: Sink,
    vendor: u16,
    product: u16,
    config: u8,
    disk: Vec<u8>,
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
}

impl Disk {
    pub fn new(sink: Sink, vendor: u16, product: u16, broken: bool) -> Disk {
        Disk {
            sink,
            vendor,
            product,
            config: 0,
            disk: vec![0; DISK_BYTES],
            phase: Phase::Command,
            tag: 0,
            asked: 0,
            moved: 0,
            failed: false,
            sense: (0, 0),
            reads: VecDeque::new(),
            halted_in: false,
            broken,
        }
    }

    fn device_descriptor(&self) -> Vec<u8> {
        let mut d = vec![18, 1, 0x00, 0x02, 0, 0, 0, 64];
        d.extend_from_slice(&self.vendor.to_le_bytes());
        d.extend_from_slice(&self.product.to_le_bytes());
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

    fn control(&mut self, s: &Submit) -> Result<Vec<u8>, Status> {
        let (rt, req) = (s.setup[0], s.setup[1]);
        let value = u16::from_le_bytes([s.setup[2], s.setup[3]]);
        Ok(match (rt, req) {
            (0x80, 6) => match (value >> 8) as u8 {
                1 => self.device_descriptor(),
                2 if value & 0xff == 0 => Self::config_descriptor(),
                3 => match value as u8 {
                    0 => vec![4, 3, 0x09, 0x04],
                    1 => super::fake::string_desc("vroom"),
                    2 => super::fake::string_desc("Test USB disk"),
                    3 => super::fake::string_desc("0001VROOMDISK"),
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
            // reset; stalling this transfer is enough for a test device.
            return Err(Status::Stall);
        }
        self.tag = u32::from_le_bytes([cbw[4], cbw[5], cbw[6], cbw[7]]);
        let len = u32::from_le_bytes([cbw[8], cbw[9], cbw[10], cbw[11]]);
        let dev_to_host = cbw[12] & 0x80 != 0;
        let n = (cbw[14] & 0x1f).clamp(1, 16) as usize;
        let cdb = &cbw[15..15 + n];
        (self.asked, self.moved, self.failed) = (len, 0, false);
        let r = self.scsi(cdb, len, dev_to_host);
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
                    (l, false) => Phase::Write { off: 0, left: l as usize, keep: false },
                }
            }
        };
        if !self.failed {
            self.sense = (0, 0);
        }
        Ok(())
    }

    /// Run one SCSI command: the phase that follows, or the sense data of
    /// a failure (key, additional sense code).
    fn scsi(&mut self, cdb: &[u8], len: u32, dev_to_host: bool) -> Result<Phase, (u8, u8)> {
        const ILLEGAL: u8 = 0x05;
        let blocks = (self.disk.len() / BLOCK) as u64;
        let reply = |v: Vec<u8>| {
            if len == 0 {
                Ok(Phase::Status)
            } else if !dev_to_host {
                Err((ILLEGAL, 0x24))
            } else {
                Ok(Phase::Reply { data: v, at: 0 })
            }
        };
        let rw = |lba: u64, count: u64| -> Result<(usize, usize), (u8, u8)> {
            if lba + count > blocks {
                return Err((ILLEGAL, 0x21));
            }
            Ok(((lba as usize) * BLOCK, (count as usize * BLOCK).min(len as usize)))
        };
        match cdb[0] {
            // TEST UNIT READY, START STOP UNIT, PREVENT ALLOW MEDIUM
            // REMOVAL, SYNCHRONIZE CACHE(10), VERIFY(10).
            0x00 | 0x1b | 0x1e | 0x35 | 0x2f => reply(Vec::new()),
            // REQUEST SENSE (fixed format).
            0x03 => {
                let (key, asc) = std::mem::take(&mut self.sense);
                reply(vec![0x70, 0, key, 0, 0, 0, 0, 10, 0, 0, 0, 0, asc, 0, 0, 0, 0, 0])
            }
            // INQUIRY: standard data only.
            0x12 if cdb.get(1).is_some_and(|b| b & 1 != 0) => Err((ILLEGAL, 0x24)),
            0x12 => {
                let mut v = vec![0x00, 0x80, 0x04, 0x02, 31, 0, 0, 0];
                v.extend_from_slice(b"vroom   Test USB disk   1.00");
                reply(v)
            }
            // MODE SENSE(6) and (10): just the header, not write protected.
            0x1a => reply(vec![3, 0, 0, 0]),
            0x5a => reply(vec![0, 6, 0, 0, 0, 0, 0, 0]),
            // READ FORMAT CAPACITIES: one formatted capacity.
            0x23 => {
                let mut v = vec![0, 0, 0, 8];
                v.extend_from_slice(&(blocks as u32).to_be_bytes());
                v.extend_from_slice(&[0x02, 0, 0x02, 0]);
                reply(v)
            }
            // READ CAPACITY(10), (16).
            0x25 => {
                let mut v = ((blocks - 1) as u32).to_be_bytes().to_vec();
                v.extend_from_slice(&(BLOCK as u32).to_be_bytes());
                reply(v)
            }
            0x9e if cdb.get(1).is_some_and(|b| b & 0x1f == 0x10) => {
                let mut v = (blocks - 1).to_be_bytes().to_vec();
                v.extend_from_slice(&(BLOCK as u32).to_be_bytes());
                v.resize(32, 0);
                reply(v)
            }
            // READ(10), WRITE(10).
            op @ (0x28 | 0x2a) if cdb.len() >= 10 => {
                let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
                let count = u16::from_be_bytes([cdb[7], cdb[8]]) as u64;
                let (off, n) = rw(lba, count)?;
                match (op, dev_to_host || len == 0) {
                    (0x28, true) => Ok(if n == 0 { Phase::Status } else { Phase::Read { off, left: n } }),
                    (0x2a, false) => Ok(if n == 0 { Phase::Status } else { Phase::Write { off, left: n, keep: true } }),
                    _ => Err((ILLEGAL, 0x24)),
                }
            }
            _ => Err((ILLEGAL, 0x20)),
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
                Phase::Read { off, left } => {
                    let n = want.min(*left);
                    let v = self.disk[*off..*off + n].to_vec();
                    (*off, *left) = (*off + n, *left - n);
                    if *left == 0 || n < want {
                        self.phase = Phase::Status;
                    }
                    v
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

    fn bulk_out(&mut self, s: &Submit) -> Result<(), Status> {
        match &mut self.phase {
            Phase::Command => self.command(&s.data)?,
            Phase::Write { off, left, keep } => {
                let n = s.data.len().min(*left);
                if *keep {
                    self.disk[*off..*off + n].copy_from_slice(&s.data[..n]);
                }
                (*off, *left) = (*off + n, *left - n);
                self.moved += n as u32;
                if *left == 0 || n < s.data.len() || !s.data.len().is_multiple_of(BLOCK) {
                    self.phase = Phase::Status;
                }
            }
            _ => return Err(Status::Stall),
        }
        self.send(s.id, Status::Ok, Vec::new(), s.len);
        self.serve();
        Ok(())
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
        false
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
            let (tx, rx) = mpsc::channel();
            let mut t = T { s: Session::new(tx.clone()), dev: Disk::new(tx, 0x1209, 0x0003, false), rx, id: 0 };
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
            assert_eq!(self.bulk(EP_OUT, 31, cbw(7, len, dir_in, cdb)).status, Status::Ok);
            let mut data = Vec::new();
            if len > 0 && dir_in {
                let d = self.bulk(EP_IN, len, vec![]);
                if d.status == Status::Stall {
                    let clear = self.req(Submit::control(0, [0x02, 1, 0, 0, EP_IN, 0, 0, 0], vec![]));
                    assert_eq!(clear.status, Status::Ok);
                } else {
                    data = d.data;
                }
            } else if len > 0 {
                assert_eq!(self.bulk(EP_OUT, len, out).status, Status::Ok);
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
        assert_eq!(&d[8..13], b"vroom");
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
        // Past the end.
        let (_, _, st) = t.cmd(512, true, &[0x28, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 1, 0], vec![]);
        assert_eq!(st, 1);
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
}
