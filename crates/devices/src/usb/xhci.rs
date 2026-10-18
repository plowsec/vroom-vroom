//! xHCI host controller with a keyboard on port 1 and a tablet on port 2.
//! Enough for Windows setup, UEFI and Linux.
//!
//! Host devices passed through sit on `PASS_PORTS` pairs of root ports
//! (a USB 2 and a USB 3 port each), after the HID ones; the device shows
//! up on the one that fits its speed. The pairs are always there, so a
//! device can be plugged in while the VM runs (`pass_plug`) and pulled
//! out again (`pass_unplug`), with the port change events a real
//! controller sends. Their transfers are asynchronous: a TD goes to the
//! device's helper process as a request and completes when the answer
//! comes back (`usb_event`). Isochronous TDs (one packet each) go in
//! batches, several batches ahead, so the device never runs dry.
//!
//! Rings live in guest memory and the guest controls them: walks are
//! bounded, Link TRB chains are capped, and every address is checked by
//! GuestMemory. A full event ring drops events instead of overwriting.
//! What the helper sends back is checked against what was asked for.

use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;

use vroom_memory::{GuestAddress, GuestMemory};
use vroom_snapshot::{decode, encode};
use vroom_usb::proto::{Done, Event, IsoDone, IsoSubmit, Request, Speed, Status, Submit, XferType, MAX_DATA, MAX_ISO_PACKET, MAX_ISO_PACKETS};
use serde::{Deserialize, Serialize};

use super::hid::{HidEvent, Keyboard, Tablet, UsbIds};
use super::uvc::Camera;
use super::{Setup, UsbDevice};
use crate::pci::{BarSpec, ConfigSpace, Intx, MsiSender, Msix, PciDevice, PciId, VendorIds};
use crate::{get_le, put_le};

const BAR_SIZE: u64 = 0x4000;
const CAPLEN: u64 = 0x40;
const OP: u64 = CAPLEN;
const RT: u64 = 0x1000;
const DB: u64 = 0x2000;
const MSIX_TABLE: u32 = 0x3000;
const MSIX_PBA: u32 = 0x3800;
const XECP: u64 = 0x100;

const MAX_SLOTS: usize = 8;
/// Keyboard and tablet.
const HID_PORTS: usize = 2;
/// Root port pairs for host devices: the VM file's first, the others
/// free for devices plugged in while the VM runs. Fixed, so the guest
/// sees the same controller whatever is plugged in (and snapshots fit).
pub const PASS_PORTS: usize = 8;
/// Most pairs a saved VM may come back with.
const MAX_PASS_PAIRS: usize = 16;
/// Most TRBs handled per doorbell / ring walk.
const WALK_LIMIT: usize = 256;
const LINK_LIMIT: usize = 8;
const MAX_DEFERRED: usize = 256;
/// Most TRBs in one TD: emulated devices, passthrough (1 MiB in pages).
const TD_TRBS: usize = 64;
const PASS_TD_TRBS: usize = 320;
/// Passthrough transfers in flight: per endpoint, per device.
const MAX_EP_INFLIGHT: usize = 8;
const MAX_DEV_INFLIGHT: usize = 32;
/// Isochronous: one request carries at most this much bus time of TDs
/// (in 125 us microframes: 8 ms), so a few requests keep the device
/// busy without holding completions back long.
const ISO_BATCH_UFRAMES: u32 = 64;
/// A stream asked to start on a Frame ID: fine up to 895 frames ahead
/// (xHCI 4.11.2.5), or a little late (it starts as soon as it can).
/// Anything else is a missed service.
const ISO_AHEAD_FRAMES: u32 = 895;
const ISO_LATE_FRAMES: u32 = 16;
/// Helper events held while paused (completions are bounded by the
/// above; the rest is attach/detach noise).
const MAX_DEFERRED_USB: usize = 256;

// TRB types
const T_NORMAL: u32 = 1;
const T_SETUP: u32 = 2;
const T_DATA: u32 = 3;
const T_STATUS: u32 = 4;
const T_ISOCH: u32 = 5;
const T_LINK: u32 = 6;
const T_EVENT_DATA: u32 = 7;
const T_NOOP: u32 = 8;
const C_ENABLE_SLOT: u32 = 9;
const C_DISABLE_SLOT: u32 = 10;
const C_ADDRESS: u32 = 11;
const C_CONFIGURE: u32 = 12;
const C_EVALUATE: u32 = 13;
const C_RESET_EP: u32 = 14;
const C_STOP_EP: u32 = 15;
const C_SET_DEQ: u32 = 16;
const C_RESET_DEVICE: u32 = 17;
const C_NOOP: u32 = 23;
const E_TRANSFER: u32 = 32;
const E_COMMAND: u32 = 33;
const E_PORT: u32 = 34;

// Completion codes
const CC_SUCCESS: u32 = 1;
const CC_BABBLE: u32 = 3;
const CC_TRANSACTION: u32 = 4;
const CC_TRB_ERROR: u32 = 5;
const CC_STALL: u32 = 6;
const CC_NO_SLOTS: u32 = 9;
const CC_SLOT_NOT_ENABLED: u32 = 11;
const CC_EP_NOT_ENABLED: u32 = 12;
const CC_SHORT: u32 = 13;
const CC_RING_UNDERRUN: u32 = 14;
const CC_RING_OVERRUN: u32 = 15;
const CC_PARAMETER: u32 = 17;
const CC_CONTEXT_STATE: u32 = 19;
const CC_MISSED_SERVICE: u32 = 23;

// USBCMD / USBSTS
const CMD_RUN: u32 = 1;
const CMD_HCRST: u32 = 2;
const CMD_INTE: u32 = 4;
const STS_HCH: u32 = 1;
const STS_EINT: u32 = 1 << 3;
const STS_PCD: u32 = 1 << 4;

// PORTSC
const PS_CCS: u32 = 1;
const PS_PED: u32 = 1 << 1;
const PS_PR: u32 = 1 << 4;
const PS_PLS_SHIFT: u32 = 5;
const PS_PP: u32 = 1 << 9;
const PS_SPEED_SHIFT: u32 = 10;
const PS_LWS: u32 = 1 << 16;
const PS_CSC: u32 = 1 << 17;
const PS_WRC: u32 = 1 << 19;
const PS_PRC: u32 = 1 << 21;
const PS_PLC: u32 = 1 << 22;
const PS_CHANGE_MASK: u32 = 0x7f << 17;
const PS_WPR: u32 = 1 << 31;
const PLS_U0: u32 = 0;
const PLS_RXDETECT: u32 = 5;
const PLS_POLLING: u32 = 7;

// TRB control bits (dword 3).
const TRB_ED: u32 = 1 << 2; // in events: TRB pointer is Event Data
const TRB_ISP: u32 = 1 << 2;
const TRB_CH: u32 = 1 << 4;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;
const TRB_BEI: u32 = 1 << 9;
const TRB_SIA: u32 = 1 << 31;

const EP_RUNNING: u32 = 1;
const EP_HALTED: u32 = 2;
const EP_STOPPED: u32 = 3;

type Trb = [u32; 4];

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct Ring {
    deq: u64,
    cycle: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Ep {
    ring: Ring,
    state: u32,
    /// Endpoint type from its context: 4 control, 2/6 bulk out/in,
    /// 3/7 interrupt, 1/5 isochronous.
    #[serde(default)]
    kind: u8,
    /// Service interval: 2^interval x 125 us.
    #[serde(default)]
    interval: u8,
}

const EPT_ISO_OUT: u8 = 1;
const EPT_BULK_OUT: u8 = 2;
const EPT_INT_OUT: u8 = 3;
const EPT_CONTROL: u8 = 4;
const EPT_ISO_IN: u8 = 5;
const EPT_BULK_IN: u8 = 6;
const EPT_INT_IN: u8 = 7;

/// A passthrough TD the helper is working on.
#[derive(Debug)]
struct Pending {
    slot: usize,
    dci: u8,
    /// Ring position of the TD's first TRB (to stop or halt on it).
    start: Ring,
    td: Vec<(u64, Trb)>,
    is_in: bool,
    len: u32,
    /// Isochronous: where each TD (one packet) ends in `td`, and its
    /// length. Empty for the other types.
    iso: Vec<(usize, u32)>,
    done: Option<Answer>,
}

/// How a passthrough TD ended.
#[derive(Debug)]
enum Answer {
    Plain(Done),
    Iso(IsoDone),
    /// Isochronous TD the controller finished by itself (No-op, bad TRB,
    /// missed service): this completion code.
    Local(u32),
}

/// A host device behind a helper process.
#[derive(Debug)]
struct Passthrough {
    link: Sender<Request>,
    /// Attached, at this speed. Live state: never saved.
    speed: Option<Speed>,
    /// The guest gave it an address since the last reset.
    addressed: bool,
    /// Show it connected once the guest has seen the disconnect.
    reconnect: bool,
    next_id: u32,
    /// A helper is plugged into this pair.
    used: bool,
    /// Bumped on each plug and unplug: events from an earlier helper are
    /// dropped.
    gen: u64,
    pending: BTreeMap<u32, Pending>,
    /// Transfers per (slot, dci), oldest first: completed in this order.
    queues: BTreeMap<(usize, u8), VecDeque<u32>>,
}

/// A link nobody listens on: what an empty port pair has.
fn no_link() -> Sender<Request> {
    std::sync::mpsc::channel().0
}

impl Passthrough {
    fn new(link: Option<Sender<Request>>) -> Passthrough {
        Passthrough {
            used: link.is_some(),
            link: link.unwrap_or_else(no_link),
            speed: None,
            addressed: false,
            reconnect: false,
            next_id: 0,
            gen: 0,
            pending: BTreeMap::new(),
            queues: BTreeMap::new(),
        }
    }

    fn alloc_id(&mut self) -> u32 {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id != 0 && !self.pending.contains_key(&self.next_id) {
                return self.next_id;
            }
        }
    }
}

/// What is behind a root port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortKind {
    Hid,
    /// The camera (UVC), after the HID ports.
    Camera,
    /// Passthrough device, on its USB 2 or USB 3 port.
    Pass(usize, bool),
}

