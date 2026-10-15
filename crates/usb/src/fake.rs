//! A pretend device behind the helper, for tests without hardware: a
//! CDC-ACM serial port that echoes what it is sent (control, bulk in and
//! out, interrupt in). Guests drive it with their stock ACM driver
//! (`/dev/ttyACM0` on Linux). Enabled with `VROOM_USB_FAKE=acm`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::desc::Config;
use crate::helper::{Device, Opener, Sink};
use crate::proto::{Done, Event, Speed, Status, Submit, XferType};

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
        let mut v = vec![0, 3];
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v[0] = v.len() as u8;
        Some(v)
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

/// Opens the fake device. `unplug_after`: pull it out once after that
/// long, and put it back three seconds later (hotplug tests).
#[derive(Debug)]
pub struct FakeOpener {
    vendor: u16,
    product: u16,
    start: Instant,
    unplug_after: Option<Duration>,
}

impl FakeOpener {
    pub fn new(vendor: u16, product: u16, unplug_after: Option<Duration>) -> FakeOpener {
        FakeOpener { vendor, product, start: Instant::now(), unplug_after }
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
        Some((Box::new(Acm::new(sink.clone(), self.vendor, self.product)), Speed::Full, self.vendor, self.product))
    }

    fn present(&mut self) -> bool {
        !self.unplugged()
    }
}
