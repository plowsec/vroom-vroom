//! Wire protocol between the VMM and the USB helper.
//!
//! A stream socketpair carries messages `[kind u8][0 u8][0 u16][id u32 le]
//! [len u32 le][payload]`. Each side treats the other as hostile: lengths
//! are capped before anything is allocated, every field is checked, and a
//! malformed message ends the connection.
//!
//! VMM -> helper ([`Request`]):
//! - Submit (1): `[ep u8][type u8][0 u16][length u32][setup 8][OUT data]`.
//!   `length` is what an IN transfer wants, or the size of the OUT data.
//! - Cancel (2): the transfer `id`. Reset (3): reset the device.
//! - IsoSubmit (4): `[ep u8][0; 3][count u32][length u32 * count][OUT data]`,
//!   an isochronous transfer: one packet per service interval.
//!
//! Helper -> VMM ([`Event`]):
//! - Attached (0x81): `[speed u8][0 u8][vendor u16][product u16]`.
//! - Detached (0x82).
//! - Done (0x83): `[status u8][0; 3][actual u32][IN data]`.
//! - IsoDone (0x84): `[status u8][0; 3][count u32][(status u8, 0; 3,
//!   actual u32) * count][IN data]`, the packets' data back to back.

use std::io::{self, Read, Write};

/// Largest single transfer, either way.
pub const MAX_DATA: usize = 1 << 20;
/// Transfers the helper accepts in flight at once (per device).
pub const MAX_INFLIGHT: usize = 64;
const HDR: usize = 12;
const SUBMIT_HDR: usize = 16;
/// Isochronous: most packets in one transfer (Linux's usbfs takes 128),
/// and the largest packet (SuperSpeedPlus moves up to 96 KiB per
/// interval).
pub const MAX_ISO_PACKETS: usize = 128;
pub const MAX_ISO_PACKET: u32 = 128 << 10;
const ISO_HDR: usize = 8;
const MAX_PAYLOAD: usize = ISO_HDR + 8 * MAX_ISO_PACKETS + MAX_DATA;

const K_SUBMIT: u8 = 1;
const K_CANCEL: u8 = 2;
const K_RESET: u8 = 3;
const K_ISO_SUBMIT: u8 = 4;
const K_ATTACHED: u8 = 0x81;
const K_DETACHED: u8 = 0x82;
const K_DONE: u8 = 0x83;
const K_ISO_DONE: u8 = 0x84;

/// Device speed, as the host saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Low,
    Full,
    High,
    Super,
    SuperPlus,
}

impl Speed {
    fn wire(self) -> u8 {
        match self {
            Speed::Low => 1,
            Speed::Full => 2,
            Speed::High => 3,
            Speed::Super => 4,
            Speed::SuperPlus => 5,
        }
    }

    fn from_wire(v: u8) -> Option<Speed> {
        Some(match v {
            1 => Speed::Low,
            2 => Speed::Full,
            3 => Speed::High,
            4 => Speed::Super,
            5 => Speed::SuperPlus,
            _ => return None,
        })
    }

    /// SuperSpeed and up: goes on a USB 3 port.
    pub fn is_super(self) -> bool {
        matches!(self, Speed::Super | Speed::SuperPlus)
    }
}

/// Endpoint transfer type (bmAttributes bits 0-1). Isochronous transfers
/// go in their own message ([`IsoSubmit`]), never in a [`Submit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XferType {
    Control,
    Iso,
    Bulk,
    Interrupt,
}

impl XferType {
    fn wire(self) -> u8 {
        match self {
            XferType::Control => 0,
            XferType::Iso => 1,
            XferType::Bulk => 2,
            XferType::Interrupt => 3,
        }
    }

    fn from_wire(v: u8) -> Option<XferType> {
        Some(match v {
            0 => XferType::Control,
            2 => XferType::Bulk,
            3 => XferType::Interrupt,
            _ => return None,
        })
    }
}

