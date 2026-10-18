//! Pretend devices behind the helper, for tests without hardware:
//! - `VROOM_USB_FAKE=acm`: a CDC-ACM serial port that echoes what it is
//!   sent (control, bulk in and out, interrupt in). Guests drive it with
//!   their stock ACM driver (`/dev/ttyACM0` on Linux).
//! - `VROOM_USB_FAKE=audio`: a USB sound card (isochronous in and out).
//!   Guests drive it with their stock USB audio driver.
//! - `VROOM_USB_FAKE=disk`: a USB stick ([`crate::msd`]).

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::desc::Config;
use crate::helper::{Device, Opener, Sink};
use crate::proto::{Done, Event, IsoDone, IsoPacket, IsoSubmit, Speed, Status, Submit, XferType};

const LOOP_CAP: usize = 1 << 20;
const EP_NOTIFY: u8 = 0x81;
const EP_OUT: u8 = 0x02;
const EP_IN: u8 = 0x83;

#[derive(Debug)]
pub struct Acm {
    sink: Sink,
    vendor: u16,
    product: u16,
    config: u8,
    line: [u8; 7],
    looped: VecDeque<u8>,
    /// Bulk IN transfers waiting for data: (id, wanted).
    reads: VecDeque<(u32, u32)>,
    /// Interrupt IN transfers: the device has nothing to say.
    notify: Vec<u32>,
}

impl Acm {
    pub fn new(sink: Sink, vendor: u16, product: u16) -> Acm {
        Acm {
            sink,
            vendor,
            product,
            config: 0,
            line: [0x80, 0x25, 0, 0, 0, 0, 8], // 9600 8N1
            looped: VecDeque::new(),
            reads: VecDeque::new(),
            notify: Vec::new(),
        }
    }

    fn device_descriptor(&self) -> Vec<u8> {
        let mut d = vec![18, 1, 0x00, 0x02, 0x02, 0, 0, 64];
        d.extend_from_slice(&self.vendor.to_le_bytes());
        d.extend_from_slice(&self.product.to_le_bytes());
        d.extend_from_slice(&[0x00, 0x01, 1, 2, 3, 1]);
        d
    }

    /// Two interfaces (CDC control with an interrupt endpoint, CDC data
    /// with bulk in/out), full speed.
    pub fn config_descriptor(max_packet: u16) -> Vec<u8> {
        let [lo, hi] = max_packet.to_le_bytes();
        let mut c = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 1, 0x02, 0x02, 0x01, 0]);
        c.extend_from_slice(&[5, 0x24, 0x00, 0x10, 0x01]); // header
        c.extend_from_slice(&[5, 0x24, 0x01, 0x00, 0x01]); // call management
        c.extend_from_slice(&[4, 0x24, 0x02, 0x02]); // ACM
        c.extend_from_slice(&[5, 0x24, 0x06, 0x00, 0x01]); // union
        c.extend_from_slice(&[7, 5, EP_NOTIFY, 3, 8, 0, 16]);
        c.extend_from_slice(&[9, 4, 1, 0, 2, 0x0a, 0, 0, 0]);
        c.extend_from_slice(&[7, 5, EP_OUT, 2, lo, hi, 0]);
        c.extend_from_slice(&[7, 5, EP_IN, 2, lo, hi, 0]);
        let n = c.len() as u16;
        c[2..4].copy_from_slice(&n.to_le_bytes());
        c
    }

    fn string(&self, idx: u8) -> Option<Vec<u8>> {
        let s = match idx {
            0 => return Some(vec![4, 3, 0x09, 0x04]),
            1 => "vroom",
            2 => "Test serial loopback",
            3 => "0001",
            _ => return None,
        };
        Some(string_desc(s))
    }

    fn send(&self, id: u32, status: Status, actual: u32, data: Vec<u8>) {
        let _ = self.sink.send(Event::Done(Done { id, status, actual, data }));
    }

    fn control(&mut self, s: &Submit) -> Result<Vec<u8>, Status> {
        let (rt, req) = (s.setup[0], s.setup[1]);
        let value = u16::from_le_bytes([s.setup[2], s.setup[3]]);
        let r = match (rt, req) {
            (0x80, 6) => match (value >> 8) as u8 {
                1 => self.device_descriptor(),
                2 if value & 0xff == 0 => Self::config_descriptor(64),
                3 => self.string(value as u8).ok_or(Status::Stall)?,
                _ => return Err(Status::Stall),
            },
            (0x80..=0x82, 0) => vec![0, 0],
            (0x80, 8) => vec![self.config],
            (0x81, 10) => vec![0],
            // CDC: SET_LINE_CODING, GET_LINE_CODING, SET_CONTROL_LINE_STATE, SEND_BREAK.
            (0x21, 0x20) if s.data.len() == 7 => {
                self.line.copy_from_slice(&s.data);
                vec![]
            }
            (0xa1, 0x21) => self.line.to_vec(),
            (0x21, 0x22) | (0x21, 0x23) => vec![],
            _ => return Err(Status::Stall),
        };
        Ok(r)
    }

    fn serve_reads(&mut self) {
        while !self.looped.is_empty() {
            let Some((id, want)) = self.reads.pop_front() else { break };
            let n = (want as usize).min(self.looped.len());
            let data: Vec<u8> = self.looped.drain(..n).collect();
            self.send(id, Status::Ok, n as u32, data);
        }
    }
}

