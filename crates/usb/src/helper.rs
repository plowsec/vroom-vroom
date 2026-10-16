//! The helper's core, the same for libusb and the test device: checks
//! every request from the VMM against the device's own descriptors,
//! handles the standard requests that change host-side state
//! (configuration, alternate setting, halt), and passes the rest on.
//!
//! The VMM is treated as hostile. It can only reach the one device the
//! helper was started for, only endpoints of the configuration and
//! alternate settings in use, with bounded sizes and a bounded number of
//! transfers in flight. It can't pick another device or address one.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::desc::{Config, Endpoint};
use crate::proto::{self, Done, Event, IsoDone, IsoSubmit, Request, Speed, Status, Submit, XferType, MAX_INFLIGHT};

/// Where completions and attach/detach notices go (to the VMM).
pub type Sink = Sender<Event>;

/// An opened host device, or a stand-in for tests.
pub trait Device: Send {
    /// Start a transfer. Its completion goes to the sink, later or
    /// right away; an error here means it never started.
    fn submit(&mut self, s: Submit) -> Result<(), Status>;
    /// Start an isochronous transfer, the same way (its completion is an
    /// [`Event::IsoDone`]).
    fn submit_iso(&mut self, s: IsoSubmit) -> Result<(), Status>;
    /// Stop a transfer; its completion (Cancelled, or done after all)
    /// still comes.
    fn cancel(&mut self, id: u32);
    fn in_flight(&self) -> usize;
    fn busy(&self, id: u32) -> bool;
    fn reset(&mut self) -> Result<(), Status>;
    /// The raw configuration descriptor whose bConfigurationValue is `value`.
    fn config_descriptor(&mut self, value: u8) -> Option<Vec<u8>>;
    /// Select a configuration (0: none) and claim its interfaces.
    fn set_configuration(&mut self, value: u8, cfg: Option<&Config>) -> Result<(), Status>;
    fn set_interface(&mut self, iface: u8, alt: u8) -> Result<(), Status>;
    fn clear_halt(&mut self, ep: u8) -> Result<(), Status>;
    /// A transfer found the device unplugged.
    fn gone(&self) -> bool;
}

/// Finds and opens the one device the helper is for.
pub trait Opener: Send {
    fn open(&mut self, sink: &Sink) -> Option<(Box<dyn Device>, Speed, u16, u16)>;
    /// Is the opened device still there?
    fn present(&mut self) -> bool;
}

// Standard requests the helper handles itself.
const SET_ADDRESS: u8 = 5;
const SET_CONFIGURATION: u8 = 9;
const SET_INTERFACE: u8 = 11;
const CLEAR_FEATURE: u8 = 1;

/// Host-side view of the device's state: the configuration and alternate
/// settings the guest selected.
#[derive(Debug)]
pub struct Session {
    cfg: Option<Config>,
    alts: BTreeMap<u8, u8>,
    sink: Sink,
}

impl Session {
    pub fn new(sink: Sink) -> Session {
        Session { cfg: None, alts: BTreeMap::new(), sink }
    }

    /// Back to unconfigured (reset, unplug).
    pub fn reset(&mut self) {
        self.cfg = None;
        self.alts.clear();
    }

    fn done(&self, id: u32, status: Status) {
        let _ = self.sink.send(Event::Done(Done { id, status, actual: 0, data: Vec::new() }));
    }

    /// The endpoint at `addr` in the configuration and alternate settings
    /// in use.
    pub fn endpoint(&self, addr: u8) -> Option<Endpoint> {
        let cfg = self.cfg.as_ref()?;
        cfg.interfaces().into_iter().find_map(|i| {
            let alt = self.alts.get(&i).copied().unwrap_or(0);
            cfg.alt(i, alt)?.endpoints.iter().find(|e| e.addr == addr).copied()
        })
    }