/// One transfer for the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submit {
    pub id: u32,
    /// Endpoint address: number, bit 7 set for IN. 0 for control.
    pub ep: u8,
    pub kind: XferType,
    /// SETUP packet (control only; zero otherwise).
    pub setup: [u8; 8],
    /// IN: bytes wanted. OUT: `data.len()`.
    pub len: u32,
    pub data: Vec<u8>,
    /// Bulk OUT: end with a zero-length packet if the data fills the last one.
    pub zero_packet: bool,
}

impl Submit {
    /// Data flows device to host.
    pub fn is_in(&self) -> bool {
        match self.kind {
            XferType::Control => self.setup[0] & 0x80 != 0,
            _ => self.ep & 0x80 != 0,
        }
    }

    pub fn control(id: u32, setup: [u8; 8], out: Vec<u8>) -> Submit {
        let len = u16::from_le_bytes([setup[6], setup[7]]) as u32;
        Submit { id, ep: 0, kind: XferType::Control, setup, len, data: out, zero_packet: false }
    }
}

/// An isochronous transfer: `packets` are the lengths, one packet per
/// service interval of the endpoint, in order. The host schedules it as
/// soon as it can, right after what the endpoint already has queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoSubmit {
    pub id: u32,
    /// Endpoint address: number (not 0), bit 7 set for IN.
    pub ep: u8,
    pub packets: Vec<u32>,
    /// OUT: the packets back to back (as long as their lengths add up
    /// to). IN: empty.
    pub data: Vec<u8>,
}

impl IsoSubmit {
    pub fn is_in(&self) -> bool {
        self.ep & 0x80 != 0
    }

    /// Bytes asked for, all packets.
    pub fn len(&self) -> usize {
        self.packets.iter().map(|&n| n as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Submit(Submit),
    IsoSubmit(IsoSubmit),
    Cancel { id: u32 },
    Reset,
}

/// How a transfer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// The device refused (STALL): the endpoint halts.
    Stall,
    /// Transfer failed on the bus (or the helper refused it).
    Error,
    /// The device sent more than asked for.
    Babble,
    Cancelled,
    /// Unplugged.
    NoDevice,
}

impl Status {
    fn wire(self) -> u8 {
        match self {
            Status::Ok => 0,
            Status::Stall => 1,
            Status::Error => 2,
            Status::Babble => 3,
            Status::Cancelled => 4,
            Status::NoDevice => 5,
        }
    }

    fn from_wire(v: u8) -> Option<Status> {
        Some(match v {
            0 => Status::Ok,
            1 => Status::Stall,
            2 => Status::Error,
            3 => Status::Babble,
            4 => Status::Cancelled,
            5 => Status::NoDevice,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub id: u32,
    pub status: Status,
    /// Bytes moved. IN: `data.len()`.
    pub actual: u32,
    pub data: Vec<u8>,
}

/// How one isochronous packet went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoPacket {
    pub status: Status,
    /// Bytes moved.
    pub actual: u32,
}

/// An isochronous transfer is over. `status` is the transfer's own (not
/// Ok: it never ran, `packets` may then be empty); each packet has its
/// own result. IN data: each packet's `actual` bytes, back to back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoDone {
    pub id: u32,
    pub status: Status,
    pub packets: Vec<IsoPacket>,
    pub data: Vec<u8>,
}

impl IsoDone {
    /// Failed as a whole.
    pub fn failed(id: u32, status: Status) -> IsoDone {
        IsoDone { id, status, packets: Vec::new(), data: Vec::new() }
    }