impl Device for Acm {
    fn submit(&mut self, s: Submit) -> Result<(), Status> {
        match (s.kind, s.ep) {
            (XferType::Control, _) => {
                let mut r = self.control(&s)?;
                if s.is_in() {
                    r.truncate(s.len as usize);
                    self.send(s.id, Status::Ok, r.len() as u32, r);
                } else {
                    self.send(s.id, Status::Ok, s.len, Vec::new());
                }
            }
            (XferType::Bulk, EP_OUT) => {
                if self.looped.len() + s.data.len() > LOOP_CAP {
                    return Err(Status::Error);
                }
                self.looped.extend(&s.data);
                self.send(s.id, Status::Ok, s.len, Vec::new());
                self.serve_reads();
            }
            (XferType::Bulk, EP_IN) => {
                self.reads.push_back((s.id, s.len));
                self.serve_reads();
            }
            (XferType::Interrupt, EP_NOTIFY) => self.notify.push(s.id),
            _ => return Err(Status::Stall),
        }
        Ok(())
    }

    fn submit_iso(&mut self, _s: IsoSubmit) -> Result<(), Status> {
        Err(Status::Stall)
    }

    fn cancel(&mut self, id: u32) {
        let before = self.reads.len() + self.notify.len();
        self.reads.retain(|&(i, _)| i != id);
        self.notify.retain(|&i| i != id);
        if self.reads.len() + self.notify.len() != before {
            self.send(id, Status::Cancelled, 0, Vec::new());
        }
    }

    fn in_flight(&self) -> usize {
        self.reads.len() + self.notify.len()
    }

    fn busy(&self, id: u32) -> bool {
        self.reads.iter().any(|&(i, _)| i == id) || self.notify.contains(&id)
    }

    fn reset(&mut self) -> Result<(), Status> {
        for (id, _) in std::mem::take(&mut self.reads) {
            self.send(id, Status::Cancelled, 0, Vec::new());
        }
        for id in std::mem::take(&mut self.notify) {
            self.send(id, Status::Cancelled, 0, Vec::new());
        }
        self.config = 0;
        self.looped.clear();
        Ok(())
    }

    fn config_descriptor(&mut self, value: u8) -> Option<Vec<u8>> {
        (value == 1).then(|| Self::config_descriptor(64))
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

    fn clear_halt(&mut self, _ep: u8) -> Result<(), Status> {
        Ok(())
    }

    fn gone(&self) -> bool {
        false
    }
}

/// Which pretend device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeKind {
    Acm,
    Audio,
    /// A USB stick; `broken`: every bulk transfer fails.
    Disk { broken: bool },
}

/// Opens the fake device. `unplug_after`: pull it out once after that
/// long, and put it back three seconds later (hotplug tests).
#[derive(Debug)]
pub struct FakeOpener {
    kind: FakeKind,
    vendor: u16,
    product: u16,
    start: Instant,
    unplug_after: Option<Duration>,
}