/// PORTSC speed ID for a host device's speed.
fn speed_id(s: Speed) -> u32 {
    match s {
        Speed::Full => 1,
        Speed::Low => 2,
        Speed::High => 3,
        Speed::Super => 4,
        Speed::SuperPlus => 5,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Slot {
    port: u8,
    ctx: u64,
    eps: BTreeMap<u8, Ep>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Intr {
    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    enq: u64,
    seg: u32,
    seg_left: u32,
    cycle: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Op {
    usbcmd: u32,
    usbsts: u32,
    dnctrl: u32,
    dcbaap: u64,
    config: u32,
}

#[derive(Serialize, Deserialize)]
struct Snap {
    cfg: ConfigSpace,
    msix: Msix,
    op: Op,
    intr: Intr,
    cmd: Ring,
    ports: Vec<u32>,
    slots: Vec<Option<Slot>>,
    kbd: Vec<u8>,
    tablet: Vec<u8>,
    #[serde(default)]
    camera: Vec<u8>,
    /// The layout had a camera port. Saves from before this field: told
    /// by the port count.
    #[serde(default)]
    camera_port: Option<bool>,
    /// Passthrough port pairs (older saves: none, see `restore`).
    #[serde(default)]
    pass_pairs: Option<u32>,
}

pub struct Xhci {
    id: String,
    cfg: ConfigSpace,
    msix: Msix,
    intx: Arc<Intx>,
    mem: GuestMemory,
    op: Op,
    intr: Intr,
    cmd: Ring,
    /// PORTSC per root port: HID ports, then a USB 2 port per
    /// passthrough device, then a USB 3 port per passthrough device.
    ports: Vec<u32>,
    slots: Vec<Option<Slot>>,
    kbd: Keyboard,
    tablet: Tablet,
    camera: Option<Camera>,
    /// A camera port in the layout. Usually "there is a camera", but a
    /// resumed VM keeps the layout it was saved with (the guest read the
    /// port count when it started): the port may be empty, or the camera
    /// portless until the next start.
    camera_port: bool,
    pass: Vec<Passthrough>,
    paused: bool,
    deferred: VecDeque<HidEvent>,
    /// (pair, its generation then, event).
    deferred_usb: VecDeque<(usize, u64, Event)>,
    /// Port changes to report once running again (after a restore).
    port_events_due: bool,
    epoch: Instant,
}

impl std::fmt::Debug for Xhci {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Xhci").field("id", &self.id).finish()
    }
}

fn trb_type(t: &Trb) -> u32 {
    (t[3] >> 10) & 0x3f
}

fn param(t: &Trb) -> u64 {
    t[0] as u64 | (t[1] as u64) << 32
}

impl Xhci {
    pub fn new(slot: u8, mem: GuestMemory, intx: Arc<Intx>, msi: Arc<dyn MsiSender>) -> Self {
        let mut cfg = ConfigSpace::new(
            PciId { vendor: 0x1b36, device: 0x000d, class: 0x0c_03_30, revision: 1, subsys_vendor: 0x1b36, subsys: 0 },
            1,
        );
        cfg.add_bar(0, BarSpec { size: BAR_SIZE, is64: true, prefetch: false });
        let msix = Msix::new(&mut cfg, 1, 0, MSIX_TABLE, MSIX_PBA, msi);
        let mut x = Xhci {
            id: format!("xhci@{slot:02x}"),
            cfg,
            msix,
            intx,
            mem,
            op: Op::default(),
            intr: Intr::default(),
            cmd: Ring::default(),
            ports: vec![0; HID_PORTS],
            slots: vec![None; MAX_SLOTS + 1],
            kbd: Keyboard::default(),
            tablet: Tablet::default(),
            camera: None,
            camera_port: false,
            pass: Vec::new(),
            paused: false,
            deferred: VecDeque::new(),
            deferred_usb: VecDeque::new(),
            port_events_due: false,
            epoch: Instant::now(),
        };
        x.hc_reset();
        x
    }

    /// Root ports for host devices behind helper processes, `PASS_PORTS`
    /// pairs (more if `links` has more). Requests for device `i` go to
    /// `links[i]`, its events come back through `usb_event(i, ..)`. The
    /// pairs past `links` are free for `pass_plug`.
    pub fn with_passthrough(mut self, links: Vec<Sender<Request>>) -> Self {
        let n = links.len().max(PASS_PORTS);
        let mut links = links.into_iter();
        self.pass = (0..n).map(|_| Passthrough::new(links.next())).collect();
        self.layout();
        self
    }

    /// A camera on a USB 2 port of its own, after the HID ports.
    pub fn with_camera(mut self, cam: Camera) -> Self {
        self.camera = Some(cam);
        self.camera_port = true;
        self.layout();
        self
    }

    /// Ports and slots for what is plugged in.
    fn layout(&mut self) {
        let (e, n) = (self.emu(), self.pass.len());
        self.ports = vec![0; e + 2 * n];
        self.slots = vec![None; MAX_SLOTS.max(e + n) + 1];
        self.hc_reset();
    }

    /// Ports of our own devices (HID, camera), before passthrough ones.
    fn emu(&self) -> usize {
        HID_PORTS + self.camera_port as usize
    }

    /// A new frame from the camera: hand it to a transfer that waits.
    pub fn camera_frame(&mut self) {
        if self.paused || self.camera.is_none() || !self.camera_port {
            return;
        }
        let port = HID_PORTS as u8 + 1;
        let ids: Vec<usize> = (1..=self.max_slots()).filter(|&i| self.slots[i].as_ref().is_some_and(|s| s.port == port)).collect();
        for id in ids {
            // EP1 IN.
            self.run_interrupt_in(id, 3);
        }
    }

    fn max_slots(&self) -> usize {
        self.slots.len() - 1
    }

    fn port_kind(&self, port: u8) -> Option<PortKind> {
        let p = (port as usize).checked_sub(1)?;
        let (e, n) = (self.emu(), self.pass.len());
        match p {
            _ if p < HID_PORTS => Some(PortKind::Hid),
            _ if p < e => Some(PortKind::Camera),
            _ if p < e + n => Some(PortKind::Pass(p - e, false)),
            _ if p < e + 2 * n => Some(PortKind::Pass(p - e - n, true)),
            _ => None,
        }
    }

    /// Index into `ports` of passthrough device `i`'s USB 2 / USB 3 port.
    fn pass_port(&self, i: usize, usb3: bool) -> usize {
        self.emu() + i + if usb3 { self.pass.len() } else { 0 }
    }

    /// PORTSC of a passthrough port with (Some) or without a device.
    fn pass_portsc(speed: Option<Speed>, usb3: bool) -> u32 {
        match speed {
            // USB 3 links train by themselves: enabled, in U0.
            Some(s) if usb3 => PS_CCS | PS_PP | PS_PED | PLS_U0 << PS_PLS_SHIFT | speed_id(s) << PS_SPEED_SHIFT,
            Some(s) => PS_CCS | PS_PP | PLS_POLLING << PS_PLS_SHIFT | speed_id(s) << PS_SPEED_SHIFT,
            None => PS_PP | PLS_RXDETECT << PS_PLS_SHIFT,
        }
    }

    /// USB names the guest sees (from the VM's `[identity]`).
    pub fn with_ids(mut self, ids: &UsbIds) -> Self {
        self.kbd = Keyboard::new(ids);
        self.tablet = Tablet::new(ids);
        self
    }

    /// The controller's own PCI IDs (from the VM's `[identity]`).
    pub fn with_pci_ids(mut self, ids: VendorIds) -> Self {
        self.cfg.set_vendor_ids(ids);
        self
    }

    fn dev(&mut self, port: u8) -> Option<&mut dyn UsbDevice> {
        match port {
            1 => Some(&mut self.kbd),
            2 => Some(&mut self.tablet),
            3 if self.camera_port => self.camera.as_mut().map(|c| c as &mut dyn UsbDevice),
            _ => None,
        }
    }

    fn hc_reset(&mut self) {
        self.op = Op { usbsts: STS_HCH, ..Default::default() };
        self.intr = Intr::default();
        self.cmd = Ring::default();
        self.slots = vec![None; self.max_slots() + 1];
        for p in 0..self.emu() {
            // Device attached, powered, waiting for a port reset.
            let speed = if p < HID_PORTS { super::SPEED_FULL } else { super::SPEED_HIGH };
            self.ports[p] = PS_CCS | PS_PP | PS_CSC | PLS_POLLING << PS_PLS_SHIFT | (speed as u32) << PS_SPEED_SHIFT;
            if p >= HID_PORTS && self.camera.is_none() {
                // An empty camera port (see `camera_port`).
                self.ports[p] = PS_PP | PLS_RXDETECT << PS_PLS_SHIFT;
            }
        }
        for i in 0..self.pass.len() {
            self.pass_forget(i);
            // A bus reset, as a real host does on reboot.
            let p = &mut self.pass[i];
            if std::mem::take(&mut p.addressed) {
                let _ = p.link.send(Request::Reset);
            }
            p.reconnect = false;
            let speed = p.speed;
            for usb3 in [false, true] {
                let here = speed.filter(|s| s.is_super() == usb3);
                let at = self.pass_port(i, usb3);
                self.ports[at] = Self::pass_portsc(here, usb3) | if here.is_some() { PS_CSC } else { 0 };
            }
        }
        self.kbd.reset();
        self.tablet.reset();
        if let Some(c) = &mut self.camera {
            c.reset();
        }
        self.intx.set(false);
    }

    // ---- interrupts / events ----

    fn update_irq(&mut self) {
        let pending = self.intr.iman & 1 != 0 && self.intr.iman & 2 != 0 && self.op.usbcmd & CMD_INTE != 0;
        if self.msix.enabled() {
            self.intx.set(false);
        } else {
            self.intx.set(pending && !self.cfg.intx_disabled());
        }
    }

    fn load_segment(&mut self) -> bool {
        if self.intr.erstsz == 0 {
            return false;
        }
        let at = GuestAddress(self.intr.erstba + 16 * self.intr.seg as u64);
        let mut e = [0u8; 16];
        if self.mem.read(at, &mut e).is_err() {
            return false;
        }
        let base = u64::from_le_bytes(e[0..8].try_into().unwrap_or([0; 8])) & !0x3f;
        let size = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) & 0xffff;
        if size < 16 || !self.mem.check_range(GuestAddress(base), size as u64 * 16) {
            return false;
        }
        self.intr.enq = base;
        self.intr.seg_left = size;
        true
    }

    fn post_event(&mut self, t: Trb) {
        self.post_event_irq(t, true);
    }

    /// Post an event; `irq` false: don't interrupt for it (a TRB with
    /// Block Event Interrupt: the driver knows a later one will).
    fn post_event_irq(&mut self, mut t: Trb, irq: bool) {
        if self.intr.erstsz == 0 || self.intr.seg_left == 0 {
            return;
        }
        // Full: the next slot would be where the driver will read.
        let next = if self.intr.seg_left > 1 { self.intr.enq + 16 } else { u64::MAX };
        if next == self.intr.erdp & !0xf {
            log::debug!("xhci: event ring full, dropping event");
            return;
        }
        t[3] = (t[3] & !1) | self.intr.cycle as u32;
        let mut b = [0u8; 16];
        for (i, w) in t.iter().enumerate() {
            b[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        if self.mem.write(GuestAddress(self.intr.enq), &b).is_err() {
            return;
        }
        self.intr.enq += 16;
        self.intr.seg_left -= 1;
        if self.intr.seg_left == 0 {
            self.intr.seg = (self.intr.seg + 1) % self.intr.erstsz;
            if self.intr.seg == 0 {
                self.intr.cycle = !self.intr.cycle;
            }
            self.load_segment();
        }
        if !irq {
            return;
        }
        self.intr.iman |= 1;
        self.op.usbsts |= STS_EINT;
        if self.intr.iman & 2 != 0 && self.op.usbcmd & CMD_INTE != 0 && self.msix.enabled() {
            self.msix.notify(0);
        }
        self.update_irq();
    }

    fn port_event(&mut self, port: usize) {
        self.op.usbsts |= STS_PCD;
        self.post_event([((port + 1) as u32) << 24, 0, CC_SUCCESS << 24, E_PORT << 10]);
    }

    fn transfer_event(&mut self, ptr: u64, code: u32, residual: u32, slot: u8, dci: u8) {
        self.transfer_event_irq(ptr, code, residual, slot, dci, true);
    }

    fn transfer_event_irq(&mut self, ptr: u64, code: u32, residual: u32, slot: u8, dci: u8, irq: bool) {
        log::debug!("xhci: transfer event slot {slot} dci {dci} ptr {ptr:#x} cc {code} residual {residual}");
        self.post_event_irq([ptr as u32, (ptr >> 32) as u32, code << 24 | (residual & 0xff_ffff), E_TRANSFER << 10 | (dci as u32) << 16 | (slot as u32) << 24], irq);
    }

    // ---- rings ----

    fn read_trb(&self, addr: u64) -> Option<Trb> {
        let mut b = [0u8; 16];
        self.mem.read(GuestAddress(addr), &mut b).ok()?;
        let w = |i: usize| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        Some([w(0), w(1), w(2), w(3)])
    }

    /// Next TRB owned by us, following Link TRBs. Returns (addr, trb).
    fn ring_peek(&self, r: &mut Ring) -> Option<(u64, Trb)> {
        for _ in 0..LINK_LIMIT {
            let t = self.read_trb(r.deq)?;
            if (t[3] & 1 == 1) != r.cycle {
                return None;
            }
            if trb_type(&t) == T_LINK {
                r.deq = param(&t) & !0xf;
                if t[3] & 2 != 0 {
                    r.cycle = !r.cycle;
                }
                continue;
            }
            return Some((r.deq, t));
        }
        None
    }

    // ---- commands ----

    fn run_commands(&mut self) {
        for _ in 0..WALK_LIMIT {
            let mut r = self.cmd;
            let Some((addr, t)) = self.ring_peek(&mut r) else {
                self.cmd = r;
                break;
            };
            r.deq = addr + 16;
            self.cmd = r;
            let (code, slot) = self.command(&t);
            log::debug!("xhci: cmd type {} slot {} -> cc {code} slot {slot}", trb_type(&t), Self::slot_id(&t));
            self.post_event([addr as u32, (addr >> 32) as u32, code << 24, E_COMMAND << 10 | (slot as u32) << 24]);
        }
    }

    fn slot_id(t: &Trb) -> usize {
        (t[3] >> 24) as usize
    }

    fn slot_mut(&mut self, id: usize) -> Option<&mut Slot> {
        if id == 0 || id > self.max_slots() {
            return None;
        }
        self.slots[id].as_mut()
    }

    fn command(&mut self, t: &Trb) -> (u32, u8) {
        let id = Self::slot_id(t);
        match trb_type(t) {
            C_ENABLE_SLOT => match (1..=self.max_slots()).find(|&i| self.slots[i].is_none()) {
                Some(i) => {
                    self.slots[i] = Some(Slot { port: 0, ctx: 0, eps: BTreeMap::new() });
                    (CC_SUCCESS, i as u8)
                }
                None => (CC_NO_SLOTS, 0),
            },
            C_DISABLE_SLOT => {
                if self.slot_mut(id).is_none() {
                    return (CC_SLOT_NOT_ENABLED, id as u8);
                }
                self.abandon_slot(id, |_| true);
                self.slots[id] = None;
                (CC_SUCCESS, id as u8)
            }
            C_ADDRESS => (self.address_device(id, param(t) & !0xf, t[3] & (1 << 9) != 0), id as u8),
            C_CONFIGURE => (self.configure(id, param(t) & !0xf, t[3] & (1 << 9) != 0), id as u8),
            C_EVALUATE => (self.evaluate(id, param(t) & !0xf), id as u8),
            C_RESET_EP | C_STOP_EP => {
                let dci = ((t[3] >> 16) & 0x1f) as u8;
                let stop = trb_type(t) == C_STOP_EP;
                (self.ep_state_cmd(id, dci, stop), id as u8)
            }
            C_SET_DEQ => {
                let dci = ((t[3] >> 16) & 0x1f) as u8;
                (self.set_deq(id, dci, param(t)), id as u8)
            }
            C_RESET_DEVICE => {
                if self.slot_mut(id).is_none() {
                    return (CC_SLOT_NOT_ENABLED, id as u8);
                }
                self.abandon_slot(id, |_| true);
                let Some(s) = self.slot_mut(id) else { return (CC_SLOT_NOT_ENABLED, id as u8) };
                s.eps.retain(|&d, _| d == 1);
                let ctx = s.ctx;
                self.write_slot_state(ctx, 1, 0);
                (CC_SUCCESS, id as u8)
            }
            C_NOOP => (CC_SUCCESS, 0),
            _ => (CC_TRB_ERROR, 0),
        }
    }

    fn ctx_read(&self, addr: u64, dwords: usize) -> Option<Vec<u32>> {
        let mut b = vec![0u8; dwords * 4];
        self.mem.read(GuestAddress(addr), &mut b).ok()?;
        Some(b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }

    fn ctx_write(&self, addr: u64, d: &[u32]) {
        let b: Vec<u8> = d.iter().flat_map(|w| w.to_le_bytes()).collect();
        let _ = self.mem.write(GuestAddress(addr), &b);
    }

    fn write_slot_state(&self, ctx: u64, state: u32, address: u32) {
        if let Some(mut s) = self.ctx_read(ctx, 8) {
            s[3] = state << 27 | address & 0xff;
            self.ctx_write(ctx, &s);
        }
    }

    fn write_ep_ctx(&self, ctx: u64, dci: u8, ep: &Ep) {
        let at = ctx + 32 * dci as u64;
        if let Some(mut e) = self.ctx_read(at, 8) {
            e[0] = (e[0] & !7) | ep.state;
            e[2] = (ep.ring.deq as u32 & !0xf) | ep.ring.cycle as u32;
            e[3] = (ep.ring.deq >> 32) as u32;
            self.ctx_write(at, &e);
        }
    }

    fn output_ctx(&self, id: usize) -> Option<u64> {
        let p: u64 = self.mem.read_obj(GuestAddress(self.op.dcbaap + 8 * id as u64)).ok()?;
        let p = u64::from_le(p) & !0x3f;
        self.mem.check_range(GuestAddress(p), 32 * 32).then_some(p)
    }

    fn address_device(&mut self, id: usize, input: u64, bsr: bool) -> u32 {
        if self.slot_mut(id).is_none() {
            return CC_SLOT_NOT_ENABLED;
        }
        let (Some(icc), Some(slot_ctx), Some(ep0)) =
            (self.ctx_read(input, 8), self.ctx_read(input + 0x20, 8), self.ctx_read(input + 0x40, 8))
        else {
            return CC_PARAMETER;
        };
        if icc[1] & 3 != 3 {
            return CC_PARAMETER;
        }
        let port = ((slot_ctx[1] >> 16) & 0xff) as u8;
        if port == 0 || port as usize > self.ports.len() {
            return CC_CONTEXT_STATE;
        }
        let Some(out) = self.output_ctx(id) else { return CC_PARAMETER };
        let ring = Ring { deq: (ep0[2] as u64 | (ep0[3] as u64) << 32) & !0xf, cycle: ep0[2] & 1 == 1 };
        let ep = Ep { ring, state: EP_RUNNING, kind: EPT_CONTROL, interval: 0 };
        let mut sc = slot_ctx.clone();
        sc[3] = if bsr { 1 << 27 } else { 2 << 27 | id as u32 };
        self.ctx_write(out, &sc);
        let mut e0 = ep0.clone();
        e0[0] = (e0[0] & !7) | EP_RUNNING;
        self.ctx_write(out + 0x20, &e0);
        self.abandon_slot(id, |_| true);
        match self.port_kind(port) {
            Some(PortKind::Pass(i, _)) => self.pass[i].addressed |= !bsr,
            _ => {
                if let Some(d) = self.dev(port) {
                    d.reset();
                }
            }
        }
        let s = self.slot_mut(id).expect("checked above");
        s.port = port;
        s.ctx = out;
        s.eps.clear();
        s.eps.insert(1, ep);
        CC_SUCCESS
    }

    fn configure(&mut self, id: usize, input: u64, deconfigure: bool) -> u32 {
        let Some(s) = self.slot_mut(id) else { return CC_SLOT_NOT_ENABLED };
        let ctx = s.ctx;
        if deconfigure {
            s.eps.retain(|&d, _| d == 1);
            self.abandon_slot(id, |d| d != 1);
            self.write_slot_state(ctx, 2, id as u32);
            return CC_SUCCESS;
        }
        let Some(icc) = self.ctx_read(input, 8) else { return CC_PARAMETER };
        let (drop, add) = (icc[0], icc[1]);
        // Dropped or re-added endpoints lose what was in flight.
        self.abandon_slot(id, |d| d > 1 && (drop | add) & (1 << d) != 0);
        let mut new_eps = Vec::new();
        for dci in 2..=31u8 {
            if add & (1 << dci) != 0 {
                let Some(e) = self.ctx_read(input + 0x20 + 0x20 * dci as u64, 8) else { return CC_PARAMETER };
                let ring = Ring { deq: (e[2] as u64 | (e[3] as u64) << 32) & !0xf, cycle: e[2] & 1 == 1 };
                let mut out_e = e.clone();
                out_e[0] = (out_e[0] & !7) | EP_RUNNING;
                self.ctx_write(ctx + 0x20 * dci as u64, &out_e);
                let interval = (((e[0] >> 16) & 0xff) as u8).min(15);
                new_eps.push((dci, Ep { ring, state: EP_RUNNING, kind: ((e[1] >> 3) & 7) as u8, interval }));
            }
        }
        let s = self.slot_mut(id).expect("checked above");
        for dci in 2..=31u8 {
            if drop & (1 << dci) != 0 {
                s.eps.remove(&dci);
            }
        }
        for (d, e) in new_eps {
            s.eps.insert(d, e);
        }
        let last = s.eps.keys().max().copied().unwrap_or(1) as u32;
        if let Some(mut sc) = self.ctx_read(ctx, 8) {
            sc[0] = (sc[0] & !(0x1f << 27)) | last << 27;
            sc[3] = 3 << 27 | id as u32;
            self.ctx_write(ctx, &sc);
        }
        CC_SUCCESS
    }

    fn evaluate(&mut self, id: usize, input: u64) -> u32 {
        let Some(s) = self.slot_mut(id) else { return CC_SLOT_NOT_ENABLED };
        let ctx = s.ctx;
        let Some(icc) = self.ctx_read(input, 8) else { return CC_PARAMETER };
        if icc[1] & 2 != 0 {
            // EP0 max packet size update.
            if let (Some(ie), Some(mut oe)) = (self.ctx_read(input + 0x40, 8), self.ctx_read(ctx + 0x20, 8)) {
                oe[1] = (oe[1] & 0xffff) | (ie[1] & 0xffff_0000);
                self.ctx_write(ctx + 0x20, &oe);
            }
        }
        CC_SUCCESS
    }

    fn ep_state_cmd(&mut self, id: usize, dci: u8, stop: bool) -> u32 {
        let Some(s) = self.slot_mut(id) else { return CC_SLOT_NOT_ENABLED };
        let Some(ep) = s.eps.get(&dci) else { return CC_EP_NOT_ENABLED };
        if !stop && ep.state != EP_HALTED {
            return CC_CONTEXT_STATE;
        }
        // Stop: passthrough transfers in flight are cancelled, and the
        // ring goes back to the first of them (the driver decides what
        // to do with them, like with a TD a real controller stopped in).
        self.abandon_slot(id, |d| d == dci);
        let Some(s) = self.slot_mut(id) else { return CC_SLOT_NOT_ENABLED };
        let ctx = s.ctx;
        let Some(ep) = s.eps.get_mut(&dci) else { return CC_EP_NOT_ENABLED };
        ep.state = EP_STOPPED;
        let e = *ep;
        self.write_ep_ctx(ctx, dci, &e);
        CC_SUCCESS
    }

    fn set_deq(&mut self, id: usize, dci: u8, p: u64) -> u32 {
        let Some(s) = self.slot_mut(id) else { return CC_SLOT_NOT_ENABLED };
        let ctx = s.ctx;
        let Some(ep) = s.eps.get_mut(&dci) else { return CC_EP_NOT_ENABLED };
        if ep.state == EP_RUNNING {
            return CC_CONTEXT_STATE;
        }
        ep.ring = Ring { deq: p & !0xf, cycle: p & 1 == 1 };
        let e = *ep;
        self.write_ep_ctx(ctx, dci, &e);
        CC_SUCCESS
    }

    // ---- transfers ----

    fn doorbell_slot(&mut self, id: usize, dci: u8) {
        let Some(s) = self.slot_mut(id) else { return };
        let Some(ep) = s.eps.get_mut(&dci) else { return };
        if ep.state == EP_HALTED {
            return;
        }
        ep.state = EP_RUNNING;
        let port = s.port;
        if let Some(PortKind::Pass(i, _)) = self.port_kind(port) {
            self.run_pass(i, id, dci);
        } else if dci == 1 {
            self.run_control(id);
        } else {
            self.run_interrupt_in(id, dci);
        }
    }

    fn ep(&self, id: usize, dci: u8) -> Option<(u8, Ep)> {
        let s = self.slots.get(id)?.as_ref()?;
        Some((s.port, *s.eps.get(&dci)?))
    }

    fn set_ep(&mut self, id: usize, dci: u8, ep: Ep) {
        if let Some(Some(s)) = self.slots.get_mut(id) {
            if let Some(e) = s.eps.get_mut(&dci) {
                *e = ep;
            }
        }
    }

    /// Gather one TD from an endpoint ring: TRBs up to the first one with
    /// the chain bit clear. Control TDs also run to (at least) the Status
    /// stage. None if the driver hasn't finished queueing it.
    fn gather_td(&self, ring: Ring, control: bool, max: usize) -> Option<(Ring, Vec<(u64, Trb)>)> {
        let mut r = ring;
        let mut td = Vec::new();
        let mut status = !control;
        for _ in 0..max {
            let (addr, t) = self.ring_peek(&mut r)?;
            r.deq = addr + 16;
            td.push((addr, t));
            status |= trb_type(&t) == T_STATUS;
            if status && t[3] & TRB_CH == 0 {
                return Some((r, td));
            }
        }
        None
    }

    fn run_control(&mut self, id: usize) {
        for _ in 0..WALK_LIMIT {
            let Some((port, mut ep)) = self.ep(id, 1) else { return };
            if ep.state != EP_RUNNING {
                return;
            }
            let Some((r, td)) = self.gather_td(ep.ring, true, TD_TRBS) else { return };
            ep.ring = r;
            self.set_ep(id, 1, ep);
            self.control_td(id, port, &td);
        }
    }

    fn control_td(&mut self, id: usize, port: u8, td: &[(u64, Trb)]) {
        let Some(&(setup_addr, setup_trb)) = td.first() else { return };
        if trb_type(&setup_trb) != T_SETUP || setup_trb[3] & TRB_IDT == 0 {
            self.transfer_event(setup_addr, CC_TRB_ERROR, 0, id as u8, 1);
            return;
        }
        let setup = Setup::parse(param(&setup_trb));
        log::debug!("xhci: slot {id} port {port} control {setup:x?} ({} trbs)", td.len());
        let data: Vec<&(u64, Trb)> = td.iter().filter(|(_, t)| trb_type(t) == T_DATA || trb_type(t) == T_NORMAL).collect();
        // OUT data stage.
        let mut out = Vec::new();
        if !setup.device_to_host() {
            for (_, t) in &data {
                let len = (t[2] & 0x1ffff) as usize;
                let mut b = vec![0u8; len.min(4096)];
                if t[3] & TRB_IDT != 0 {
                    let n = 8.min(len);
                    b.truncate(n);
                    b.copy_from_slice(&param(t).to_le_bytes()[..n]);
                } else if self.mem.read(GuestAddress(param(t)), &mut b).is_err() {
                    return self.transfer_event(setup_addr, CC_TRB_ERROR, 0, id as u8, 1);
                }
                out.extend_from_slice(&b);
            }
        }
        let result = match self.dev(port) {
            Some(d) => d.control(setup, &out),
            None => Err(super::Stall),
        };
        match result {
            Ok(reply) => {
                let input = setup.device_to_host().then_some(reply.as_slice());
                self.complete_td(id, 1, td, input);
            }
            Err(_) => {
                // STALL on the data or status stage; endpoint halts.
                let at = data.last().map(|(a, _)| *a).unwrap_or(td[td.len() - 1].0);
                if let Some((_, mut ep)) = self.ep(id, 1) {
                    ep.state = EP_HALTED;
                    self.set_ep(id, 1, ep);
                    let ctx = self.slots[id].as_ref().map(|s| s.ctx).unwrap_or(0);
                    self.write_ep_ctx(ctx, 1, &ep);
                }
                self.transfer_event(at, CC_STALL, 0, id as u8, 1);
            }
        }
    }

    /// Walk a finished TD and post its events: every TRB with IOC gets
    /// one (Setup included: EDK2 waits on the first and last TRB), short
    /// data TRBs with ISP too, and Event Data TRBs report the bytes moved
    /// since the previous one (Windows relies on these). `input` is the
    /// data for an IN transfer, written into the data buffers in order.
    fn complete_td(&mut self, id: usize, dci: u8, td: &[(u64, Trb)], input: Option<&[u8]>) {
        let (slot, mut off, mut edtla, mut short) = (id as u8, 0usize, 0u32, false);
        for &(addr, t) in td {
            let ioc = t[3] & TRB_IOC != 0;
            // Block Event Interrupt (Normal, Isoch, Event Data TRBs).
            let irq = !(t[3] & TRB_BEI != 0 && matches!(trb_type(&t), T_NORMAL | T_ISOCH | T_EVENT_DATA));
            match trb_type(&t) {
                T_DATA | T_NORMAL | T_ISOCH => {
                    let len = (t[2] & 0x1ffff) as usize;
                    let n = match input {
                        Some(src) => {
                            let n = len.min(src.len().saturating_sub(off));
                            if n > 0 && self.mem.write(GuestAddress(param(&t)), &src[off..off + n]).is_err() {
                                return self.transfer_event(addr, CC_TRB_ERROR, 0, slot, dci);
                            }
                            off += n;
                            n
                        }
                        None => len,
                    };
                    edtla = edtla.wrapping_add(n as u32);
                    let residual = (len - n) as u32;
                    if residual > 0 {
                        short = true;
                        if ioc || t[3] & TRB_ISP != 0 {
                            self.transfer_event_irq(addr, CC_SHORT, residual, slot, dci, irq);
                        }
                    } else if ioc {
                        self.transfer_event_irq(addr, CC_SUCCESS, 0, slot, dci, irq);
                    }
                }
                T_EVENT_DATA => {
                    if ioc {
                        let cc = if short { CC_SHORT } else { CC_SUCCESS };
                        let p = param(&t);
                        log::debug!("xhci: event data slot {slot} dci {dci} edtla {edtla} cc {cc}");
                        self.post_event_irq([p as u32, (p >> 32) as u32, cc << 24 | (edtla & 0xff_ffff), E_TRANSFER << 10 | TRB_ED | (dci as u32) << 16 | (slot as u32) << 24], irq);
                    }
                    edtla = 0;
                    short = false;
                }
                _ => {
                    // Setup, Status, No-op.
                    if ioc {
                        self.transfer_event(addr, CC_SUCCESS, 0, slot, dci);
                    }
                }
            }
        }
    }

    fn run_interrupt_in(&mut self, id: usize, dci: u8) {
        if dci.is_multiple_of(2) {
            // OUT endpoints: consume and complete (nothing to deliver).
            return self.drain_out(id, dci);
        }
        let epnum = dci / 2;
        for _ in 0..WALK_LIMIT {
            let Some((port, mut ep)) = self.ep(id, dci) else { return };
            if ep.state != EP_RUNNING {
                return;
            }
            let Some((r, td)) = self.gather_td(ep.ring, false, TD_TRBS) else { return };
            // TDs with a data buffer wait for a report; pure No-op /
            // Event Data TDs complete right away.
            let room: usize = td.iter().filter(|(_, t)| trb_type(t) == T_NORMAL).map(|(_, t)| (t[2] & 0x1ffff) as usize).sum();
            let report = if td.iter().any(|(_, t)| trb_type(t) == T_NORMAL) {
                match self.dev(port).and_then(|d| d.poll_in_max(epnum, room)) {
                    Some(r) => r,
                    None => return,
                }
            } else {
                Vec::new()
            };
            ep.ring = r;
            self.set_ep(id, dci, ep);
            self.complete_td(id, dci, &td, Some(&report));
        }
    }

    fn drain_out(&mut self, id: usize, dci: u8) {
        for _ in 0..WALK_LIMIT {
            let Some((_, mut ep)) = self.ep(id, dci) else { return };
            let Some((r, td)) = self.gather_td(ep.ring, false, TD_TRBS) else { return };
            ep.ring = r;
            self.set_ep(id, dci, ep);
            self.complete_td(id, dci, &td, None);
        }
    }

    // ---- passthrough ----

    /// Hand TDs queued on a passthrough endpoint to the device's helper,
    /// up to a few in flight.
    fn run_pass(&mut self, i: usize, id: usize, dci: u8) {
        for _ in 0..WALK_LIMIT {
            let Some((_, mut ep)) = self.ep(id, dci) else { return };
            if ep.state != EP_RUNNING || self.pass[i].speed.is_none() {
                return;
            }
            if matches!(ep.kind, EPT_ISO_OUT | EPT_ISO_IN) {
                return self.run_pass_iso(i, id, dci);
            }
            let p = &self.pass[i];
            let queued = p.queues.get(&(id, dci)).map_or(0, |q| q.len());
            if queued >= if dci == 1 { 1 } else { MAX_EP_INFLIGHT } || p.pending.len() >= MAX_DEV_INFLIGHT {
                return;
            }
            let start = ep.ring;
            let Some((r, td)) = self.gather_td(ep.ring, dci == 1, PASS_TD_TRBS) else { return };
            ep.ring = r;
            self.set_ep(id, dci, ep);
            let mut sub = match self.pass_submit(dci, ep.kind, &td) {
                Ok(s) => s,
                Err((at, cc)) => {
                    self.transfer_event(at, cc, 0, id as u8, dci);
                    continue;
                }
            };
            let p = &mut self.pass[i];
            sub.id = p.alloc_id();
            let (rid, is_in, len) = (sub.id, sub.is_in(), sub.len);
            if p.link.send(Request::Submit(sub)).is_err() {
                // The helper is gone: the device is as good as unplugged.
                let at = td.last().map(|t| t.0).unwrap_or(0);
                self.pass_halt(id, dci, start, at, CC_TRANSACTION);
                return;
            }
            p.pending.insert(rid, Pending { slot: id, dci, start, td, is_in, len, iso: Vec::new(), done: None });
            p.queues.entry((id, dci)).or_default().push_back(rid);
        }
    }

    /// Isochronous TDs to the helper: each TD is one packet (one service
    /// interval); consecutive TDs go in one request, a few requests ahead.
    /// The host sends them as soon as it can, back to back, so a Frame ID
    /// only matters when a stream starts (nothing in flight).
    fn run_pass_iso(&mut self, i: usize, id: usize, dci: u8) {
        let is_in = dci % 2 == 1;
        for _ in 0..WALK_LIMIT {
            let Some((_, mut ep)) = self.ep(id, dci) else { return };
            if ep.state != EP_RUNNING || self.pass[i].speed.is_none() {
                return;
            }
            let p = &self.pass[i];
            let queued = p.queues.get(&(id, dci)).map_or(0, |q| q.len());
            if queued >= MAX_EP_INFLIGHT || p.pending.len() >= MAX_DEV_INFLIGHT {
                return;
            }
            let start = ep.ring;
            let budget = (ISO_BATCH_UFRAMES.checked_shr(ep.interval as u32).unwrap_or(0) as usize).clamp(1, MAX_ISO_PACKETS);
            let (mut ring, mut td, mut ends, mut out, mut total) = (ep.ring, Vec::new(), Vec::new(), Vec::new(), 0usize);
            let mut local = None;
            while ends.len() < budget {
                let Some((r, one)) = self.gather_td(ring, false, PASS_TD_TRBS) else { break };
                match self.iso_td(&one, queued == 0 && ends.is_empty(), is_in) {
                    Ok((len, data)) => {
                        if total + len as usize > MAX_DATA {
                            break;
                        }
                        let last = one.last().map_or(0, |t| t.1[3]);
                        total += len as usize;
                        out.extend_from_slice(&data);
                        td.extend_from_slice(&one);
                        ends.push((td.len(), len));
                        ring = r;
                        // The driver wants to hear about this one now: the
                        // end of its URB, likely. End the request here.
                        if last & TRB_IOC != 0 && last & TRB_BEI == 0 {
                            break;
                        }
                    }
                    Err(cc) => {
                        // Finished here, in order: alone, after the batch.
                        if ends.is_empty() {
                            local = Some((one, cc));
                            ring = r;
                        }
                        break;
                    }
                }
            }
            if ends.is_empty() && local.is_none() {
                return;
            }
            ep.ring = ring;
            self.set_ep(id, dci, ep);
            if let Some((one, cc)) = local {
                self.iso_local(i, id, dci, start, one, cc);
                continue;
            }
            let p = &mut self.pass[i];
            let rid = p.alloc_id();
            let addr = (dci / 2) | if is_in { 0x80 } else { 0 };
            let sub = IsoSubmit { id: rid, ep: addr, packets: ends.iter().map(|e| e.1).collect(), data: out };
            if p.link.send(Request::IsoSubmit(sub)).is_err() {
                let at = td.last().map_or(0, |t| t.0);
                self.pass_halt(id, dci, start, at, CC_TRANSACTION);
                return;
            }
            p.pending.insert(rid, Pending { slot: id, dci, start, td, is_in, len: total as u32, iso: ends, done: None });
            p.queues.entry((id, dci)).or_default().push_back(rid);
        }
    }

    /// One isochronous TD: its packet length and, for OUT, its data.
    /// Err: the controller finishes it itself, with that code. `starts`:
    /// the first of a stream (its Frame ID counts).
    fn iso_td(&self, td: &[(u64, Trb)], starts: bool, is_in: bool) -> Result<(u32, Vec<u8>), u32> {
        let Some(&(_, first)) = td.first() else { return Err(CC_TRB_ERROR) };
        match trb_type(&first) {
            T_ISOCH => {}
            // TDs the driver cancelled by turning them into No-ops.
            T_NOOP if td.iter().all(|(_, t)| matches!(trb_type(t), T_NOOP | T_EVENT_DATA)) => return Err(CC_SUCCESS),
            _ => return Err(CC_TRB_ERROR),
        }
        let data: Vec<&Trb> = td.iter().map(|(_, t)| t).filter(|t| matches!(trb_type(t), T_ISOCH | T_NORMAL)).collect();
        let len: u32 = data.iter().map(|t| t[2] & 0x1ffff).sum();
        if len > MAX_ISO_PACKET {
            return Err(CC_TRB_ERROR);
        }
        if starts && first[3] & TRB_SIA == 0 && self.frame_missed((first[3] >> 20) & 0x7ff) {
            return Err(CC_MISSED_SERVICE);
        }
        let mut out = Vec::new();
        if !is_in {
            for t in data {
                let n = (t[2] & 0x1ffff) as usize;
                if t[3] & TRB_IDT != 0 {
                    out.extend_from_slice(&param(t).to_le_bytes()[..n.min(8)]);
                } else {
                    let mut b = vec![0u8; n];
                    self.mem.read(GuestAddress(param(t)), &mut b).map_err(|_| CC_TRB_ERROR)?;
                    out.extend_from_slice(&b);
                }
            }
        }
        Ok((len, out))
    }

    /// A stream asked to start on Frame ID `fid`: too late, or too far
    /// ahead? (Frames are 1 ms, the MFINDEX register's upper bits.)
    fn frame_missed(&self, fid: u32) -> bool {
        let now = (self.epoch.elapsed().as_micros() / 1000) as u32 & 0x7ff;
        let ahead = fid.wrapping_sub(now) & 0x7ff;
        ahead > ISO_AHEAD_FRAMES && 0x800 - ahead > ISO_LATE_FRAMES
    }

    /// An isochronous TD the controller finishes itself: now if nothing
    /// is in flight before it, else in turn.
    fn iso_local(&mut self, i: usize, id: usize, dci: u8, start: Ring, td: Vec<(u64, Trb)>, cc: u32) {
        let p = &mut self.pass[i];
        if p.queues.get(&(id, dci)).is_none_or(|q| q.is_empty()) {
            return self.iso_finish(id, dci, &td, cc);
        }
        let rid = p.alloc_id();
        let pending = Pending { slot: id, dci, start, td, is_in: dci % 2 == 1, len: 0, iso: Vec::new(), done: Some(Answer::Local(cc)) };
        p.pending.insert(rid, pending);
        p.queues.entry((id, dci)).or_default().push_back(rid);
    }

    /// Post an isochronous TD that moved no data: a No-op TD succeeds
    /// (events for its IOC TRBs); anything else reports `cc` on its last
    /// TRB, whatever its flags say (errors always do).
    fn iso_finish(&mut self, slot: usize, dci: u8, td: &[(u64, Trb)], cc: u32) {
        if cc == CC_SUCCESS {
            return self.complete_td(slot, dci, td, None);
        }
        let Some(&(at, t)) = td.last() else { return };
        let residual = if matches!(trb_type(&t), T_ISOCH | T_NORMAL) { t[2] & 0x1ffff } else { 0 };
        self.transfer_event(at, cc, residual, slot as u8, dci);
    }

    /// A finished isochronous request: each TD gets its packet's result,
    /// in order. The answer must match what was asked (as many packets,
    /// none longer than asked, IN data adding up); else every TD fails.
    /// Isochronous endpoints never halt.
    fn iso_complete(&mut self, p: &Pending, d: IsoDone) {
        let (slot, dci) = (p.slot, p.dci);
        let fits = d.packets.len() == p.iso.len()
            && d.packets.iter().zip(&p.iso).all(|(k, t)| k.actual <= t.1)
            && d.data.len() == if p.is_in { d.actual() } else { 0 };
        let failed = d.status != Status::Ok && d.packets.is_empty() && d.data.is_empty();
        if !fits && !failed {
            log::debug!("usb: isochronous answer doesn't match the request");
        }
        let (mut from, mut off) = (0, 0usize);
        for (k, &(end, _)) in p.iso.iter().enumerate() {
            let td = &p.td[from..end.min(p.td.len())];
            from = end;
            let (status, actual) = match (fits, failed) {
                (true, _) => (d.packets[k].status, d.packets[k].actual as usize),
                (false, true) => (d.status, 0),
                (false, false) => (Status::Babble, 0),
            };
            let data = if p.is_in && fits { &d.data[off..off + actual] } else { &[][..] };
            off += if fits && p.is_in { actual } else { 0 };
            match status {
                Status::Ok => self.complete_td(slot, dci, td, p.is_in.then_some(data)),
                Status::Babble => self.iso_finish(slot, dci, td, CC_BABBLE),
                _ => self.iso_finish(slot, dci, td, CC_TRANSACTION),
            }
        }
    }

    /// An isochronous endpoint ran out of TDs with nothing in flight: a
    /// real controller says so (ring underrun for OUT, overrun for IN)
    /// and waits for the doorbell.
    fn iso_idle(&mut self, i: usize, slot: usize, dci: u8) {
        let Some((_, ep)) = self.ep(slot, dci) else { return };
        if !matches!(ep.kind, EPT_ISO_OUT | EPT_ISO_IN) || ep.state != EP_RUNNING {
            return;
        }
        let mut r = ep.ring;
        if self.pass[i].queues.get(&(slot, dci)).is_some_and(|q| !q.is_empty()) || self.ring_peek(&mut r).is_some() {
            return;
        }
        let cc = if dci % 2 == 1 { CC_RING_OVERRUN } else { CC_RING_UNDERRUN };
        self.transfer_event(0, cc, 0, slot as u8, dci);
    }

    /// The request for one TD. Err: (TRB, completion code) to fail it with.
    fn pass_submit(&self, dci: u8, kind: u8, td: &[(u64, Trb)]) -> Result<Submit, (u64, u32)> {
        let first = td.first().map(|t| t.0).unwrap_or(0);
        let trb_err = |at: u64| (at, CC_TRB_ERROR);
        let data: Vec<&(u64, Trb)> = td.iter().filter(|(_, t)| matches!(trb_type(t), T_DATA | T_NORMAL)).collect();
        let total: usize = data.iter().map(|(_, t)| (t[2] & 0x1ffff) as usize).sum();
        if total > MAX_DATA {
            return Err(trb_err(first));
        }
        let read_out = |this: &Self| -> Result<Vec<u8>, (u64, u32)> {
            let mut out = Vec::with_capacity(total);
            for (at, t) in &data {
                let len = (t[2] & 0x1ffff) as usize;
                if t[3] & TRB_IDT != 0 {
                    out.extend_from_slice(&param(t).to_le_bytes()[..len.min(8)]);
                } else {
                    let mut b = vec![0u8; len];
                    this.mem.read(GuestAddress(param(t)), &mut b).map_err(|_| trb_err(*at))?;
                    out.extend_from_slice(&b);
                }
            }
            Ok(out)
        };
        if dci == 1 {
            let Some(&(at, t)) = td.first() else { return Err(trb_err(first)) };
            if trb_type(&t) != T_SETUP || t[3] & TRB_IDT == 0 {
                return Err(trb_err(at));
            }
            let setup = param(&t).to_le_bytes();
            let wlen = u16::from_le_bytes([setup[6], setup[7]]) as usize;
            if setup[0] & 0x80 != 0 {
                return Ok(Submit::control(0, setup, Vec::new()));
            }
            let out = read_out(self)?;
            if out.len() != wlen {
                return Err(trb_err(at));
            }
            return Ok(Submit::control(0, setup, out));
        }
        let xfer = match kind {
            EPT_BULK_OUT | EPT_BULK_IN => XferType::Bulk,
            EPT_INT_OUT | EPT_INT_IN => XferType::Interrupt,
            _ => return Err(trb_err(first)),
        };
        // DCI: endpoint number * 2, plus one for IN.
        let ep = (dci / 2) | if dci % 2 == 1 { 0x80 } else { 0 };
        let mut s = Submit { id: 0, ep, kind: xfer, setup: [0; 8], len: total as u32, data: Vec::new(), zero_packet: false };
        if ep & 0x80 == 0 {
            s.data = read_out(self)?;
            // Linux ends a bulk OUT that fills its last packet with an
            // empty TRB in the same TD: the device must see a ZLP.
            s.zero_packet = xfer == XferType::Bulk && total > 0 && data.len() > 1 && data.last().is_some_and(|(_, t)| t[2] & 0x1ffff == 0);
        }
        Ok(s)
    }

    /// What a helper says: device `dev` attached, detached, or a
    /// transfer done. Held while paused.
    pub fn usb_event(&mut self, dev: usize, ev: Event) {
        if dev >= self.pass.len() {
            return;
        }
        if self.paused {
            if self.deferred_usb.len() < MAX_DEFERRED_USB {
                self.deferred_usb.push_back((dev, self.pass[dev].gen, ev));
            } else {
                log::warn!("usb: too many events while paused, dropping one");
            }
            return;
        }
        match ev {
            Event::Attached { speed, vendor, product } => {
                log::info!("usb: {vendor:04x}:{product:04x} attached ({speed:?})");
                self.pass_attach(dev, Some(speed));
            }
            Event::Detached => {
                log::info!("usb: device {dev} detached");
                self.pass_attach(dev, None);
            }
            Event::Done(d) => self.pass_done(dev, d.id, Answer::Plain(d)),
            Event::IsoDone(d) => self.pass_done(dev, d.id, Answer::Iso(d)),
        }
    }

    /// A port pair with nothing plugged in, for a device plugged in now.
    pub fn pass_free(&self) -> Option<usize> {
        // One the guest hasn't seen go yet is fine: a new device shows
        // once it has (as for a replug).
        (0..self.pass.len()).find(|&i| !self.pass[i].used && self.pass[i].speed.is_none())
    }

    /// Plug a device's helper into pair `i` (see `pass_free`). The guest
    /// sees the device once the helper says it is there (`Attached`).
    /// Returns the generation its events must carry (`pass_event`).
    pub fn pass_plug(&mut self, i: usize, link: Sender<Request>) -> Option<u64> {
        let p = self.pass.get_mut(i).filter(|p| !p.used)?;
        p.used = true;
        p.link = link;
        p.gen += 1;
        Some(p.gen)
    }

    /// An event from the helper plugged in as generation `gen`; one from
    /// a helper since unplugged is dropped.
    pub fn pass_event(&mut self, i: usize, gen: u64, ev: Event) {
        if self.pass.get(i).is_some_and(|p| p.gen == gen) {
            self.usb_event(i, ev);
        }
    }

    /// Pull the device of generation `gen` out of pair `i`: the guest
    /// sees it go (a port change event), transfers in flight end, and
    /// the pair is free again once the guest has seen that.
    pub fn pass_unplug(&mut self, i: usize, gen: u64) -> bool {
        let Some(p) = self.pass.get_mut(i).filter(|p| p.used && p.gen == gen) else { return false };
        p.gen += 1;
        p.used = false;
        let gen = p.gen;
        // Cancels go nowhere: the helper is on its way out.
        self.pass_event(i, gen, Event::Detached);
        self.pass[i].link = no_link();
        true
    }

    /// Pairs with a helper plugged in.
    pub fn pass_used(&self, i: usize) -> bool {
        self.pass.get(i).is_some_and(|p| p.used)
    }

    fn running(&self) -> bool {
        self.op.usbcmd & CMD_RUN != 0
    }

    /// Show the device on its port (or none). A device already shown
    /// goes away first; the new one appears once the driver has seen
    /// that (it clears the connect change), so a replug is never missed.
    fn pass_attach(&mut self, i: usize, speed: Option<Speed>) {
        self.pass_forget(i);
        let p = &mut self.pass[i];
        p.speed = speed;
        p.addressed = false;
        p.reconnect = false;
        let mut shown = false;
        for usb3 in [false, true] {
            let at = self.pass_port(i, usb3);
            let was = self.ports[at];
            // Shown, or its unplug not yet seen by the driver.
            if was & (PS_CCS | PS_CSC) != 0 {
                self.ports[at] = Self::pass_portsc(None, usb3) | PS_CSC;
                shown = true;
                if was & PS_CCS != 0 && self.running() {
                    self.port_event(at);
                }
            }
        }
        if shown {
            self.pass[i].reconnect = speed.is_some();
        } else {
            self.pass_connect(i);
        }
    }

    fn pass_connect(&mut self, i: usize) {
        let Some(s) = self.pass[i].speed else { return };
        self.pass[i].reconnect = false;
        let at = self.pass_port(i, s.is_super());
        self.ports[at] = Self::pass_portsc(Some(s), s.is_super()) | PS_CSC;
        if self.running() {
            self.port_event(at);
        }
    }

    fn pass_done(&mut self, i: usize, id: u32, a: Answer) {
        let Some(p) = self.pass[i].pending.get_mut(&id) else {
            // Cancelled already, or made up.
            return;
        };
        if p.done.is_some() {
            // Answered twice, or not ours to answer.
            return;
        }
        p.done = Some(a);
        let key = (p.slot, p.dci);
        let mut finished = false;
        // Complete in order: a TD finishes only after those before it.
        loop {
            let pass = &mut self.pass[i];
            let Some(&front) = pass.queues.get(&key).and_then(|q| q.front()) else { break };
            if pass.pending.get(&front).is_none_or(|p| p.done.is_none()) {
                break;
            }
            if let Some(q) = pass.queues.get_mut(&key) {
                q.pop_front();
            }
            let Some(p) = pass.pending.remove(&front) else { break };
            self.pass_complete(p);
            finished = true;
        }
        self.run_pass(i, key.0, key.1);
        if finished {
            self.iso_idle(i, key.0, key.1);
        }
    }

    fn pass_complete(&mut self, mut p: Pending) {
        let d = match p.done.take() {
            None => return,
            Some(Answer::Local(cc)) => return self.iso_finish(p.slot, p.dci, &p.td, cc),
            // An isochronous request answered as a plain one: failed.
            Some(Answer::Plain(_)) if !p.iso.is_empty() => return self.iso_complete(&p, IsoDone::failed(0, Status::Error)),
            Some(Answer::Iso(d)) if !p.iso.is_empty() => return self.iso_complete(&p, d),
            Some(Answer::Plain(d)) => d,
            // And the other way round: babble.
            Some(Answer::Iso(d)) => Done { id: d.id, status: Status::Babble, actual: 0, data: Vec::new() },
        };
        let (slot, dci) = (p.slot, p.dci);
        // The helper must answer what was asked: never more data than
        // the TD has room for, never data for an OUT transfer.
        let sane = if p.is_in { d.data.len() == d.actual as usize && d.actual <= p.len } else { d.data.is_empty() && d.actual <= p.len };
        let status = if sane { d.status } else { Status::Babble };
        let last = p.td.iter().rev().find(|(_, t)| matches!(trb_type(t), T_DATA | T_NORMAL)).or(p.td.last()).map(|t| t.0).unwrap_or(0);
        match status {
            Status::Ok => self.complete_td(slot, dci, &p.td, p.is_in.then_some(d.data.as_slice())),
            Status::Stall => self.pass_halt(slot, dci, p.start, last, CC_STALL),
            Status::Babble => self.pass_halt(slot, dci, p.start, last, CC_BABBLE),
            Status::Error | Status::Cancelled | Status::NoDevice => self.pass_halt(slot, dci, p.start, last, CC_TRANSACTION),
        }
    }

    /// The endpoint halts on a failed TD: later transfers are cancelled
    /// and the ring points back at the failed TD, as on real hardware.
    fn pass_halt(&mut self, slot: usize, dci: u8, start: Ring, at: u64, cc: u32) {
        self.abandon_slot(slot, |d| d == dci);
        if let Some((_, mut ep)) = self.ep(slot, dci) {
            ep.state = EP_HALTED;
            ep.ring = start;
            self.set_ep(slot, dci, ep);
            let ctx = self.slots[slot].as_ref().map(|s| s.ctx).unwrap_or(0);
            self.write_ep_ctx(ctx, dci, &ep);
        }
        self.transfer_event(at, cc, 0, slot as u8, dci);
    }

    /// Cancel passthrough transfers of `slot` on endpoints `which`, and
    /// rewind each ring to its first unfinished TD.
    fn abandon_slot(&mut self, slot: usize, which: impl Fn(u8) -> bool) {
        for i in 0..self.pass.len() {
            let keys: Vec<(usize, u8)> = self.pass[i].queues.keys().filter(|k| k.0 == slot && which(k.1)).copied().collect();
            for key in keys {
                let p = &mut self.pass[i];
                let ids = p.queues.remove(&key).unwrap_or_default();
                let mut first = None;
                for id in ids {
                    if let Some(x) = p.pending.remove(&id) {
                        first = first.or(Some(x.start));
                        if x.done.is_none() {
                            let _ = p.link.send(Request::Cancel { id });
                        }
                    }
                }
                if let (Some(start), Some((_, mut ep))) = (first, self.ep(key.0, key.1)) {
                    ep.ring = start;
                    self.set_ep(key.0, key.1, ep);
                }
            }
        }
    }

    /// Drop everything in flight for device `i` (reset, unplug, restore).
    fn pass_forget(&mut self, i: usize) {
        let slots: Vec<usize> = self.pass[i].queues.keys().map(|k| k.0).collect();
        for s in slots {
            self.abandon_slot(s, |_| true);
        }
    }

    /// Input from the host (display server). Queued while paused.
    pub fn input(&mut self, ev: HidEvent) {
        if self.paused {
            if self.deferred.len() < MAX_DEFERRED {
                self.deferred.push_back(ev);
            }
            return;
        }
        let port = match ev {
            HidEvent::Key { code, down } => {
                self.kbd.event(code, down);
                1
            }
            other => {
                self.tablet.event(other);
                2
            }
        };
        // Deliver to the slot bound to that port, on EP1 IN (DCI 3).
        let ids: Vec<usize> = (1..=self.max_slots()).filter(|&i| self.slots[i].as_ref().is_some_and(|s| s.port == port)).collect();
        for id in ids {
            self.run_interrupt_in(id, 3);
        }
    }

    // ---- registers ----

    fn cap_read(&self, off: u64) -> u32 {
        let n = self.pass.len() as u32;
        match off {
            0x00 => 0x0100_0000 | CAPLEN as u32,
            0x04 => (self.ports.len() as u32) << 24 | 1 << 8 | self.max_slots() as u32,
            0x08 => 0,
            0x0c => 0,
            0x10 => 1 | ((XECP / 4) as u32) << 16, // AC64, xECP
            0x14 => DB as u32,
            0x18 => RT as u32,
            0x1c => 0,
            // Supported protocols: USB 2.0 (HID ports, then one per
            // passthrough device), then USB 3.0 if there are any.
            o if o == XECP => 0x0200_0002 | if n > 0 { 4 << 8 } else { 0 },
            o if o == XECP + 4 => 0x2042_5355, // "USB "
            o if o == XECP + 8 => (self.emu() as u32 + n) << 8 | 1,
            o if o == XECP + 0x10 && n > 0 => 0x0300_0002,
            o if o == XECP + 0x14 && n > 0 => 0x2042_5355,
            o if o == XECP + 0x18 && n > 0 => n << 8 | (self.emu() as u32 + n + 1),
            _ => 0,
        }
    }

    fn op_read(&self, off: u64) -> u64 {
        match off {
            0x00 => self.op.usbcmd as u64,
            0x04 => self.op.usbsts as u64,
            0x08 => 1, // 4K pages
            0x14 => self.op.dnctrl as u64,
            0x18 => 0, // CRCR reads as 0 (except CRR, never set: we're synchronous)
            0x30 => self.op.dcbaap,
            0x34 => self.op.dcbaap >> 32,
            0x38 => self.op.config as u64,
            o if o >= 0x400 => {
                let p = ((o - 0x400) / 0x10) as usize;
                if p < self.ports.len() && (o - 0x400) % 0x10 == 0 {
                    self.ports[p] as u64
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    fn op_write(&mut self, off: u64, v: u64, len: usize) {
        match off {
            0x00 => {
                let v = v as u32;
                if v & CMD_HCRST != 0 {
                    self.hc_reset();
                    return;
                }
                let was_running = self.op.usbcmd & CMD_RUN != 0;
                self.op.usbcmd = v & 0x3f0f;
                if v & CMD_RUN != 0 && !was_running {
                    self.op.usbsts &= !STS_HCH;
                    for p in 0..self.ports.len() {
                        if self.ports[p] & PS_CSC != 0 {
                            self.port_event(p);
                        }
                    }
                } else if v & CMD_RUN == 0 {
                    self.op.usbsts |= STS_HCH;
                }
                self.update_irq();
            }
            0x04 => {
                // RW1C: EINT, PCD, HSE, SRE.
                self.op.usbsts &= !(v as u32 & (STS_EINT | STS_PCD | 1 << 2 | 1 << 10));
                self.update_irq();
            }
            0x14 => self.op.dnctrl = v as u32,
            0x18 | 0x1c => {
                let lo = off == 0x18;
                let cur = self.cmd.deq | self.cmd.cycle as u64;
                let nv = match (len, lo) {
                    (8, _) => v,
                    (_, true) => (cur & !0xffff_ffff) | (v & 0xffff_ffff),
                    (_, false) => (cur & 0xffff_ffff) | (v << 32),
                };
                if lo && v & 0x6 != 0 {
                    // Command abort/stop: nothing is ever in flight.
                    let d = self.cmd.deq;
                    self.post_event([d as u32, (d >> 32) as u32, 24 << 24, E_COMMAND << 10]);
                    return;
                }
                self.cmd = Ring { deq: nv & !0x3f, cycle: nv & 1 == 1 };
            }
            0x30 => self.op.dcbaap = if len == 8 { v & !0x3f } else { (self.op.dcbaap & !0xffff_ffff) | (v & 0xffff_ffc0) },
            0x34 => self.op.dcbaap = (self.op.dcbaap & 0xffff_ffff) | (v << 32),
            0x38 => self.op.config = v as u32 & 0xff,
            o if o >= 0x400 => {
                let p = ((o - 0x400) / 0x10) as usize;
                if p < self.ports.len() && (o - 0x400) % 0x10 == 0 {
                    self.portsc_write(p, v as u32);
                }
            }
            _ => {}
        }
    }

    fn portsc_write(&mut self, p: usize, v: u32) {
        let old = self.ports[p];
        log::debug!("xhci: portsc{p} {old:#x} <- {v:#x}");
        let mut s = old & !(v & PS_CHANGE_MASK);
        if v & PS_PED != 0 {
            s &= !PS_PED;
        }
        let mut notify = false;
        let kind = self.port_kind((p + 1) as u8);
        if v & (PS_PR | PS_WPR) != 0 && s & PS_CCS != 0 {
            // Reset completes instantly.
            s = (s & !(0xf << PS_PLS_SHIFT)) | PS_PED | PS_PRC | PLS_U0 << PS_PLS_SHIFT;
            if v & PS_WPR != 0 {
                s |= PS_WRC;
            }
            match kind {
                Some(PortKind::Pass(i, _)) => {
                    let slots: Vec<usize> =
                        (1..=self.max_slots()).filter(|&id| self.slots[id].as_ref().is_some_and(|sl| sl.port as usize == p + 1)).collect();
                    for id in slots {
                        self.abandon_slot(id, |_| true);
                    }
                    // The device itself only needs a reset once it has
                    // been set up (not on its first enumeration).
                    let pass = &mut self.pass[i];
                    if std::mem::take(&mut pass.addressed) {
                        let _ = pass.link.send(Request::Reset);
                    }
                }
                _ => {
                    if let Some(d) = self.dev((p + 1) as u8) {
                        d.reset();
                    }
                }
            }
            notify = true;
        }
        if v & PS_LWS != 0 {
            let pls = (v >> PS_PLS_SHIFT) & 0xf;
            let cur = (s >> PS_PLS_SHIFT) & 0xf;
            s = (s & !(0xf << PS_PLS_SHIFT)) | pls << PS_PLS_SHIFT;
            if cur == 3 && pls == 15 {
                // Resume: settle in U0 and report it.
                s = (s & !(0xf << PS_PLS_SHIFT)) | PLS_U0 << PS_PLS_SHIFT | PS_PLC;
                notify = true;
            }
        }
        self.ports[p] = s;
        if notify {
            self.port_event(p);
        }
        // The driver saw a passthrough device go: show the new one.
        if let Some(PortKind::Pass(i, _)) = kind {
            if v & PS_CSC != 0 && s & PS_CCS == 0 && self.pass[i].reconnect {
                self.pass_connect(i);
            }
        }
    }

    fn rt_read(&self, off: u64) -> u64 {
        match off {
            0x00 => (self.epoch.elapsed().as_micros() as u64 / 125) & 0x3fff,
            0x20 => self.intr.iman as u64,
            0x24 => self.intr.imod as u64,
            0x28 => self.intr.erstsz as u64,
            0x30 => self.intr.erstba,
            0x34 => self.intr.erstba >> 32,
            0x38 => self.intr.erdp,
            0x3c => self.intr.erdp >> 32,
            _ => 0,
        }
    }

    fn rt_write(&mut self, off: u64, v: u64, len: usize) {
        let half = |old: u64, lo: bool| -> u64 {
            match (len, lo) {
                (8, _) => v,
                (_, true) => (old & !0xffff_ffff) | (v & 0xffff_ffff),
                (_, false) => (old & 0xffff_ffff) | (v << 32),
            }
        };
        match off {
            0x20 => {
                let v = v as u32;
                self.intr.iman = (self.intr.iman & !(v & 1)) & !2 | (v & 2);
                self.update_irq();
            }
            0x24 => self.intr.imod = v as u32,
            0x28 => self.intr.erstsz = (v as u32 & 0xffff).min(16),
            0x30 | 0x34 => {
                // Drivers write low then high; reload on each write (a
                // half-written address just fails validation).
                self.intr.erstba = half(self.intr.erstba, off == 0x30) & !0x3f;
                self.intr.seg = 0;
                self.intr.cycle = true;
                self.intr.seg_left = 0;
                self.load_segment();
            }
            0x38 | 0x3c => {
                let nv = half(self.intr.erdp, off == 0x38);
                // EHB (bit 3) is RW1C; keep the pointer.
                self.intr.erdp = nv & !0xf;
            }
            _ => {}
        }
    }
}

impl PciDevice for Xhci {
    fn config(&self) -> &ConfigSpace {
        &self.cfg
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.cfg
    }

    fn config_written(&mut self, _off: usize, _len: usize) {
        self.msix.update(&self.cfg);
        self.update_irq();
    }

    fn bar_read(&mut self, _bar: usize, off: u64, data: &mut [u8]) {
        let v: u64 = match off {
            o if o < OP => self.cap_read(o) as u64,
            o if (XECP..XECP + 0x20).contains(&o) => self.cap_read(o) as u64,
            o if o < RT => self.op_read(o - OP),
            o if o < DB => self.rt_read(o - RT),
            o if o >= MSIX_TABLE as u64 && o < MSIX_PBA as u64 => {
                self.msix.table_read(o - MSIX_TABLE as u64, data);
                return;
            }
            o if o >= MSIX_PBA as u64 => {
                self.msix.pba_read(o - MSIX_PBA as u64, data);
                return;
            }
            _ => 0,
        };
        put_le(data, v);
    }

    fn bar_write(&mut self, _bar: usize, off: u64, data: &[u8]) {
        let v = get_le(data);
        match off {
            o if (XECP..XECP + 0x20).contains(&o) => {}
            o if (OP..RT).contains(&o) => self.op_write(o - OP, v, data.len()),
            o if (RT..DB).contains(&o) => self.rt_write(o - RT, v, data.len()),
            o if (DB..MSIX_TABLE as u64).contains(&o) => {
                if self.op.usbcmd & CMD_RUN == 0 {
                    return;
                }
                let slot = ((o - DB) / 4) as usize;
                let target = (v & 0xff) as u8;
                if slot == 0 {
                    self.run_commands();
                } else if (1..=31).contains(&target) {
                    self.doorbell_slot(slot, target);
                }
            }
            o if o >= MSIX_TABLE as u64 && o < MSIX_PBA as u64 => self.msix.table_write(o - MSIX_TABLE as u64, data),
            _ => {}
        }
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn pause(&mut self) {
        self.paused = true;
    }

    fn resume(&mut self) {
        self.paused = false;
        if std::mem::take(&mut self.port_events_due) && self.running() {
            for p in 0..self.ports.len() {
                if self.ports[p] & PS_CSC != 0 {
                    self.port_event(p);
                }
            }
        }
        while let Some(e) = self.deferred.pop_front() {
            self.input(e);
        }
        while let Some((i, gen, e)) = self.deferred_usb.pop_front() {
            self.pass_event(i, gen, e);
        }
    }

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        encode(&Snap {
            cfg: self.cfg.clone(),
            msix: self.msix.clone(),
            op: self.op.clone(),
            intr: self.intr.clone(),
            cmd: self.cmd,
            ports: self.ports.clone(),
            slots: self.slots.clone(),
            kbd: self.kbd.save(),
            tablet: self.tablet.save(),
            camera: self.camera.as_ref().map(|c| c.save()).unwrap_or_default(),
            camera_port: Some(self.camera_port),
            pass_pairs: Some(self.pass.len() as u32),
        })
    }

    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        let s: Snap = decode(data)?;
        let mut cfg = s.cfg;
        // The saved layout wins (the guest read the port count when its
        // driver started): the camera port, and as many passthrough port
        // pairs as there were. Older saves have one pair per device the VM
        // file had then and no count; the camera port is the odd one out.
        let camera_port = s.camera_port.unwrap_or(s.ports.len().saturating_sub(HID_PORTS) % 2 == 1);
        let emu = HID_PORTS + camera_port as usize;
        let pairs = match s.pass_pairs {
            Some(n) => n as usize,
            None => s.ports.len().saturating_sub(emu) / 2,
        };
        if !cfg.rebuild_masks(&self.cfg)
            || s.msix.vectors() != self.msix.vectors()
            || pairs > MAX_PASS_PAIRS
            || s.ports.len() != emu + 2 * pairs
            || s.slots.len() < 2
            || s.slots.len() > MAX_SLOTS.max(emu + pairs) + 1
        {
            return Err(vroom_snapshot::Error::Invalid("xhci layout".into()));
        }
        if s.slots.iter().flatten().any(|sl| sl.port as usize > s.ports.len() || sl.eps.keys().any(|&d| d == 0 || d > 31)) {
            return Err(vroom_snapshot::Error::Invalid("xhci slots".into()));
        }
        if pairs < self.pass.len() && self.pass[pairs..].iter().any(|p| p.used) {
            log::info!("usb: the VM was saved with fewer USB ports; devices past them come at its next start");
        }
        self.pass.truncate(pairs);
        while self.pass.len() < pairs {
            self.pass.push(Passthrough::new(None));
        }
        let sender = self.msix.sender();
        self.cfg = cfg;
        self.msix = s.msix;
        if let Some(sn) = sender {
            self.msix.set_sender(sn);
        }
        self.msix.update(&self.cfg);
        self.op = s.op;
        self.intr = s.intr;
        self.intr.erstsz = self.intr.erstsz.min(16);
        self.cmd = s.cmd;
        self.camera_port = camera_port;
        self.ports = s.ports;
        self.slots = s.slots;
        self.kbd.restore(&s.kbd);
        self.tablet.restore(&s.tablet);
        if let Some(c) = &mut self.camera {
            c.restore(&s.camera);
        }
        if self.camera.is_some() && !self.camera_port {
            log::info!("usb camera: added while the VM was saved; there from its next start");
        }
        if self.camera_port && self.camera.is_none() {
            // Removed while saved: the guest sees it unplugged.
            let p = HID_PORTS;
            if self.ports[p] & PS_CCS != 0 {
                self.ports[p] = PS_PP | PLS_RXDETECT << PS_PLS_SHIFT | PS_CSC;
                self.port_events_due = true;
            }
        }
        // Passthrough devices can't be saved: what the guest had is
        // gone. It sees the device leave, and come back if it is here.
        for i in 0..self.pass.len() {
            self.pass_forget(i);
            let p = &mut self.pass[i];
            p.addressed = false;
            p.reconnect = false;
            let speed = p.speed;
            let mut shown = false;
            for usb3 in [false, true] {
                let at = self.pass_port(i, usb3);
                if self.ports[at] & PS_CCS != 0 {
                    self.ports[at] = Self::pass_portsc(None, usb3) | PS_CSC;
                    shown = true;
                } else {
                    self.ports[at] = Self::pass_portsc(None, usb3) | (self.ports[at] & PS_CSC);
                }
            }
            if shown {
                self.pass[i].reconnect = speed.is_some();
            } else if let Some(sp) = speed {
                let at = self.pass_port(i, sp.is_super());
                self.ports[at] = Self::pass_portsc(Some(sp), sp.is_super()) | PS_CSC;
            }
            self.port_events_due = true;
        }
        self.update_irq();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::IntxRouter;
    use crate::NoIrq;

    struct NoMsi;
    impl MsiSender for NoMsi {
        fn send(&self, _a: u64, _d: u32) {}
    }

    #[test]
    fn a_saved_vm_keeps_its_camera_port() {
        let mem = GuestMemory::new(&[(GuestAddress(RAM), 1 << 20)]).unwrap();
        let mk = |cam: bool| {
            let r = IntxRouter::new((0..4).map(|_| Arc::new(NoIrq) as Arc<dyn crate::IrqLine>).collect());
            let x = Xhci::new(6, mem.clone(), r.pin(6, 1), Arc::new(NoMsi));
            if cam {
                x.with_camera(Camera::new(&UsbIds::default(), Arc::new(super::super::uvc::CameraFeed::default())))
            } else {
                x
            }
        };
        let (mut with, mut without) = (mk(true), mk(false));
        let saved_without = without.save().unwrap();
        // Camera turned off while saved: the port stays, empty, and the
        // guest is told it changed. (A slot on the camera port too.)
        with.slots[1] = Some(Slot { port: HID_PORTS as u8 + 1, ctx: 0, eps: BTreeMap::new() });
        let saved_with = with.save().unwrap();
        let mut x = mk(false);
        x.restore(&saved_with).unwrap();
        assert_eq!(x.ports.len(), HID_PORTS + 1);
        assert_eq!(x.ports[HID_PORTS] & (PS_CCS | PS_CSC), PS_CSC);
        assert!(x.dev(3).is_none());
        // Turned on while saved: no port until the next start.
        let mut x = mk(true);
        x.restore(&saved_without).unwrap();
        assert_eq!(x.ports.len(), HID_PORTS);
        assert!(x.dev(3).is_none());
        // Unchanged: as before.
        with.restore(&saved_with).unwrap();
        assert!(with.dev(3).is_some());
    }

    const RAM: u64 = 0x4000_0000;
    const DCBAA: u64 = RAM;
    const CMD_RING: u64 = RAM + 0x1000;
    const ERST: u64 = RAM + 0x2000;
    const EVT: u64 = RAM + 0x3000;
    const INPUT: u64 = RAM + 0x5000;
    const OUTPUT: u64 = RAM + 0x6000;
    const EP0: u64 = RAM + 0x7000;
    const BUF: u64 = RAM + 0x8000;

    struct Host {
        x: Xhci,
        mem: GuestMemory,
        evt: u64,
        cmd: u64,
    }

    impl Host {
        fn new() -> Host {
            let mem = GuestMemory::new(&[(GuestAddress(RAM), 1 << 20)]).unwrap();
            let r = IntxRouter::new((0..4).map(|_| Arc::new(NoIrq) as Arc<dyn crate::IrqLine>).collect());
            let x = Xhci::new(6, mem.clone(), r.pin(6, 1), Arc::new(NoMsi));
            Host::with(x, mem)
        }

        fn write_u32s(&self, at: u64, v: &[u32]) {
            let b: Vec<u8> = v.iter().flat_map(|w| w.to_le_bytes()).collect();
            self.mem.write(GuestAddress(at), &b).unwrap();
        }

        fn with(x: Xhci, mem: GuestMemory) -> Host {
            let mut h = Host { x, mem, evt: 0, cmd: CMD_RING };
            h.mem.write_obj(GuestAddress(ERST), &EVT.to_le()).unwrap();
            h.mem.write_obj(GuestAddress(ERST + 8), &256u32.to_le()).unwrap();
            h.mem.write_obj(GuestAddress(DCBAA + 8), &OUTPUT.to_le()).unwrap();
            h.w(RT + 0x28, 1);
            h.w(RT + 0x30, ERST);
            h.w(RT + 0x38, EVT);
            h.w(OP + 0x30, DCBAA);
            h.w(OP + 0x18, CMD_RING | 1);
            h.w(OP, (CMD_RUN | CMD_INTE) as u64);
            h
        }

        fn w(&mut self, off: u64, v: u64) {
            self.x.bar_write(0, off, &v.to_le_bytes());
        }

        fn put_trb(&self, at: u64, t: Trb) {
            let b: Vec<u8> = t.iter().flat_map(|w| w.to_le_bytes()).collect();
            self.mem.write(GuestAddress(at), &b).unwrap();
        }

        /// Events posted since the last call.
        fn events(&mut self) -> Vec<Trb> {
            let mut out = Vec::new();
            while let Some(t) = self.x.read_trb(EVT + 16 * self.evt) {
                if t[3] & 1 == 0 {
                    break;
                }
                out.push(t);
                self.evt += 1;
            }
            out
        }

        fn command(&mut self, mut t: Trb) -> Trb {
            t[3] |= 1;
            self.put_trb(self.cmd, t);
            self.cmd += 16;
            self.w(DB, 0);
            let ev = self.events();
            let c = ev.iter().find(|e| trb_type(e) == E_COMMAND).copied().expect("command completion");
            assert_eq!(c[2] >> 24, CC_SUCCESS, "command {t:x?}");
            c
        }

        /// Keyboard on port 1, addressed as slot 1.
        fn keyboard() -> Host {
            let mut h = Host::new();
            let ports = h.events();
            assert_eq!(ports.len(), HID_PORTS);
            assert!(ports.iter().all(|e| trb_type(e) == E_PORT));
            let c = h.command([0, 0, 0, C_ENABLE_SLOT << 10]);
            assert_eq!(c[3] >> 24, 1);
            let mut input = [0u32; 3 * 8];
            input[1] = 3; // add slot + ep0
            input[8 + 1] = 1 << 16; // root port 1
            input[16 + 1] = 64 << 16; // max packet
            input[16 + 2] = EP0 as u32 | 1;
            input[16 + 3] = (EP0 >> 32) as u32;
            let b: Vec<u8> = input.iter().flat_map(|w| w.to_le_bytes()).collect();
            h.mem.write(GuestAddress(INPUT), &b).unwrap();
            h.command([INPUT as u32, (INPUT >> 32) as u32, 0, C_ADDRESS << 10 | 1 << 24]);
            h
        }
    }

    #[test]
    fn control_in_with_event_data() {
        let mut h = Host::keyboard();
        // GET_DESCRIPTOR(device), 64 byte buffer: the device returns 18.
        let setup: u64 = 0x80 | 6 << 8 | 0x0100 << 16 | 64 << 48;
        let c = 1; // cycle
        h.put_trb(EP0, [setup as u32, (setup >> 32) as u32, 8, T_SETUP << 10 | TRB_IDT | 3 << 16 | c]);
        h.put_trb(EP0 + 16, [BUF as u32, 0, 64, T_DATA << 10 | 1 << 16 | TRB_CH | TRB_ISP | c]);
        h.put_trb(EP0 + 32, [0xbeef_0000, 0x12, 0, T_EVENT_DATA << 10 | TRB_IOC | c]);
        h.put_trb(EP0 + 48, [0, 0, 0, T_STATUS << 10 | TRB_IOC | c]);
        h.w(DB + 4, 1);
        let ev = h.events();
        assert_eq!(ev.len(), 3, "{ev:x?}");
        // Short data stage (ISP): residual 46.
        assert_eq!(param(&ev[0]), EP0 + 16);
        assert_eq!((ev[0][2] >> 24, ev[0][2] & 0xff_ffff), (CC_SHORT, 46));
        // Event Data: our pointer back, ED set, bytes moved.
        assert_eq!(param(&ev[1]), 0x12_beef_0000);
        assert_ne!(ev[1][3] & TRB_ED, 0);
        assert_eq!((ev[1][2] >> 24, ev[1][2] & 0xff_ffff), (CC_SHORT, 18));
        // Status stage.
        assert_eq!(param(&ev[2]), EP0 + 48);
        assert_eq!(ev[2][2] >> 24, CC_SUCCESS);
        assert!(ev.iter().all(|e| trb_type(e) == E_TRANSFER && (e[3] >> 16) & 0x1f == 1 && e[3] >> 24 == 1));
        let mut d = [0u8; 2];
        h.mem.read(GuestAddress(BUF), &mut d).unwrap();
        assert_eq!(d, [18, 1]); // bLength, DEVICE descriptor
    }

    #[test]
    fn unfinished_td_waits_for_the_driver() {
        let mut h = Host::keyboard();
        let setup: u64 = 0x80 | 6 << 8 | 0x0100 << 16 | 18 << 48;
        // Setup + Data queued, Status not yet: nothing may complete.
        h.put_trb(EP0, [setup as u32, (setup >> 32) as u32, 8, T_SETUP << 10 | TRB_IDT | 3 << 16 | 1]);
        h.put_trb(EP0 + 16, [BUF as u32, 0, 18, T_DATA << 10 | 1 << 16 | 1]);
        h.w(DB + 4, 1);
        assert!(h.events().is_empty());
        h.put_trb(EP0 + 32, [0, 0, 0, T_STATUS << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 1);
        let ev = h.events();
        assert_eq!(ev.len(), 1);
        assert_eq!(param(&ev[0]), EP0 + 32);
    }

    #[test]
    fn link_loops_are_bounded() {
        let mut h = Host::keyboard();
        // A Link TRB pointing at itself: the walk must give up.
        h.put_trb(EP0, [EP0 as u32, (EP0 >> 32) as u32, 0, T_LINK << 10 | 1]);
        h.w(DB + 4, 1);
        assert!(h.events().is_empty());
    }

    #[test]
    fn pci_ids_from_config() {
        let mem = GuestMemory::new(&[(GuestAddress(RAM), 1 << 20)]).unwrap();
        let r = IntxRouter::new((0..4).map(|_| Arc::new(NoIrq) as Arc<dyn crate::IrqLine>).collect());
        let ids = VendorIds { vendor: 0x8086, device: 0xa0ed, subsys_vendor: 0x17aa, subsys: 0x22d8 };
        let x = Xhci::new(6, mem, r.pin(6, 1), Arc::new(NoMsi)).with_pci_ids(ids);
        let mut d = [0u8; 4];
        x.config().read(0, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0xa0ed_8086);
        x.config().read(0x2c, &mut d);
        assert_eq!(u32::from_le_bytes(d), 0x22d8_17aa);
        assert_eq!(x.config().raw()[0x0b], 0x0c, "class stays USB");
    }

    /// The helper side, in-process: a fake device (the serial port unless
    /// said otherwise) behind a session.
    struct FakeHelper<D = vroom_usb::fake::Acm> {
        rx: std::sync::mpsc::Receiver<Request>,
        events: std::sync::mpsc::Receiver<Event>,
        session: vroom_usb::helper::Session,
        dev: D,
    }

    impl FakeHelper {
        fn new() -> (FakeHelper, Sender<Request>) {
            FakeHelper::with(|etx| vroom_usb::fake::Acm::new(etx, 0x1209, 0x000a))
        }
    }

    impl<D: vroom_usb::helper::Device> FakeHelper<D> {
        fn with(dev: impl FnOnce(Sender<Event>) -> D) -> (FakeHelper<D>, Sender<Request>) {
            let (tx, rx) = std::sync::mpsc::channel();
            let (etx, events) = std::sync::mpsc::channel();
            let dev = dev(etx.clone());
            (FakeHelper { rx, events, session: vroom_usb::helper::Session::new(etx), dev }, tx)
        }

        /// Run requests and hand back completions until both are quiet.
        fn pump(&mut self, x: &mut Xhci) {
            loop {
                let mut busy = false;
                while let Ok(r) = self.rx.try_recv() {
                    self.session.request(Some(&mut self.dev), r);
                    busy = true;
                }
                while let Ok(e) = self.events.try_recv() {
                    x.usb_event(0, e);
                    busy = true;
                }
                if !busy {
                    break;
                }
            }
        }
    }

    const EP1: u64 = RAM + 0x9000;
    const EP2: u64 = RAM + 0xa000;
    const EP3: u64 = RAM + 0xb000;
    const PASS_PORT: u64 = OP + 0x400 + 0x10 * HID_PORTS as u64;

    fn portsc(h: &mut Host, at: u64) -> u32 {
        let mut ps = [0u8; 4];
        h.x.bar_read(0, at, &mut ps);
        u32::from_le_bytes(ps)
    }

    /// The fake serial port attached on the first passthrough port,
    /// addressed as slot 1, configured, with bulk OUT (DCI 4), bulk IN
    /// (DCI 7) and interrupt IN (DCI 3) rings.
    fn pass_host() -> (Host, FakeHelper) {
        let mem = GuestMemory::new(&[(GuestAddress(RAM), 1 << 20)]).unwrap();
        let r = IntxRouter::new((0..4).map(|_| Arc::new(NoIrq) as Arc<dyn crate::IrqLine>).collect());
        let (mut helper, link) = FakeHelper::new();
        let x = Xhci::new(6, mem.clone(), r.pin(6, 1), Arc::new(NoMsi)).with_passthrough(vec![link]);
        let mut h = Host::with(x, mem);
        assert_eq!(h.events().len(), HID_PORTS, "only the HID ports have devices");
        h.x.usb_event(0, Event::Attached { speed: Speed::Full, vendor: 0x1209, product: 0x000a });
        let ev = h.events();
        assert_eq!((ev.len(), trb_type(&ev[0]), ev[0][0] >> 24), (1, E_PORT, HID_PORTS as u32 + 1));
        let ps = portsc(&mut h, PASS_PORT);
        assert_eq!((ps & PS_CCS, (ps >> PS_SPEED_SHIFT) & 0xf), (PS_CCS, 1));
        h.w(PASS_PORT, (PS_PP | PS_PR | PS_CSC) as u64);
        assert_eq!(trb_type(&h.events()[0]), E_PORT);
        h.command([0, 0, 0, C_ENABLE_SLOT << 10]);
        let mut input = [0u32; 3 * 8];
        input[1] = 3;
        input[8 + 1] = (HID_PORTS as u32 + 1) << 16;
        input[16 + 1] = 64 << 16 | (EPT_CONTROL as u32) << 3;
        input[16 + 2] = EP0 as u32 | 1;
        h.write_u32s(INPUT, &input);
        h.command([INPUT as u32, (INPUT >> 32) as u32, 0, C_ADDRESS << 10 | 1 << 24]);
        // SET_CONFIGURATION(1): no data stage.
        let set_cfg: u64 = 9 << 8 | 1 << 16;
        h.put_trb(EP0, [set_cfg as u32, (set_cfg >> 32) as u32, 8, T_SETUP << 10 | TRB_IDT | 1]);
        h.put_trb(EP0 + 16, [0, 0, 0, T_STATUS << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 1);
        helper.pump(&mut h.x);
        let ev = h.events();
        assert_eq!((ev.len(), ev[0][2] >> 24), (1, CC_SUCCESS), "{ev:x?}");
        let mut input = [0u32; 9 * 8];
        input[1] = 1 | 1 << 3 | 1 << 4 | 1 << 7;
        for (dci, ty, ring) in [(3usize, EPT_INT_IN, EP1), (4, EPT_BULK_OUT, EP2), (7, EPT_BULK_IN, EP3)] {
            input[8 + 8 * dci + 1] = 64 << 16 | (ty as u32) << 3;
            input[8 + 8 * dci + 2] = ring as u32 | 1;
        }
        h.write_u32s(INPUT, &input);
        h.command([INPUT as u32, (INPUT >> 32) as u32, 0, C_CONFIGURE << 10 | 1 << 24]);
        (h, helper)
    }

    #[test]
    fn passthrough_control_and_bulk() {
        let (mut h, mut helper) = pass_host();
        // GET_DESCRIPTOR(device) goes to the device behind the helper.
        let setup: u64 = 0x80 | 6 << 8 | 0x0100 << 16 | 18 << 48;
        h.put_trb(EP0 + 32, [setup as u32, (setup >> 32) as u32, 8, T_SETUP << 10 | TRB_IDT | 3 << 16 | 1]);
        h.put_trb(EP0 + 48, [BUF as u32, 0, 18, T_DATA << 10 | 1 << 16 | 1]);
        h.put_trb(EP0 + 64, [0, 0, 0, T_STATUS << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 1);
        assert!(h.events().is_empty(), "completes only when the helper answers");
        helper.pump(&mut h.x);
        assert_eq!(h.events()[0][2] >> 24, CC_SUCCESS);
        let mut d = [0u8; 18];
        h.mem.read(GuestAddress(BUF), &mut d).unwrap();
        assert_eq!((d[4], u16::from_le_bytes([d[8], d[9]])), (2, 0x1209), "CDC class, the device's vendor");
        // Bulk OUT "hello", then bulk IN 64: the loopback answers short.
        h.mem.write(GuestAddress(BUF + 0x100), b"hello").unwrap();
        h.put_trb(EP2, [(BUF + 0x100) as u32, 0, 5, T_NORMAL << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 4);
        h.put_trb(EP3, [(BUF + 0x200) as u32, 0, 64, T_NORMAL << 10 | TRB_IOC | TRB_ISP | 1]);
        h.w(DB + 4, 7);
        helper.pump(&mut h.x);
        let ev = h.events();
        assert_eq!(ev.len(), 2, "{ev:x?}");
        assert_eq!(((ev[0][3] >> 16) & 0x1f, ev[0][2] >> 24), (4, CC_SUCCESS));
        assert_eq!(((ev[1][3] >> 16) & 0x1f, ev[1][2] >> 24, ev[1][2] & 0xff_ffff), (7, CC_SHORT, 59));
        let mut b = [0u8; 5];
        h.mem.read(GuestAddress(BUF + 0x200), &mut b).unwrap();
        assert_eq!(&b, b"hello");
        // A request the device refuses stalls and halts EP0.
        let vendor: u64 = 0xc0 | 0x55 << 8 | 4 << 48;
        h.put_trb(EP0 + 80, [vendor as u32, (vendor >> 32) as u32, 8, T_SETUP << 10 | TRB_IDT | 3 << 16 | 1]);
        h.put_trb(EP0 + 96, [BUF as u32, 0, 4, T_DATA << 10 | 1 << 16 | 1]);
        h.put_trb(EP0 + 112, [0, 0, 0, T_STATUS << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 1);
        helper.pump(&mut h.x);
        let ev = h.events();
        assert_eq!((ev.len(), ev[0][2] >> 24, param(&ev[0])), (1, CC_STALL, EP0 + 96));
        assert_eq!(h.x.ep(1, 1).unwrap().1.state, EP_HALTED);
    }

    #[test]
    fn passthrough_stop_and_unplug() {
        let (mut h, mut helper) = pass_host();
        // Interrupt IN waits on the device; Stop Endpoint cancels it and
        // leaves the ring at that TD.
        h.put_trb(EP1, [BUF as u32, 0, 8, T_NORMAL << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 3);
        helper.pump(&mut h.x);
        assert!(h.events().is_empty());
        assert_eq!(h.x.pass[0].pending.len(), 1);
        h.command([0, 0, 0, C_STOP_EP << 10 | 3 << 16 | 1 << 24]);
        assert!(h.x.pass[0].pending.is_empty());
        let mut e = [0u8; 16];
        h.mem.read(GuestAddress(OUTPUT + 0x20 * 3), &mut e).unwrap();
        assert_eq!(u64::from_le_bytes(e[8..16].try_into().unwrap()) & !0xf, EP1);
        // The late "cancelled" answer is ignored.
        helper.pump(&mut h.x);
        assert!(h.events().is_empty());
        // Unplugged: the port says so.
        h.x.usb_event(0, Event::Detached);
        let ev = h.events();
        assert_eq!((ev.len(), trb_type(&ev[0])), (1, E_PORT));
        assert_eq!(portsc(&mut h, PASS_PORT) & (PS_CCS | PS_CSC), PS_CSC);
        // Back again: it shows up once the driver acknowledged the unplug.
        h.x.usb_event(0, Event::Attached { speed: Speed::High, vendor: 0x1209, product: 0x000a });
        assert!(h.events().is_empty());
        h.w(PASS_PORT, (PS_PP | PS_CSC) as u64);
        assert_eq!(trb_type(&h.events()[0]), E_PORT);
        let ps = portsc(&mut h, PASS_PORT);
        assert_eq!((ps & PS_CCS, (ps >> PS_SPEED_SHIFT) & 0xf), (PS_CCS, 3));
        // A SuperSpeed device goes on the USB 3 port.
        h.x.usb_event(0, Event::Detached);
        h.w(PASS_PORT, (PS_PP | PS_CSC) as u64);
        h.x.usb_event(0, Event::Attached { speed: Speed::Super, vendor: 0x1209, product: 0x000a });
        let usb3 = PASS_PORT + 0x10 * PASS_PORTS as u64;
        let ps = portsc(&mut h, usb3);
        assert_eq!((ps & (PS_CCS | PS_PED), (ps >> PS_SPEED_SHIFT) & 0xf), (PS_CCS | PS_PED, 4));
    }

    #[test]
    fn passthrough_restore_replugs() {
        let (mut h, _helper) = pass_host();
        h.x.pause();
        let snap = h.x.save().unwrap();
        h.x.restore(&snap).unwrap();
        h.x.resume();
        assert_eq!(portsc(&mut h, PASS_PORT) & (PS_CCS | PS_CSC), PS_CSC, "the old device left");
        assert!(h.events().iter().any(|e| trb_type(e) == E_PORT));
        h.w(PASS_PORT, (PS_PP | PS_CSC) as u64);
        assert_eq!(portsc(&mut h, PASS_PORT) & (PS_CCS | PS_CSC), PS_CCS | PS_CSC, "and is back");
    }

    #[test]
    fn passthrough_answers_are_checked() {
        let (mut h, mut helper) = pass_host();
        h.put_trb(EP3, [(BUF + 0x200) as u32, 0, 4, T_NORMAL << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 7);
        helper.pump(&mut h.x);
        let id = *h.x.pass[0].pending.keys().next().unwrap();
        // More data than asked for: babble, nothing past the buffer.
        h.mem.write(GuestAddress(BUF + 0x204), &[0xaa; 4]).unwrap();
        h.x.usb_event(0, Event::Done(Done { id, status: Status::Ok, actual: 8, data: vec![1; 8] }));
        let ev = h.events();
        assert_eq!(ev[0][2] >> 24, CC_BABBLE);
        let mut b = [0u8; 4];
        h.mem.read(GuestAddress(BUF + 0x204), &mut b).unwrap();
        assert_eq!(b, [0xaa; 4]);
        // Unknown ids and other devices are ignored.
        h.x.usb_event(0, Event::Done(Done { id: 999, status: Status::Ok, actual: 0, data: vec![] }));
        h.x.usb_event(5, Event::Detached);
        h.x.usb_event(99, Event::Detached);
        assert!(h.events().is_empty());
    }

    fn bare_host() -> Host {
        let mem = GuestMemory::new(&[(GuestAddress(RAM), 1 << 20)]).unwrap();
        let r = IntxRouter::new((0..4).map(|_| Arc::new(NoIrq) as Arc<dyn crate::IrqLine>).collect());
        let x = Xhci::new(6, mem.clone(), r.pin(6, 1), Arc::new(NoMsi)).with_passthrough(Vec::new());
        let mut h = Host::with(x, mem);
        assert_eq!(h.events().len(), HID_PORTS);
        h
    }

    #[test]
    fn the_port_pairs_are_always_there() {
        let mut h = bare_host();
        let mut d = [0u8; 4];
        h.x.bar_read(0, 0x04, &mut d);
        assert_eq!(u32::from_le_bytes(d) >> 24, (HID_PORTS + 2 * PASS_PORTS) as u32);
        // USB 2: the HID ports and one per pair; USB 3: one per pair.
        h.x.bar_read(0, XECP + 8, &mut d);
        assert_eq!(u32::from_le_bytes(d), ((HID_PORTS + PASS_PORTS) as u32) << 8 | 1);
        h.x.bar_read(0, XECP + 0x18, &mut d);
        assert_eq!(u32::from_le_bytes(d), (PASS_PORTS as u32) << 8 | (HID_PORTS + PASS_PORTS + 1) as u32);
        assert_eq!(portsc(&mut h, PASS_PORT) & PS_CCS, 0);
    }

    #[test]
    fn plug_and_unplug_while_running() {
        let mut h = bare_host();
        let i = h.x.pass_free().unwrap();
        assert_eq!(i, 0);
        let (mut helper, link) = FakeHelper::new();
        let gen = h.x.pass_plug(i, link).unwrap();
        assert!(h.x.pass_plug(i, no_link()).is_none(), "taken");
        assert_eq!(h.x.pass_free(), Some(1));
        assert!(h.events().is_empty(), "nothing shows before the helper says so");
        h.x.pass_event(i, gen, Event::Attached { speed: Speed::High, vendor: 0x1209, product: 0x000a });
        let ev = h.events();
        assert_eq!((ev.len(), trb_type(&ev[0]), ev[0][0] >> 24), (1, E_PORT, HID_PORTS as u32 + 1));
        let ps = portsc(&mut h, PASS_PORT);
        assert_eq!((ps & (PS_CCS | PS_CSC), (ps >> PS_SPEED_SHIFT) & 0xf), (PS_CCS | PS_CSC, 3));
        // The guest resets the port and talks to the device.
        h.w(PASS_PORT, (PS_PP | PS_PR | PS_CSC) as u64);
        assert_eq!(trb_type(&h.events()[0]), E_PORT);
        h.command([0, 0, 0, C_ENABLE_SLOT << 10]);
        let mut input = [0u32; 3 * 8];
        input[1] = 3;
        input[8 + 1] = (HID_PORTS as u32 + 1) << 16;
        input[16 + 1] = 64 << 16 | (EPT_CONTROL as u32) << 3;
        input[16 + 2] = EP0 as u32 | 1;
        h.write_u32s(INPUT, &input);
        h.command([INPUT as u32, (INPUT >> 32) as u32, 0, C_ADDRESS << 10 | 1 << 24]);
        let setup: u64 = 0x80 | 6 << 8 | 0x0100 << 16 | 18 << 48;
        h.put_trb(EP0, [setup as u32, (setup >> 32) as u32, 8, T_SETUP << 10 | TRB_IDT | 3 << 16 | 1]);
        h.put_trb(EP0 + 16, [BUF as u32, 0, 18, T_DATA << 10 | 1 << 16 | 1]);
        h.put_trb(EP0 + 32, [0, 0, 0, T_STATUS << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 1);
        let pump = |h: &mut Host, helper: &mut FakeHelper| loop {
            let mut busy = false;
            while let Ok(r) = helper.rx.try_recv() {
                helper.session.request(Some(&mut helper.dev), r);
                busy = true;
            }
            while let Ok(e) = helper.events.try_recv() {
                h.x.pass_event(i, gen, e);
                busy = true;
            }
            if !busy {
                break;
            }
        };
        pump(&mut h, &mut helper);
        assert_eq!(h.events()[0][2] >> 24, CC_SUCCESS);
        // A transfer in flight, then the device is pulled out.
        h.put_trb(EP0 + 48, [setup as u32, (setup >> 32) as u32, 8, T_SETUP << 10 | TRB_IDT | 3 << 16 | 1]);
        h.put_trb(EP0 + 64, [BUF as u32, 0, 18, T_DATA << 10 | 1 << 16 | 1]);
        h.put_trb(EP0 + 80, [0, 0, 0, T_STATUS << 10 | TRB_IOC | 1]);
        h.w(DB + 4, 1);
        assert_eq!(h.x.pass[i].pending.len(), 1);
        assert!(h.x.pass_unplug(i, gen));
        assert!(h.x.pass[i].pending.is_empty());
        let ev = h.events();
        assert_eq!((ev.len(), trb_type(&ev[0]), ev[0][0] >> 24), (1, E_PORT, HID_PORTS as u32 + 1));
        assert_eq!(portsc(&mut h, PASS_PORT) & (PS_CCS | PS_CSC), PS_CSC);
        // What the old helper still says goes nowhere.
        pump(&mut h, &mut helper);
        h.x.pass_event(i, gen, Event::Attached { speed: Speed::High, vendor: 0x1209, product: 0x000a });
        assert!(h.events().is_empty());
        assert!(!h.x.pass_unplug(i, gen), "once");
        assert_eq!(h.x.pass_free(), Some(0));
        // Another device on the same pair: it shows once the guest has
        // seen the last one go.
        let (_other, link) = FakeHelper::new();
        let g2 = h.x.pass_plug(0, link).unwrap();
        h.x.pass_event(0, g2, Event::Attached { speed: Speed::Full, vendor: 0x1209, product: 0x000b });
        assert!(h.events().is_empty());
        h.w(PASS_PORT, (PS_PP | PS_CSC) as u64);
        assert_eq!(trb_type(&h.events()[0]), E_PORT);
        assert_eq!(portsc(&mut h, PASS_PORT) & (PS_CCS | PS_CSC), PS_CCS | PS_CSC);
    }

    #[test]
    fn unplug_while_paused_shows_on_resume() {
        let (mut h, _helper) = pass_host();
        let gen = h.x.pass_plug(1, no_link()).unwrap();
        h.x.pass_event(1, gen, Event::Attached { speed: Speed::Super, vendor: 1, product: 2 });
        let usb3 = PASS_PORT + 0x10 * (PASS_PORTS as u64 + 1);
        assert_eq!(portsc(&mut h, usb3) & PS_CCS, PS_CCS);
        h.events();
        h.x.pause();
        assert!(h.x.pass_unplug(1, gen));
        assert_eq!(portsc(&mut h, usb3) & PS_CCS, PS_CCS, "nothing moves while paused");
        assert_ne!(h.x.pass_free(), Some(1));
        h.x.resume();
        assert_eq!(trb_type(&h.events()[0]), E_PORT);
        assert_eq!(portsc(&mut h, usb3) & (PS_CCS | PS_CSC), PS_CSC);
    }

    #[test]
    fn a_saved_vm_keeps_its_usb_port_pairs() {
        let (mut h, _helper) = pass_host();
        h.x.pause();
        let mut snap: Snap = decode(&h.x.save().unwrap()).unwrap();
        assert_eq!(snap.pass_pairs, Some(PASS_PORTS as u32));
        // A save from before the spare pairs: one pair, as a VM with one
        // [[usb]] device had (USB 2 port 3, USB 3 port 4), and no count.
        let ports = snap.ports.clone();
        snap.ports = vec![ports[0], ports[1], ports[HID_PORTS], ports[HID_PORTS + PASS_PORTS]];
        snap.pass_pairs = None;
        snap.slots.truncate(MAX_SLOTS + 1);
        snap.slots[1].as_mut().unwrap().port = 4;
        h.x.restore(&encode(&snap).unwrap()).unwrap();
        assert_eq!((h.x.ports.len(), h.x.pass.len()), (HID_PORTS + 2, 1), "the layout it was saved with");
        assert_eq!(h.x.slots[1].as_ref().unwrap().port, 4);
        assert_eq!(h.x.pass_free(), None, "no spare pair until the next start");
        h.x.resume();
        assert_eq!(portsc(&mut h, PASS_PORT) & (PS_CCS | PS_CSC), PS_CSC, "the device left");
        // And back to the spare pairs from a save that had them.
        h.x.pause();
        let back: Snap = decode(&h.x.save().unwrap()).unwrap();
        assert_eq!(back.pass_pairs, Some(1));
        let mut x8 = snap.clone_with_pairs(PASS_PORTS);
        x8.slots.truncate(MAX_SLOTS + 1);
        h.x.restore(&encode(&x8).unwrap()).unwrap();
        assert_eq!(h.x.pass.len(), PASS_PORTS);
        assert!(h.x.pass_free().is_some());
        // Counts that don't add up, or too many pairs: refused.
        let mut bad = x8.clone_with_pairs(3);
        bad.ports.push(0);
        assert!(h.x.restore(&encode(&bad).unwrap()).is_err());
        assert!(h.x.restore(&encode(&x8.clone_with_pairs(MAX_PASS_PAIRS + 1)).unwrap()).is_err());
    }

    impl Snap {
        /// This save with `n` empty passthrough pairs.
        fn clone_with_pairs(&self, n: usize) -> Snap {
            let mut s: Snap = decode(&encode(self).unwrap()).unwrap();
            let emu = HID_PORTS + s.camera_port.unwrap_or(false) as usize;
            s.ports.truncate(emu);
            s.ports.extend(std::iter::repeat_n(PS_PP, 2 * n));
            s.pass_pairs = Some(n as u32);
            for sl in s.slots.iter_mut() {
                if sl.as_ref().is_some_and(|sl| sl.port as usize > emu) {
                    *sl = None;
                }
            }
            s
        }
    }

    // ---- isochronous, with the pretend sound card ----

    use vroom_usb::fake::Audio;
    use vroom_usb::proto::IsoPacket;

    const ISO_OUT_RING: u64 = EP1;
    const ISO_IN_RING: u64 = EP2;
    const IN_BUF: u64 = RAM + 0xc000;
    const OUT_BUF: u64 = RAM + 0xd000;
    /// Speakers: endpoint 1 OUT. Microphone: endpoint 2 IN.
    const DCI_OUT: u8 = 2;
    const DCI_IN: u8 = 5;

    fn isoch(buf: u64, len: u32, flags: u32) -> Trb {
        [buf as u32, (buf >> 32) as u32, len, T_ISOCH << 10 | flags | 1]
    }

    fn codes(ev: &[Trb]) -> Vec<u32> {
        ev.iter().map(|e| e[2] >> 24).collect()
    }

    /// The pretend sound card on the first passthrough port, slot 1,
    /// both streaming interfaces on, its two isochronous endpoints
    /// configured (1 ms interval).
    fn audio_host() -> (Host, FakeHelper<Audio>) {
        let mem = GuestMemory::new(&[(GuestAddress(RAM), 1 << 20)]).unwrap();
        let r = IntxRouter::new((0..4).map(|_| Arc::new(NoIrq) as Arc<dyn crate::IrqLine>).collect());
        let (mut helper, link) = FakeHelper::with(|etx| Audio::new(etx, 0x1209, 0x0002, false));
        let x = Xhci::new(6, mem.clone(), r.pin(6, 1), Arc::new(NoMsi)).with_passthrough(vec![link]);
        let mut h = Host::with(x, mem);
        h.events();
        h.x.usb_event(0, Event::Attached { speed: Speed::Full, vendor: 0x1209, product: 0x0002 });
        h.w(PASS_PORT, (PS_PP | PS_PR | PS_CSC) as u64);
        h.events();
        h.command([0, 0, 0, C_ENABLE_SLOT << 10]);
        let mut input = [0u32; 3 * 8];
        input[1] = 3;
        input[8 + 1] = (HID_PORTS as u32 + 1) << 16;
        input[16 + 1] = 64 << 16 | (EPT_CONTROL as u32) << 3;
        input[16 + 2] = EP0 as u32 | 1;
        h.write_u32s(INPUT, &input);
        h.command([INPUT as u32, (INPUT >> 32) as u32, 0, C_ADDRESS << 10 | 1 << 24]);
        // SET_CONFIGURATION(1), SET_INTERFACE(1, 1), SET_INTERFACE(2, 1).
        let set_alt = |iface: u64| 0x01 | 11 << 8 | 1 << 16 | iface << 32;
        for (k, setup) in [9u64 << 8 | 1 << 16, set_alt(1), set_alt(2)].into_iter().enumerate() {
            let at = EP0 + 32 * k as u64;
            h.put_trb(at, [setup as u32, (setup >> 32) as u32, 8, T_SETUP << 10 | TRB_IDT | 1]);
            h.put_trb(at + 16, [0, 0, 0, T_STATUS << 10 | TRB_IOC | 1]);
            h.w(DB + 4, 1);
            helper.pump(&mut h.x);
            assert_eq!(codes(&h.events()), vec![CC_SUCCESS]);
        }
        let mut input = [0u32; 7 * 8];
        input[1] = 1 | 1 << DCI_OUT | 1 << DCI_IN;
        for (dci, ty, ring, mp) in [(DCI_OUT, EPT_ISO_OUT, ISO_OUT_RING, 200u32), (DCI_IN, EPT_ISO_IN, ISO_IN_RING, 100)] {
            let at = 8 + 8 * dci as usize;
            input[at] = 3 << 16; // 2^3 x 125 us
            input[at + 1] = mp << 16 | (ty as u32) << 3;
            input[at + 2] = ring as u32 | 1;
        }
        h.write_u32s(INPUT, &input);
        h.command([INPUT as u32, (INPUT >> 32) as u32, 0, C_CONFIGURE << 10 | 1 << 24]);
        (h, helper)
    }

    #[test]
    fn passthrough_isochronous_in() {
        let (mut h, mut helper) = audio_host();
        // One URB of 8 packets, as Linux queues them: IOC on each TD, BEI
        // on all but the last, ISP. The device sends 96 bytes a packet.
        for k in 0..8u64 {
            let bei = if k < 7 { TRB_BEI } else { 0 };
            h.put_trb(ISO_IN_RING + 16 * k, isoch(IN_BUF + 100 * k, 100, TRB_SIA | TRB_IOC | TRB_ISP | bei));
        }
        h.w(DB + 4, DCI_IN as u64);
        assert!(h.events().is_empty());
        assert_eq!(h.x.pass[0].pending.len(), 1, "one request for the URB");
        helper.pump(&mut h.x);
        let ev = h.events();
        assert_eq!(ev.len(), 9, "{ev:x?}");
        for (k, e) in ev[..8].iter().enumerate() {
            assert_eq!((param(e), e[2] >> 24, e[2] & 0xff_ffff), (ISO_IN_RING + 16 * k as u64, CC_SHORT, 4));
        }
        // The ring ran dry: overrun (IN), no TRB.
        assert_eq!((param(&ev[8]), ev[8][2] >> 24, (ev[8][3] >> 16) & 0x1f), (0, CC_RING_OVERRUN, DCI_IN as u32));
        // The samples count up, packet after packet.
        let mut b = [0u8; 800];
        h.mem.read(GuestAddress(IN_BUF), &mut b).unwrap();
        for k in 0..8 {
            for s in 0..48 {
                assert_eq!(u16::from_le_bytes([b[100 * k + 2 * s], b[100 * k + 2 * s + 1]]), (48 * k + s) as u16);
            }
        }
        assert_eq!(h.x.ep(1, DCI_IN).unwrap().1.state, EP_RUNNING);
    }

    #[test]
    fn passthrough_isochronous_out_queues_ahead() {
        let (mut h, mut helper) = audio_host();
        // 12 URBs of 2 packets: a request each, 8 in flight, the rest
        // wait on the ring and go as the first ones finish.
        for k in 0..24u64 {
            h.mem.write(GuestAddress(OUT_BUF + 192 * k), &[k as u8; 192]).unwrap();
            let ioc = if k % 2 == 1 { TRB_IOC } else { 0 };
            h.put_trb(ISO_OUT_RING + 16 * k, isoch(OUT_BUF + 192 * k, 192, TRB_SIA | ioc));
        }
        h.w(DB + 4, DCI_OUT as u64);
        assert_eq!(h.x.pass[0].queues[&(1, DCI_OUT)].len(), MAX_EP_INFLIGHT);
        helper.pump(&mut h.x);
        let ev = h.events();
        assert_eq!(ev.len(), 13, "{ev:x?}");
        for (k, e) in ev[..12].iter().enumerate() {
            assert_eq!((param(e), e[2] >> 24), (ISO_OUT_RING + 16 * (2 * k as u64 + 1), CC_SUCCESS));
        }
        assert_eq!((param(&ev[12]), ev[12][2] >> 24), (0, CC_RING_UNDERRUN));
        assert_eq!((helper.dev.played, helper.dev.played_sum), (24 * 192, 192 * (0..24).sum::<u64>()));
    }

    #[test]
    fn passthrough_isochronous_frames_and_noops() {
        let (mut h, mut helper) = audio_host();
        let now = (h.x.epoch.elapsed().as_millis() as u32) & 0x7ff;
        let fid = |f: u32| (f & 0x7ff) << 20;
        // A stream asked to start 1000 frames from now: missed service,
        // nothing sent. A No-op TD (one the driver cancelled): success.
        h.put_trb(ISO_IN_RING, isoch(IN_BUF, 100, fid(now + 1000) | TRB_IOC));
        h.put_trb(ISO_IN_RING + 16, [0, 0, 0, T_NOOP << 10 | TRB_IOC | 1]);
        // Two frames from now: fine. The next one's frame ID no longer
        // counts: the stream runs.
        h.put_trb(ISO_IN_RING + 32, isoch(IN_BUF, 100, fid(now + 2) | TRB_IOC));
        h.put_trb(ISO_IN_RING + 48, isoch(IN_BUF + 100, 100, fid(now + 1500) | TRB_IOC));
        // Not starting with an Isoch TRB: TRB error, after the others.
        h.put_trb(ISO_IN_RING + 64, [IN_BUF as u32, 0, 100, T_NORMAL << 10 | TRB_IOC | 1]);
        h.w(DB + 4, DCI_IN as u64);
        let ev = h.events();
        assert_eq!(codes(&ev), vec![CC_MISSED_SERVICE, CC_SUCCESS]);
        assert_eq!((param(&ev[0]), ev[0][2] & 0xff_ffff, param(&ev[1])), (ISO_IN_RING, 100, ISO_IN_RING + 16));
        assert_eq!(h.x.pass[0].pending.len(), 3);
        helper.pump(&mut h.x);
        let ev = h.events();
        assert_eq!(codes(&ev), vec![CC_SHORT, CC_SHORT, CC_TRB_ERROR, CC_RING_OVERRUN]);
        assert_eq!((param(&ev[0]), param(&ev[1]), param(&ev[2])), (ISO_IN_RING + 32, ISO_IN_RING + 48, ISO_IN_RING + 64));
    }

    #[test]
    fn passthrough_isochronous_answers_are_checked() {
        let (mut h, _helper) = audio_host();
        let ok = |actual| IsoPacket { status: Status::Ok, actual };
        let submit = |h: &mut Host, at: u64, buf: u64| {
            h.put_trb(at, isoch(buf, 100, TRB_SIA | TRB_IOC | TRB_BEI));
            h.put_trb(at + 16, isoch(buf + 100, 100, TRB_SIA | TRB_IOC));
            h.w(DB + 4, DCI_IN as u64);
            *h.x.pass[0].pending.keys().next().unwrap()
        };
        h.mem.write(GuestAddress(IN_BUF), &[0xaa; 400]).unwrap();
        // A packet longer than asked: every TD babbles, nothing written,
        // the endpoint keeps running.
        let id = submit(&mut h, ISO_IN_RING, IN_BUF);
        h.x.usb_event(0, Event::IsoDone(IsoDone { id, status: Status::Ok, packets: vec![ok(101), ok(0)], data: vec![1; 101] }));
        assert_eq!(codes(&h.events()), vec![CC_BABBLE, CC_BABBLE, CC_RING_OVERRUN]);
        let mut b = [0u8; 200];
        h.mem.read(GuestAddress(IN_BUF), &mut b).unwrap();
        assert_eq!(b, [0xaa; 200]);
        assert_eq!(h.x.ep(1, DCI_IN).unwrap().1.state, EP_RUNNING);
        // A packet that failed, one short: each says so.
        let id = submit(&mut h, ISO_IN_RING + 32, IN_BUF + 200);
        let err = IsoPacket { status: Status::Error, actual: 0 };
        h.x.usb_event(0, Event::IsoDone(IsoDone { id, status: Status::Ok, packets: vec![err, ok(10)], data: vec![7; 10] }));
        // A second answer to the same request is ignored.
        h.x.usb_event(0, Event::IsoDone(IsoDone { id, status: Status::Ok, packets: vec![ok(1), ok(1)], data: vec![1, 1] }));
        let ev = h.events();
        assert_eq!(codes(&ev), vec![CC_TRANSACTION, CC_SHORT, CC_RING_OVERRUN]);
        assert_eq!((ev[0][2] & 0xff_ffff, ev[1][2] & 0xff_ffff), (100, 90));
        h.mem.read(GuestAddress(IN_BUF + 300), &mut b[..11]).unwrap();
        assert_eq!(b[..11], [7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 0xaa]);
        // Answered as a plain transfer: the TDs fail.
        let id = submit(&mut h, ISO_IN_RING + 64, IN_BUF);
        h.x.usb_event(0, Event::Done(Done { id, status: Status::Ok, actual: 0, data: vec![] }));
        assert_eq!(codes(&h.events()), vec![CC_TRANSACTION, CC_TRANSACTION, CC_RING_OVERRUN]);
    }

    #[test]
    fn passthrough_isochronous_stop() {
        let (mut h, mut helper) = audio_host();
        for k in 0..20u64 {
            h.put_trb(ISO_IN_RING + 16 * k, isoch(IN_BUF + 100 * k, 100, TRB_SIA | TRB_IOC));
        }
        h.w(DB + 4, DCI_IN as u64);
        assert_eq!(h.x.pass[0].queues[&(1, DCI_IN)].len(), MAX_EP_INFLIGHT);
        // Stop: what is in flight is cancelled, the ring is back at the
        // first TD.
        h.command([0, 0, 0, C_STOP_EP << 10 | (DCI_IN as u32) << 16 | 1 << 24]);
        assert!(h.x.pass[0].pending.is_empty());
        let mut e = [0u8; 16];
        h.mem.read(GuestAddress(OUTPUT + 0x20 * DCI_IN as u64), &mut e).unwrap();
        assert_eq!(u64::from_le_bytes(e[8..16].try_into().unwrap()) & !0xf, ISO_IN_RING);
        // The helper's late answers are ignored.
        helper.pump(&mut h.x);
        assert!(h.events().is_empty());
        // The doorbell starts it again from there.
        h.w(DB + 4, DCI_IN as u64);
        helper.pump(&mut h.x);
        let ev = h.events();
        assert_eq!(ev.len(), 21);
        assert!(ev[..20].iter().enumerate().all(|(k, e)| param(e) == ISO_IN_RING + 16 * k as u64 && e[2] >> 24 == CC_SHORT));
        assert_eq!(ev[20][2] >> 24, CC_RING_OVERRUN);
    }
}