    /// Bytes moved, all packets.
    pub fn actual(&self) -> usize {
        self.packets.iter().map(|p| p.actual as usize).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Attached { speed: Speed, vendor: u16, product: u16 },
    Detached,
    Done(Done),
    IsoDone(IsoDone),
}

fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("usb protocol: {m}"))
}

fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HDR + payload.len());
    v.extend_from_slice(&[kind, 0, 0, 0]);
    v.extend_from_slice(&id.to_le_bytes());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Request::Submit(s) => {
                let mut p = Vec::with_capacity(SUBMIT_HDR + s.data.len());
                p.extend_from_slice(&[s.ep, s.kind.wire(), s.zero_packet as u8, 0]);
                p.extend_from_slice(&s.len.to_le_bytes());
                p.extend_from_slice(&s.setup);
                p.extend_from_slice(&s.data);
                frame(K_SUBMIT, s.id, &p)
            }
            Request::IsoSubmit(s) => {
                let mut p = Vec::with_capacity(ISO_HDR + 4 * s.packets.len() + s.data.len());
                p.extend_from_slice(&[s.ep, 0, 0, 0]);
                p.extend_from_slice(&(s.packets.len() as u32).to_le_bytes());
                for n in &s.packets {
                    p.extend_from_slice(&n.to_le_bytes());
                }
                p.extend_from_slice(&s.data);
                frame(K_ISO_SUBMIT, s.id, &p)
            }
            Request::Cancel { id } => frame(K_CANCEL, *id, &[]),
            Request::Reset => frame(K_RESET, 0, &[]),
        }
    }

    /// Parse one request (`kind`, `id`, payload). Structural checks only;
    /// the helper checks endpoints against the device on top.
    pub fn decode(kind: u8, id: u32, p: &[u8]) -> io::Result<Request> {
        match kind {
            K_SUBMIT => {
                if p.len() < SUBMIT_HDR {
                    return Err(bad("short submit"));
                }
                let ep = p[0];
                let kind = XferType::from_wire(p[1]).ok_or_else(|| bad("transfer type"))?;
                if p[2] > 1 || p[3] != 0 {
                    return Err(bad("reserved bits"));
                }
                let zero_packet = p[2] == 1;
                let len = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
                let mut setup = [0u8; 8];
                setup.copy_from_slice(&p[8..16]);
                let data = p[SUBMIT_HDR..].to_vec();
                if len as usize > MAX_DATA {
                    return Err(bad("transfer too large"));
                }
                // Endpoint address: number 0-15 and the direction bit.
                if ep & 0x70 != 0 {
                    return Err(bad("endpoint address"));
                }
                let s = Submit { id, ep, kind, setup, len, data, zero_packet };
                if zero_packet && (kind != XferType::Bulk || s.is_in()) {
                    return Err(bad("zero packet is for bulk OUT"));
                }
                match kind {
                    XferType::Control => {
                        if ep != 0 || u16::from_le_bytes([setup[6], setup[7]]) as u32 != len {
                            return Err(bad("control transfer"));
                        }
                    }
                    _ => {
                        if ep & 0x0f == 0 || setup != [0; 8] {
                            return Err(bad("endpoint 0 is control only"));
                        }
                    }
                }
                let want = if s.is_in() { 0 } else { len as usize };
                if s.data.len() != want {
                    return Err(bad("data length"));
                }
                Ok(Request::Submit(s))
            }
            K_ISO_SUBMIT => {
                if p.len() < ISO_HDR || p[1..4] != [0, 0, 0] {
                    return Err(bad("short isochronous submit"));
                }
                let ep = p[0];
                if ep & 0x70 != 0 || ep & 0x0f == 0 {
                    return Err(bad("endpoint address"));
                }
                let n = u32::from_le_bytes([p[4], p[5], p[6], p[7]]) as usize;
                if n == 0 || n > MAX_ISO_PACKETS || p.len() < ISO_HDR + 4 * n {
                    return Err(bad("isochronous packet count"));
                }
                let packets: Vec<u32> = p[ISO_HDR..ISO_HDR + 4 * n].chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
                if packets.iter().any(|&l| l > MAX_ISO_PACKET) {
                    return Err(bad("isochronous packet too large"));
                }
                let s = IsoSubmit { id, ep, packets, data: p[ISO_HDR + 4 * n..].to_vec() };
                if s.len() > MAX_DATA || s.data.len() != if s.is_in() { 0 } else { s.len() } {
                    return Err(bad("isochronous data length"));
                }
                Ok(Request::IsoSubmit(s))
            }
            K_CANCEL if p.is_empty() => Ok(Request::Cancel { id }),
            K_RESET if p.is_empty() && id == 0 => Ok(Request::Reset),
            _ => Err(bad("unknown request")),
        }
    }
}