impl FakeOpener {
    pub fn new(kind: FakeKind, vendor: u16, product: u16, unplug_after: Option<Duration>) -> FakeOpener {
        FakeOpener { kind, vendor, product, start: Instant::now(), unplug_after }
    }

    fn unplugged(&self) -> bool {
        self.unplug_after.is_some_and(|t| {
            let e = self.start.elapsed();
            e >= t && e < t + Duration::from_secs(3)
        })
    }
}

impl Opener for FakeOpener {
    fn open(&mut self, sink: &Sink) -> Option<(Box<dyn Device>, Speed, u16, u16)> {
        if self.unplugged() {
            return None;
        }
        let dev: Box<dyn Device> = match self.kind {
            FakeKind::Acm => Box::new(Acm::new(sink.clone(), self.vendor, self.product)),
            FakeKind::Audio => Box::new(Audio::new(sink.clone(), self.vendor, self.product, true)),
            FakeKind::Disk { broken } => Box::new(crate::msd::Disk::new(sink.clone(), self.vendor, self.product, broken)),
        };
        let speed = if matches!(self.kind, FakeKind::Disk { .. }) { Speed::High } else { Speed::Full };
        Some((dev, speed, self.vendor, self.product))
    }

    fn present(&mut self) -> bool {
        !self.unplugged()
    }
}

pub(crate) fn string_desc(s: &str) -> Vec<u8> {
    let mut v = vec![0, 3];
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v[0] = v.len() as u8;
    v
}

const AUDIO_OUT: u8 = 0x01;
const AUDIO_IN: u8 = 0x82;
/// Microphone: 48 kHz, 16-bit mono: 48 samples per 1 ms frame.
const MIC_PACKET: usize = 96;

type Due = (Instant, u32, Event);

/// Completes isochronous transfers when a real device would: one packet
/// per frame (1 ms), each endpoint's transfers back to back.
struct Clock {
    tx: mpsc::Sender<Due>,
    /// When each endpoint (out, in) is done with what it has queued.
    busy_until: [Option<Instant>; 2],
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clock").finish()
    }
}

/// A USB sound card (USB Audio Class 1, full speed): speakers taking
/// 48 kHz 16-bit stereo (isochronous OUT, endpoint 1), and a microphone
/// giving 48 kHz 16-bit mono (isochronous IN, endpoint 2). The
/// microphone's samples count up by one each (a ramp, so a test can see
/// that nothing was lost or reordered); what is played is counted.
#[derive(Debug)]
pub struct Audio {
    sink: Sink,
    vendor: u16,
    product: u16,
    config: u8,
    alts: [u8; 3],
    /// Next microphone sample.
    ramp: u16,
    /// Bytes played, and their sum (tests look at these).
    pub played: u64,
    pub played_sum: u64,
    /// Isochronous transfers not completed yet.
    pending: Arc<Mutex<HashSet<u32>>>,
    /// Some: complete in real time. None: right away (unit tests).
    clock: Option<Clock>,
}