    /// One request from the VMM. `dev` is None while nothing is plugged in.
    pub fn request(&mut self, dev: Option<&mut (dyn Device + '_)>, r: Request) {
        let Some(dev) = dev else {
            match r {
                Request::Submit(s) => self.done(s.id, Status::NoDevice),
                Request::IsoSubmit(s) => self.iso_done(s.id, Status::NoDevice),
                _ => {}
            }
            return;
        };
        match r {
            Request::Submit(s) => self.submit(dev, s),
            Request::IsoSubmit(s) => self.submit_iso(dev, s),
            Request::Cancel { id } => dev.cancel(id),
            Request::Reset => {
                if let Err(e) = dev.reset() {
                    log::info!("usb: reset: {e:?}");
                }
                self.reset();
            }
        }
    }

    fn iso_done(&self, id: u32, status: Status) {
        let _ = self.sink.send(Event::IsoDone(IsoDone::failed(id, status)));
    }

    /// Isochronous: an endpoint of that type in use, and no packet
    /// larger than it moves per interval.
    fn submit_iso(&mut self, dev: &mut dyn Device, s: IsoSubmit) {
        let id = s.id;
        if dev.in_flight() >= MAX_INFLIGHT || dev.busy(id) {
            log::debug!("usb: transfer {id} refused (too many, or id in use)");
            return self.iso_done(id, Status::Error);
        }
        let r = match self.endpoint(s.ep) {
            Some(Endpoint { kind: Some(XferType::Iso), per_interval, .. }) => {
                if s.packets.iter().all(|&n| n <= per_interval) {
                    dev.submit_iso(s)
                } else {
                    log::debug!("usb: transfer {id}: packets larger than endpoint {:#x} takes ({per_interval})", s.ep);
                    Err(Status::Error)
                }
            }
            _ => {
                log::debug!("usb: transfer {id} for endpoint {:#x} (isochronous), not in use", s.ep);
                Err(Status::Stall)
            }
        };
        if let Err(st) = r {
            self.iso_done(id, st);
        }
    }

    fn submit(&mut self, dev: &mut dyn Device, s: Submit) {
        let id = s.id;
        if dev.in_flight() >= MAX_INFLIGHT || dev.busy(id) {
            log::debug!("usb: transfer {id} refused (too many, or id in use)");
            return self.done(id, Status::Error);
        }
        let r = match s.kind {
            // Isochronous has its own request.
            XferType::Iso => Err(Status::Error),
            XferType::Control => match self.standard(dev, &s) {
                Some(r) => r.map(|_| self.done(id, Status::Ok)),
                None => dev.submit(s),
            },
            kind => match self.endpoint(s.ep) {
                Some(Endpoint { kind: Some(k), .. }) if k == kind => dev.submit(s),
                _ => {
                    log::debug!("usb: transfer {id} for endpoint {:#x} ({kind:?}), not in use", s.ep);
                    Err(Status::Stall)
                }
            },
        };
        if let Err(st) = r {
            self.done(id, st);
        }
    }

    /// Standard requests that change what the host stack holds. None:
    /// pass it to the device as is.
    fn standard(&mut self, dev: &mut dyn Device, s: &Submit) -> Option<Result<(), Status>> {
        let (rt, req) = (s.setup[0], s.setup[1]);
        let value = u16::from_le_bytes([s.setup[2], s.setup[3]]);
        let index = u16::from_le_bytes([s.setup[4], s.setup[5]]);
        Some(match (rt, req) {
            // The host stack owns the address.
            (0x00, SET_ADDRESS) => Err(Status::Stall),
            (0x00, SET_CONFIGURATION) => {
                let v = value as u8;
                if v == 0 {
                    let r = dev.set_configuration(0, None);
                    self.reset();
                    return Some(r);
                }
                let Some(cfg) = dev.config_descriptor(v).and_then(|b| Config::parse(&b)) else {
                    return Some(Err(Status::Stall));
                };
                dev.set_configuration(v, Some(&cfg)).map(|_| {
                    self.cfg = Some(cfg);
                    self.alts.clear();
                })
            }
            (0x01, SET_INTERFACE) => {
                let (iface, alt) = (index as u8, value as u8);
                if !self.cfg.as_ref().is_some_and(|c| c.alt(iface, alt).is_some()) {
                    return Some(Err(Status::Stall));
                }
                dev.set_interface(iface, alt).map(|_| {
                    self.alts.insert(iface, alt);
                })
            }
            // ENDPOINT_HALT: the host side has a data toggle to reset too.
            (0x02, CLEAR_FEATURE) if value == 0 => {
                let ep = index as u8;
                if self.endpoint(ep).is_none() {
                    return Some(Err(Status::Stall));
                }
                dev.clear_halt(ep)
            }
            _ => return None,
        })
    }
}

struct Shared {
    dev: Option<Box<dyn Device>>,
    session: Session,
}

/// Serve the VMM on `sock` until it goes away. The device comes and goes
/// with `opener`, polled once a second.
pub fn serve(sock: UnixStream, mut opener: Box<dyn Opener>) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<Event>();
    let mut wsock = sock.try_clone()?;
    std::thread::Builder::new().name("usb-writer".into()).spawn(move || {
        for ev in rx {
            if wsock.write_all(&ev.encode()).is_err() {
                break;
            }
        }
    })?;
    let shared = Arc::new(Mutex::new(Shared { dev: None, session: Session::new(tx.clone()) }));
    let watch = shared.clone();
    std::thread::Builder::new().name("usb-watch".into()).spawn(move || loop {
        {
            let mut s = watch.lock().unwrap_or_else(|e| e.into_inner());
            let gone = s.dev.as_ref().is_some_and(|d| d.gone());
            if s.dev.is_some() && (gone || !opener.present()) {
                log::info!("usb: device unplugged");
                s.dev = None;
                s.session.reset();
                let _ = tx.send(Event::Detached);
            } else if s.dev.is_none() {
                if let Some((d, speed, vendor, product)) = opener.open(&tx) {
                    log::info!("usb: attached {vendor:04x}:{product:04x} ({speed:?})");
                    s.dev = Some(d);
                    s.session.reset();
                    let _ = tx.send(Event::Attached { speed, vendor, product });
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    })?;
    let mut r = io::BufReader::new(sock);
    let res = loop {
        match proto::read_request(&mut r) {
            Ok(Some(req)) => {
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                let Shared { dev, session } = &mut *s;
                session.request(dev.as_deref_mut(), req);
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    // Let go of the device (release interfaces, close) before exiting.
    shared.lock().unwrap_or_else(|e| e.into_inner()).dev = None;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Acm;

    fn setup(rt: u8, req: u8, value: u16, index: u16, len: u16) -> [u8; 8] {
        let mut s = [rt, req, 0, 0, 0, 0, 0, 0];
        s[2..4].copy_from_slice(&value.to_le_bytes());
        s[4..6].copy_from_slice(&index.to_le_bytes());
        s[6..8].copy_from_slice(&len.to_le_bytes());
        s
    }

    fn done(rx: &mpsc::Receiver<Event>) -> Done {
        match rx.try_recv() {
            Ok(Event::Done(d)) => d,
            other => panic!("expected a completion, got {other:?}"),
        }
    }

    fn bulk(id: u32, ep: u8, len: u32, data: Vec<u8>) -> Request {
        Request::Submit(Submit { id, ep, kind: XferType::Bulk, setup: [0; 8], len, data, zero_packet: false })
    }

    #[test]
    fn checks_endpoints_against_the_configuration() {
        let (tx, rx) = mpsc::channel();
        let mut dev = Acm::new(tx.clone(), 0x1209, 0x0001);
        let mut s = Session::new(tx);
        // Not configured yet: bulk endpoints don't exist.
        s.request(Some(&mut dev), bulk(1, 0x02, 2, vec![1, 2]));
        assert_eq!(done(&rx).status, Status::Stall);
        s.request(Some(&mut dev), Request::Submit(Submit::control(2, setup(0, SET_CONFIGURATION, 1, 0, 0), vec![])));
        assert_eq!(done(&rx).status, Status::Ok);
        // Loopback: OUT then IN.
        s.request(Some(&mut dev), bulk(3, 0x02, 3, vec![7, 8, 9]));
        assert_eq!((done(&rx).status, 3), (Status::Ok, 3));
        s.request(Some(&mut dev), Request::Submit(Submit { id: 4, ep: 0x83, kind: XferType::Bulk, setup: [0; 8], len: 64, data: vec![], zero_packet: false }));
        let d = done(&rx);
        assert_eq!((d.id, d.data), (4, vec![7, 8, 9]));
        // Wrong type for an endpoint, endpoint of no interface.
        s.request(Some(&mut dev), Request::Submit(Submit { id: 5, ep: 0x83, kind: XferType::Interrupt, setup: [0; 8], len: 8, data: vec![], zero_packet: false }));
        assert_eq!(done(&rx).status, Status::Stall);
        s.request(Some(&mut dev), bulk(6, 0x05, 1, vec![0]));
        assert_eq!(done(&rx).status, Status::Stall);
        // SET_ADDRESS never reaches the device; bad alternate setting.
        s.request(Some(&mut dev), Request::Submit(Submit::control(7, setup(0, SET_ADDRESS, 5, 0, 0), vec![])));
        assert_eq!(done(&rx).status, Status::Stall);
        s.request(Some(&mut dev), Request::Submit(Submit::control(8, setup(1, SET_INTERFACE, 3, 1, 0), vec![])));
        assert_eq!(done(&rx).status, Status::Stall);
        // Interrupt IN waits; a duplicate id is refused; cancel ends it.
        let irq = Submit { id: 9, ep: 0x81, kind: XferType::Interrupt, setup: [0; 8], len: 8, data: vec![], zero_packet: false };
        s.request(Some(&mut dev), Request::Submit(irq.clone()));
        assert!(rx.try_recv().is_err());
        s.request(Some(&mut dev), Request::Submit(irq));
        assert_eq!(done(&rx).status, Status::Error);
        s.request(Some(&mut dev), Request::Cancel { id: 9 });
        assert_eq!(done(&rx).status, Status::Cancelled);
        // Nothing plugged in.
        s.request(None, bulk(10, 0x02, 1, vec![0]));
        assert_eq!(done(&rx).status, Status::NoDevice);
    }

    #[test]
    fn caps_transfers_in_flight() {
        let (tx, rx) = mpsc::channel();
        let mut dev = Acm::new(tx.clone(), 0x1209, 0x0001);
        let mut s = Session::new(tx);
        s.request(Some(&mut dev), Request::Submit(Submit::control(0, setup(0, SET_CONFIGURATION, 1, 0, 0), vec![])));
        done(&rx);
        for id in 1..=MAX_INFLIGHT as u32 + 1 {
            s.request(Some(&mut dev), Request::Submit(Submit { id, ep: 0x81, kind: XferType::Interrupt, setup: [0; 8], len: 8, data: vec![], zero_packet: false }));
        }
        let d = done(&rx);
        assert_eq!((d.id, d.status), (MAX_INFLIGHT as u32 + 1, Status::Error));
    }
}

#[cfg(test)]
mod iso_tests {
    use super::*;
    use crate::fake::{Audio, FakeKind, FakeOpener};
    use crate::proto::IsoPacket;

    const SET_CONFIG: [u8; 8] = [0, SET_CONFIGURATION, 1, 0, 0, 0, 0, 0];

    fn set_alt(iface: u8, alt: u8) -> [u8; 8] {
        [1, SET_INTERFACE, alt, 0, iface, 0, 0, 0]
    }

    fn iso(id: u32, ep: u8, packets: Vec<u32>) -> Request {
        let data = if ep & 0x80 != 0 { Vec::new() } else { vec![1; packets.iter().sum::<u32>() as usize] };
        Request::IsoSubmit(IsoSubmit { id, ep, packets, data })
    }

    fn iso_done(rx: &mpsc::Receiver<Event>) -> IsoDone {
        match rx.try_recv() {
            Ok(Event::IsoDone(d)) => d,
            other => panic!("expected an isochronous completion, got {other:?}"),
        }
    }

    fn done(rx: &mpsc::Receiver<Event>) -> Done {
        match rx.try_recv() {
            Ok(Event::Done(d)) => d,
            other => panic!("expected a completion, got {other:?}"),
        }
    }

    /// Samples the pretend microphone sent: they count up by one.
    fn check_ramp(data: &[u8], from: u16) -> u16 {
        let mut want = from;
        for c in data.chunks_exact(2) {
            assert_eq!(u16::from_le_bytes([c[0], c[1]]), want);
            want = want.wrapping_add(1);
        }
        want
    }

    #[test]
    fn checks_isochronous_requests() {
        let (tx, rx) = mpsc::channel();
        let mut dev = Audio::new(tx.clone(), 0x1209, 0x0002, false);
        let mut s = Session::new(tx);
        // Not configured: no such endpoint.
        s.request(Some(&mut dev), iso(1, 0x82, vec![96]));
        assert_eq!(iso_done(&rx).status, Status::Stall);
        s.request(Some(&mut dev), Request::Submit(Submit::control(2, SET_CONFIG, vec![])));
        assert_eq!(done(&rx).status, Status::Ok);
        // Alternate setting 0 has no endpoint.
        s.request(Some(&mut dev), iso(3, 0x82, vec![96]));
        assert_eq!(iso_done(&rx).status, Status::Stall);
        s.request(Some(&mut dev), Request::Submit(Submit::control(4, set_alt(2, 1), vec![])));
        assert_eq!(done(&rx).status, Status::Ok);
        s.request(Some(&mut dev), iso(5, 0x82, vec![96, 96, 40, 0]));
        let d = iso_done(&rx);
        let ok = |actual| IsoPacket { status: Status::Ok, actual };
        assert_eq!((d.status, d.packets.clone()), (Status::Ok, vec![ok(96), ok(96), ok(40), ok(0)]));
        assert_eq!(check_ramp(&d.data, 0), 116);
        // A packet larger than the endpoint moves per frame (100).
        s.request(Some(&mut dev), iso(6, 0x82, vec![96, 101]));
        assert_eq!(iso_done(&rx).status, Status::Error);
        // Isochronous dressed as a plain transfer.
        s.request(Some(&mut dev), Request::Submit(Submit { id: 7, ep: 0x82, kind: XferType::Iso, setup: [0; 8], len: 96, data: vec![], zero_packet: false }));
        assert_eq!(done(&rx).status, Status::Error);
        // Speakers: once their alternate setting is on.
        s.request(Some(&mut dev), iso(8, 0x01, vec![192; 2]));
        assert_eq!(iso_done(&rx).status, Status::Stall);
        s.request(Some(&mut dev), Request::Submit(Submit::control(9, set_alt(1, 1), vec![])));
        assert_eq!(done(&rx).status, Status::Ok);
        s.request(Some(&mut dev), iso(10, 0x01, vec![192, 200]));
        let d = iso_done(&rx);
        assert_eq!((d.packets, d.data.len()), (vec![ok(192), ok(200)], 0));
        assert_eq!(dev.played, 392);
        // Unplugged.
        s.request(None, iso(11, 0x01, vec![192]));
        assert_eq!(iso_done(&rx).status, Status::NoDevice);
    }

    /// The whole helper over its socket, the pretend sound card in real
    /// time: transfers queued ahead complete in order, one packet per
    /// millisecond, with the samples in sequence across them.
    #[test]
    fn isochronous_over_the_socket() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || serve(theirs, Box::new(FakeOpener::new(FakeKind::Audio, 0x1209, 0x0002, None))));
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut r = io::BufReader::new(ours.try_clone().unwrap());
        let mut w = ours;
        let mut next = || proto::read_event(&mut r).unwrap().expect("helper still there");
        assert!(matches!(next(), Event::Attached { speed: Speed::Full, vendor: 0x1209, product: 0x0002 }));
        for (id, setup) in [(1, SET_CONFIG), (2, set_alt(2, 1))] {
            w.write_all(&Request::Submit(Submit::control(id, setup, vec![])).encode()).unwrap();
            assert!(matches!(next(), Event::Done(Done { status: Status::Ok, .. })));
        }
        let t0 = std::time::Instant::now();
        for id in 10..13 {
            w.write_all(&iso(id, 0x82, vec![96; 10]).encode()).unwrap();
        }
        let mut ramp = 0;
        for id in 10..13 {
            let Event::IsoDone(d) = next() else { panic!("expected a completion") };
            assert_eq!((d.id, d.status, d.packets.len()), (id, Status::Ok, 10));
            ramp = check_ramp(&d.data, ramp);
        }
        assert_eq!(ramp, 3 * 10 * 48);
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(29), "30 frames took {took:?}");
        // A long one, cancelled.
        w.write_all(&iso(20, 0x82, vec![96; 100]).encode()).unwrap();
        w.write_all(&Request::Cancel { id: 20 }.encode()).unwrap();
        let Event::IsoDone(d) = next() else { panic!("expected a completion") };
        assert_eq!((d.id, d.status), (20, Status::Cancelled));
    }
}