impl Event {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Event::Attached { speed, vendor, product } => {
                let mut p = vec![speed.wire(), 0];
                p.extend_from_slice(&vendor.to_le_bytes());
                p.extend_from_slice(&product.to_le_bytes());
                frame(K_ATTACHED, 0, &p)
            }
            Event::Detached => frame(K_DETACHED, 0, &[]),
            Event::Done(d) => {
                let mut p = Vec::with_capacity(8 + d.data.len());
                p.extend_from_slice(&[d.status.wire(), 0, 0, 0]);
                p.extend_from_slice(&d.actual.to_le_bytes());
                p.extend_from_slice(&d.data);
                frame(K_DONE, d.id, &p)
            }
            Event::IsoDone(d) => {
                let mut p = Vec::with_capacity(ISO_HDR + 8 * d.packets.len() + d.data.len());
                p.extend_from_slice(&[d.status.wire(), 0, 0, 0]);
                p.extend_from_slice(&(d.packets.len() as u32).to_le_bytes());
                for k in &d.packets {
                    p.extend_from_slice(&[k.status.wire(), 0, 0, 0]);
                    p.extend_from_slice(&k.actual.to_le_bytes());
                }
                p.extend_from_slice(&d.data);
                frame(K_ISO_DONE, d.id, &p)
            }
        }
    }

    /// Parse one event. The VMM checks `Done` against the transfer it
    /// sent (direction, size) on top.
    pub fn decode(kind: u8, id: u32, p: &[u8]) -> io::Result<Event> {
        match kind {
            K_ATTACHED if p.len() == 6 && id == 0 && p[1] == 0 => {
                let speed = Speed::from_wire(p[0]).ok_or_else(|| bad("speed"))?;
                Ok(Event::Attached { speed, vendor: u16::from_le_bytes([p[2], p[3]]), product: u16::from_le_bytes([p[4], p[5]]) })
            }
            K_DETACHED if p.is_empty() && id == 0 => Ok(Event::Detached),
            K_DONE if p.len() >= 8 && p[1..4] == [0, 0, 0] => {
                let status = Status::from_wire(p[0]).ok_or_else(|| bad("status"))?;
                let actual = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
                let data = p[8..].to_vec();
                if actual as usize > MAX_DATA || (!data.is_empty() && data.len() != actual as usize) {
                    return Err(bad("completion length"));
                }
                Ok(Event::Done(Done { id, status, actual, data }))
            }
            K_ISO_DONE if p.len() >= ISO_HDR && p[1..4] == [0, 0, 0] => {
                let status = Status::from_wire(p[0]).ok_or_else(|| bad("status"))?;
                let n = u32::from_le_bytes([p[4], p[5], p[6], p[7]]) as usize;
                if n > MAX_ISO_PACKETS || p.len() < ISO_HDR + 8 * n {
                    return Err(bad("isochronous packet count"));
                }
                let mut packets = Vec::with_capacity(n);
                for c in p[ISO_HDR..ISO_HDR + 8 * n].chunks_exact(8) {
                    let status = Status::from_wire(c[0]).ok_or_else(|| bad("status"))?;
                    let actual = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
                    if c[1..4] != [0, 0, 0] || actual > MAX_ISO_PACKET {
                        return Err(bad("isochronous packet"));
                    }
                    packets.push(IsoPacket { status, actual });
                }
                let d = IsoDone { id, status, packets, data: p[ISO_HDR + 8 * n..].to_vec() };
                if !d.data.is_empty() && d.data.len() != d.actual() {
                    return Err(bad("completion length"));
                }
                Ok(Event::IsoDone(d))
            }
            _ => Err(bad("unknown event")),
        }
    }
}