fn clock_thread(rx: mpsc::Receiver<Due>, sink: Sink, pending: Arc<Mutex<HashSet<u32>>>) {
    // Each endpoint's due times come in order; the two interleave, so
    // keep them sorted.
    let mut due: VecDeque<Due> = VecDeque::new();
    loop {
        let now = Instant::now();
        while due.front().is_some_and(|d| d.0 <= now) {
            let Some((_, id, ev)) = due.pop_front() else { break };
            if pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                let _ = sink.send(ev);
            }
        }
        let wait = due.front().map_or(Duration::from_secs(1), |d| d.0.saturating_duration_since(now));
        match rx.recv_timeout(wait) {
            Ok(d) => {
                let at = due.partition_point(|x| x.0 <= d.0);
                due.insert(at, d);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

impl Audio {
    pub fn new(sink: Sink, vendor: u16, product: u16, real_time: bool) -> Audio {
        let pending = Arc::new(Mutex::new(HashSet::new()));
        let clock = real_time.then(|| {
            let (tx, rx) = mpsc::channel::<Due>();
            let (sink, pending) = (sink.clone(), pending.clone());
            let _ = std::thread::Builder::new().name("usb-fake-clock".into()).spawn(move || clock_thread(rx, sink, pending));
            Clock { tx, busy_until: [None; 2] }
        });
        Audio { sink, vendor, product, config: 0, alts: [0; 3], ramp: 0, played: 0, played_sum: 0, pending, clock }
    }

    fn device_descriptor(&self) -> Vec<u8> {
        let mut d = vec![18, 1, 0x10, 0x01, 0, 0, 0, 64];
        d.extend_from_slice(&self.vendor.to_le_bytes());
        d.extend_from_slice(&self.product.to_le_bytes());
        d.extend_from_slice(&[0x00, 0x01, 1, 2, 3, 1]);
        d
    }

    /// Audio control (interface 0: USB stream to speaker, microphone to
    /// USB stream), then the two streaming interfaces, each with an
    /// empty alternate setting 0 and the endpoint in setting 1.
    pub fn config_descriptor() -> Vec<u8> {
        let mut c = vec![9, 2, 0, 0, 3, 1, 0, 0x80, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 0, 1, 1, 0, 0]);
        // Header (UAC 1.0): its length plus the terminals', the two
        // streaming interfaces.
        c.extend_from_slice(&[10, 0x24, 1, 0x00, 0x01, 52, 0, 2, 1, 2]);
        // Input terminal 1, USB streaming, 2 channels (left, right).
        c.extend_from_slice(&[12, 0x24, 2, 1, 0x01, 0x01, 0, 2, 0x03, 0x00, 0, 0]);
        // Output terminal 2, speaker, from 1.
        c.extend_from_slice(&[9, 0x24, 3, 2, 0x01, 0x03, 0, 1, 0]);
        // Input terminal 3, microphone, 1 channel.
        c.extend_from_slice(&[12, 0x24, 2, 3, 0x01, 0x02, 0, 1, 0x00, 0x00, 0, 0]);
        // Output terminal 4, USB streaming, from 3.
        c.extend_from_slice(&[9, 0x24, 3, 4, 0x01, 0x01, 0, 3, 0]);
        for (iface, terminal, channels, ep, attrs, max) in [(1u8, 1u8, 2u8, AUDIO_OUT, 0x09u8, 200u16), (2, 4, 1, AUDIO_IN, 0x05, 100)] {
            c.extend_from_slice(&[9, 4, iface, 0, 0, 1, 2, 0, 0]);
            c.extend_from_slice(&[9, 4, iface, 1, 1, 1, 2, 0, 0]);
            // General: terminal link, delay, PCM.
            c.extend_from_slice(&[7, 0x24, 1, terminal, 1, 0x01, 0x00]);
            // Type I: channels, 2 bytes per sample, 16 bits, one rate: 48000.
            c.extend_from_slice(&[11, 0x24, 2, 1, channels, 2, 16, 1, 0x80, 0xbb, 0x00]);
            // Isochronous (adaptive out, asynchronous in), every frame.
            let [lo, hi] = max.to_le_bytes();
            c.extend_from_slice(&[9, 5, ep, attrs, lo, hi, 1, 0, 0]);
            c.extend_from_slice(&[7, 0x25, 1, 0, 0, 0, 0]);
        }
        let n = c.len() as u16;
        c[2..4].copy_from_slice(&n.to_le_bytes());
        c
    }

    fn control(&mut self, s: &Submit) -> Result<Vec<u8>, Status> {
        let (rt, req) = (s.setup[0], s.setup[1]);
        let value = u16::from_le_bytes([s.setup[2], s.setup[3]]);
        let index = u16::from_le_bytes([s.setup[4], s.setup[5]]);
        Ok(match (rt, req) {
            (0x80, 6) => match (value >> 8) as u8 {
                1 => self.device_descriptor(),
                2 if value & 0xff == 0 => Self::config_descriptor(),
                3 => match value as u8 {
                    0 => vec![4, 3, 0x09, 0x04],
                    1 => string_desc("vroom"),
                    2 => string_desc("Test sound card"),
                    3 => string_desc("0001"),
                    _ => return Err(Status::Stall),
                },
                _ => return Err(Status::Stall),
            },
            (0x80..=0x82, 0) => vec![0, 0],
            (0x80, 8) => vec![self.config],
            (0x81, 10) => vec![*self.alts.get(index as usize).ok_or(Status::Stall)?],
            _ => return Err(Status::Stall),
        })
    }

    /// Endpoint `ep` (0 out, 1 in) finishes `s` with `ev`: now, or when
    /// its packets have had their frames.
    fn complete(&mut self, ep: usize, s: &IsoSubmit, ev: Event) {
        let Some(c) = self.clock.as_mut() else {
            let _ = self.sink.send(ev);
            return;
        };
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(s.id);
        let now = Instant::now();
        let start = c.busy_until[ep].filter(|&t| t > now).unwrap_or(now);
        let due = start + Duration::from_millis(s.packets.len() as u64);
        c.busy_until[ep] = Some(due);
        let _ = c.tx.send((due, s.id, ev));
    }
}

impl Device for Audio {
    fn submit(&mut self, s: Submit) -> Result<(), Status> {
        if s.kind != XferType::Control {
            return Err(Status::Stall);
        }
        let mut r = self.control(&s)?;
        if s.is_in() {
            r.truncate(s.len as usize);
            let _ = self.sink.send(Event::Done(Done { id: s.id, status: Status::Ok, actual: r.len() as u32, data: r }));
        } else {
            let _ = self.sink.send(Event::Done(Done { id: s.id, status: Status::Ok, actual: s.len, data: Vec::new() }));
        }
        Ok(())
    }

    fn submit_iso(&mut self, s: IsoSubmit) -> Result<(), Status> {
        let ok = |actual| IsoPacket { status: Status::Ok, actual };
        match s.ep {
            AUDIO_OUT if self.alts[1] == 1 => {
                self.played += s.data.len() as u64;
                self.played_sum = s.data.iter().fold(self.played_sum, |a, &b| a.wrapping_add(b as u64));
                let packets = s.packets.iter().map(|&n| ok(n)).collect();
                let ev = Event::IsoDone(IsoDone { id: s.id, status: Status::Ok, packets, data: Vec::new() });
                self.complete(0, &s, ev);
            }
            AUDIO_IN if self.alts[2] == 1 => {
                let mut data = Vec::with_capacity(s.len());
                let mut packets = Vec::with_capacity(s.packets.len());
                for &n in &s.packets {
                    let n = (n as usize).min(MIC_PACKET) & !1;
                    for _ in 0..n / 2 {
                        data.extend_from_slice(&self.ramp.to_le_bytes());
                        self.ramp = self.ramp.wrapping_add(1);
                    }
                    packets.push(ok(n as u32));
                }
                let ev = Event::IsoDone(IsoDone { id: s.id, status: Status::Ok, packets, data });
                self.complete(1, &s, ev);
            }
            _ => return Err(Status::Stall),
        }
        Ok(())
    }

    fn cancel(&mut self, id: u32) {
        if self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
            let _ = self.sink.send(Event::IsoDone(IsoDone::failed(id, Status::Cancelled)));
        }
    }

    fn in_flight(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn busy(&self, id: u32) -> bool {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).contains(&id)
    }

    fn reset(&mut self) -> Result<(), Status> {
        let ids: Vec<u32> = self.pending.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect();
        for id in ids {
            self.cancel(id);
        }
        self.config = 0;
        self.alts = [0; 3];
        Ok(())
    }

    fn config_descriptor(&mut self, value: u8) -> Option<Vec<u8>> {
        (value == 1).then(Self::config_descriptor)
    }

    fn set_configuration(&mut self, value: u8, _cfg: Option<&Config>) -> Result<(), Status> {
        self.config = value;
        self.alts = [0; 3];
        Ok(())
    }

    fn set_interface(&mut self, iface: u8, alt: u8) -> Result<(), Status> {
        match (iface, alt) {
            (0, 0) => {}
            (1 | 2, 0 | 1) => {
                let i = iface as usize;
                if i == 1 && self.alts[1] == 1 && alt == 0 {
                    log::info!("usb: pretend sound card: {} bytes played so far (sum {})", self.played, self.played_sum);
                }
                self.alts[i] = alt;
                if let Some(c) = self.clock.as_mut() {
                    c.busy_until[i - 1] = None;
                }
            }
            _ => return Err(Status::Stall),
        }
        Ok(())
    }

    fn clear_halt(&mut self, _ep: u8) -> Result<(), Status> {
        Ok(())
    }

    fn gone(&self) -> bool {
        false
    }
}