/// Read one frame: (kind, id, payload). None at a clean end of stream.
/// The length is checked before the payload is allocated.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, u32, Vec<u8>)>> {
    let mut h = [0u8; HDR];
    let mut got = 0;
    while got < HDR {
        match r.read(&mut h[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(bad("truncated header")),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    if h[1..4] != [0, 0, 0] {
        return Err(bad("header"));
    }
    let id = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
    let len = u32::from_le_bytes([h[8], h[9], h[10], h[11]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(bad("message too large"));
    }
    let mut p = vec![0u8; len];
    r.read_exact(&mut p)?;
    Ok(Some((h[0], id, p)))
}

pub fn read_request(r: &mut impl Read) -> io::Result<Option<Request>> {
    match read_frame(r)? {
        Some((k, id, p)) => Request::decode(k, id, &p).map(Some),
        None => Ok(None),
    }
}

pub fn read_event(r: &mut impl Read) -> io::Result<Option<Event>> {
    match read_frame(r)? {
        Some((k, id, p)) => Event::decode(k, id, &p).map(Some),
        None => Ok(None),
    }
}

pub fn write_msg(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_req(r: Request) {
        let b = r.encode();
        assert_eq!(read_request(&mut &b[..]).unwrap(), Some(r));
    }

    fn rt_ev(e: Event) {
        let b = e.encode();
        assert_eq!(read_event(&mut &b[..]).unwrap(), Some(e));
    }

    #[test]
    fn round_trips() {
        rt_req(Request::Submit(Submit::control(7, [0x80, 6, 0, 1, 0, 0, 18, 0], vec![])));
        rt_req(Request::Submit(Submit::control(8, [0x21, 0x20, 0, 0, 0, 0, 7, 0], vec![1; 7])));
        rt_req(Request::Submit(Submit { id: 9, ep: 0x02, kind: XferType::Bulk, setup: [0; 8], len: 3, data: vec![1, 2, 3], zero_packet: false }));
        rt_req(Request::Submit(Submit { id: 10, ep: 0x81, kind: XferType::Interrupt, setup: [0; 8], len: 64, data: vec![], zero_packet: false }));
        rt_req(Request::Cancel { id: 10 });
        rt_req(Request::Reset);
        rt_ev(Event::Attached { speed: Speed::High, vendor: 0x1050, product: 0x0407 });
        rt_ev(Event::Detached);
        rt_ev(Event::Done(Done { id: 3, status: Status::Ok, actual: 2, data: vec![9, 9] }));
        rt_ev(Event::Done(Done { id: 4, status: Status::Stall, actual: 0, data: vec![] }));
        assert_eq!(read_request(&mut &[][..]).unwrap(), None);
    }

    #[test]
    fn isochronous_round_trips() {
        rt_req(Request::IsoSubmit(IsoSubmit { id: 11, ep: 0x82, packets: vec![96; 8], data: vec![] }));
        rt_req(Request::IsoSubmit(IsoSubmit { id: 12, ep: 0x01, packets: vec![2, 0, 1], data: vec![1, 2, 3] }));
        let pk = |status, actual| IsoPacket { status, actual };
        rt_ev(Event::IsoDone(IsoDone { id: 11, status: Status::Ok, packets: vec![pk(Status::Ok, 2), pk(Status::Error, 0), pk(Status::Ok, 1)], data: vec![7, 8, 9] }));
        rt_ev(Event::IsoDone(IsoDone { id: 12, status: Status::Ok, packets: vec![pk(Status::Ok, 2), pk(Status::Ok, 1)], data: vec![] }));
        rt_ev(Event::IsoDone(IsoDone::failed(13, Status::NoDevice)));
    }

    #[test]
    fn rejects_bad_isochronous() {
        let enc = |s: IsoSubmit| Request::IsoSubmit(s).encode();
        let iso = IsoSubmit { id: 1, ep: 0x01, packets: vec![2, 1], data: vec![1, 2, 3] };
        let cases = [
            // Data not adding up, IN with data, endpoint 0, reserved bits.
            enc(IsoSubmit { data: vec![1, 2], ..iso.clone() }),
            enc(IsoSubmit { ep: 0x81, ..iso.clone() }),
            enc(IsoSubmit { ep: 0x00, ..iso.clone() }),
            enc(IsoSubmit { ep: 0x21, ..iso.clone() }),
            // No packets, too many, one too large.
            enc(IsoSubmit { packets: vec![], data: vec![], ..iso.clone() }),
            enc(IsoSubmit { ep: 0x81, packets: vec![1; MAX_ISO_PACKETS + 1], data: vec![], ..iso.clone() }),
            enc(IsoSubmit { ep: 0x81, packets: vec![MAX_ISO_PACKET + 1], data: vec![], ..iso.clone() }),
            // More in all than a transfer may carry.
            enc(IsoSubmit { ep: 0x81, packets: vec![MAX_ISO_PACKET; 9], data: vec![], ..iso.clone() }),
        ];
        for c in cases {
            assert!(read_request(&mut &c[..]).is_err(), "{c:?}");
        }
        // A count larger than what follows.
        let mut b = enc(iso.clone());
        b[HDR + 4] = 50;
        assert!(read_request(&mut &b[..]).is_err());
        // Plain submit claiming to be isochronous.
        let mut b = Request::Submit(Submit { id: 1, ep: 0x01, kind: XferType::Bulk, setup: [0; 8], len: 0, data: vec![], zero_packet: false }).encode();
        b[HDR + 1] = 1;
        assert!(read_request(&mut &b[..]).is_err());
        // Completions: data not matching the packets, a packet too large.
        let pk = IsoPacket { status: Status::Ok, actual: 2 };
        let d = Event::IsoDone(IsoDone { id: 1, status: Status::Ok, packets: vec![pk], data: vec![1, 2, 3] }).encode();
        assert!(read_event(&mut &d[..]).is_err());
        let d = Event::IsoDone(IsoDone { id: 1, status: Status::Ok, packets: vec![IsoPacket { status: Status::Ok, actual: MAX_ISO_PACKET + 1 }], data: vec![] }).encode();
        assert!(read_event(&mut &d[..]).is_err());
    }

    #[test]
    fn rejects_bad_requests() {
        let enc = |s: Submit| Request::Submit(s).encode();
        let bulk = Submit { id: 1, ep: 0x02, kind: XferType::Bulk, setup: [0; 8], len: 3, data: vec![1, 2, 3], zero_packet: false };
        let cases = [
            // OUT data doesn't match the length.
            enc(Submit { len: 4, ..bulk.clone() }),
            // IN with data.
            enc(Submit { ep: 0x82, ..bulk.clone() }),
            // Bulk on endpoint 0, reserved address bits.
            enc(Submit { ep: 0x80, data: vec![], ..bulk.clone() }),
            enc(Submit { ep: 0x12, ..bulk.clone() }),
            // Control length not matching wLength, control off endpoint 0.
            enc(Submit { kind: XferType::Control, ep: 0, setup: [0, 9, 1, 0, 0, 0, 5, 0], ..bulk.clone() }),
            enc(Submit { kind: XferType::Control, ep: 1, setup: [0, 9, 1, 0, 0, 0, 3, 0], ..bulk.clone() }),
        ];
        for c in cases {
            assert!(read_request(&mut &c[..]).is_err(), "{c:?}");
        }
        // Huge length: refused before allocating.
        let mut b = frame(K_SUBMIT, 1, &[]);
        b[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_request(&mut &b[..]).is_err());
        // Completion data not matching `actual`.
        let mut d = Event::Done(Done { id: 1, status: Status::Ok, actual: 2, data: vec![1, 2] }).encode();
        d[16] = 3;
        assert!(read_event(&mut &d[..]).is_err());
        // Truncated payload.
        let r = Request::Cancel { id: 1 }.encode();
        let mut t = r.clone();
        t[8] = 4;
        assert!(read_request(&mut &t[..]).is_err());
    }
}
