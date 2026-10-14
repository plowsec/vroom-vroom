//! xHCI host controller (USB 2 root ports only) with a keyboard on port 1
//! and a tablet on port 2. Enough for Windows setup, UEFI and Linux.
//!
//! Rings live in guest memory and the guest controls them: walks are
//! bounded, Link TRB chains are capped, and every address is checked by
//! GuestMemory. A full event ring drops events instead of overwriting.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use bulwark_memory::{GuestAddress, GuestMemory};
use bulwark_snapshot::{decode, encode};
use serde::{Deserialize, Serialize};

use super::hid::{HidEvent, Keyboard, Tablet};
use super::{Setup, UsbDevice};
use crate::pci::{BarSpec, ConfigSpace, Intx, MsiSender, Msix, PciDevice, PciId};
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
const NPORTS: usize = 2;
/// Most TRBs handled per doorbell / ring walk.
const WALK_LIMIT: usize = 256;
const LINK_LIMIT: usize = 8;
const MAX_DEFERRED: usize = 256;

// TRB types
const T_NORMAL: u32 = 1;
const T_SETUP: u32 = 2;
const T_DATA: u32 = 3;
const T_STATUS: u32 = 4;
const T_LINK: u32 = 6;
const T_EVENT_DATA: u32 = 7;
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
const CC_TRB_ERROR: u32 = 5;
const CC_STALL: u32 = 6;
const CC_NO_SLOTS: u32 = 9;
const CC_SLOT_NOT_ENABLED: u32 = 11;
const CC_EP_NOT_ENABLED: u32 = 12;
const CC_SHORT: u32 = 13;
const CC_PARAMETER: u32 = 17;
const CC_CONTEXT_STATE: u32 = 19;

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
const PS_PRC: u32 = 1 << 21;
const PS_PLC: u32 = 1 << 22;
const PS_CHANGE_MASK: u32 = 0x7f << 17;
const PLS_U0: u32 = 0;
const PLS_POLLING: u32 = 7;

// TRB control bits (dword 3).
const TRB_ED: u32 = 1 << 2; // in events: TRB pointer is Event Data
const TRB_ISP: u32 = 1 << 2;
const TRB_CH: u32 = 1 << 4;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;

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
    ports: [u32; NPORTS],
    slots: Vec<Option<Slot>>,
    kbd: Vec<u8>,
    tablet: Vec<u8>,
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
    ports: [u32; NPORTS],
    slots: Vec<Option<Slot>>,
    kbd: Keyboard,
    tablet: Tablet,
    paused: bool,
    deferred: VecDeque<HidEvent>,
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
            ports: [0; NPORTS],
            slots: vec![None; MAX_SLOTS + 1],
            kbd: Keyboard::default(),
            tablet: Tablet::default(),
            paused: false,
            deferred: VecDeque::new(),
            epoch: Instant::now(),
        };
        x.hc_reset();
        x
    }

    fn dev(&mut self, port: u8) -> Option<&mut dyn UsbDevice> {
        match port {
            1 => Some(&mut self.kbd),
            2 => Some(&mut self.tablet),
            _ => None,
        }
    }

    fn hc_reset(&mut self) {
        self.op = Op { usbsts: STS_HCH, ..Default::default() };
        self.intr = Intr::default();
        self.cmd = Ring::default();
        self.slots = vec![None; MAX_SLOTS + 1];
        for p in 0..NPORTS {
            // Device attached, powered, waiting for a port reset.
            self.ports[p] = PS_CCS | PS_PP | PS_CSC | PLS_POLLING << PS_PLS_SHIFT | (super::SPEED_FULL as u32) << PS_SPEED_SHIFT;
        }
        self.kbd.reset();
        self.tablet.reset();
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

    fn post_event(&mut self, mut t: Trb) {
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
        log::debug!("xhci: transfer event slot {slot} dci {dci} ptr {ptr:#x} cc {code} residual {residual}");
        self.post_event([ptr as u32, (ptr >> 32) as u32, code << 24 | (residual & 0xff_ffff), E_TRANSFER << 10 | (dci as u32) << 16 | (slot as u32) << 24]);
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
        if id == 0 || id > MAX_SLOTS {
            return None;
        }
        self.slots[id].as_mut()
    }

    fn command(&mut self, t: &Trb) -> (u32, u8) {
        let id = Self::slot_id(t);
        match trb_type(t) {
            C_ENABLE_SLOT => match (1..=MAX_SLOTS).find(|&i| self.slots[i].is_none()) {
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
        if port == 0 || port as usize > NPORTS {
            return CC_CONTEXT_STATE;
        }
        let Some(out) = self.output_ctx(id) else { return CC_PARAMETER };
        let ring = Ring { deq: (ep0[2] as u64 | (ep0[3] as u64) << 32) & !0xf, cycle: ep0[2] & 1 == 1 };
        let ep = Ep { ring, state: EP_RUNNING };
        let mut sc = slot_ctx.clone();
        sc[3] = if bsr { 1 << 27 } else { 2 << 27 | id as u32 };
        self.ctx_write(out, &sc);
        let mut e0 = ep0.clone();
        e0[0] = (e0[0] & !7) | EP_RUNNING;
        self.ctx_write(out + 0x20, &e0);
        if let Some(d) = self.dev(port) {
            d.reset();
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
            self.write_slot_state(ctx, 2, id as u32);
            return CC_SUCCESS;
        }
        let Some(icc) = self.ctx_read(input, 8) else { return CC_PARAMETER };
        let (drop, add) = (icc[0], icc[1]);
        let mut new_eps = Vec::new();
        for dci in 2..=31u8 {
            if add & (1 << dci) != 0 {
                let Some(e) = self.ctx_read(input + 0x20 + 0x20 * dci as u64, 8) else { return CC_PARAMETER };
                let ring = Ring { deq: (e[2] as u64 | (e[3] as u64) << 32) & !0xf, cycle: e[2] & 1 == 1 };
                let mut out_e = e.clone();
                out_e[0] = (out_e[0] & !7) | EP_RUNNING;
                self.ctx_write(ctx + 0x20 * dci as u64, &out_e);
                new_eps.push((dci, Ep { ring, state: EP_RUNNING }));
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
        let ctx = s.ctx;
        let Some(ep) = s.eps.get_mut(&dci) else { return CC_EP_NOT_ENABLED };
        if !stop && ep.state != EP_HALTED {
            return CC_CONTEXT_STATE;
        }
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
        if dci == 1 {
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
    fn gather_td(&self, ring: Ring, control: bool) -> Option<(Ring, Vec<(u64, Trb)>)> {
        let mut r = ring;
        let mut td = Vec::new();
        let mut status = !control;
        for _ in 0..64 {
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
            let Some((r, td)) = self.gather_td(ep.ring, true) else { return };
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
            match trb_type(&t) {
                T_DATA | T_NORMAL => {
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
                            self.transfer_event(addr, CC_SHORT, residual, slot, dci);
                        }
                    } else if ioc {
                        self.transfer_event(addr, CC_SUCCESS, 0, slot, dci);
                    }
                }
                T_EVENT_DATA => {
                    if ioc {
                        let cc = if short { CC_SHORT } else { CC_SUCCESS };
                        let p = param(&t);
                        log::debug!("xhci: event data slot {slot} dci {dci} edtla {edtla} cc {cc}");
                        self.post_event([p as u32, (p >> 32) as u32, cc << 24 | (edtla & 0xff_ffff), E_TRANSFER << 10 | TRB_ED | (dci as u32) << 16 | (slot as u32) << 24]);
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
            let Some((r, td)) = self.gather_td(ep.ring, false) else { return };
            // TDs with a data buffer wait for a report; pure No-op /
            // Event Data TDs complete right away.
            let report = if td.iter().any(|(_, t)| trb_type(t) == T_NORMAL) {
                match self.dev(port).and_then(|d| d.poll_in(epnum)) {
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
            let Some((r, td)) = self.gather_td(ep.ring, false) else { return };
            ep.ring = r;
            self.set_ep(id, dci, ep);
            self.complete_td(id, dci, &td, None);
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
        let ids: Vec<usize> = (1..=MAX_SLOTS).filter(|&i| self.slots[i].as_ref().is_some_and(|s| s.port == port)).collect();
        for id in ids {
            self.run_interrupt_in(id, 3);
        }
    }

    // ---- registers ----

    fn cap_read(&self, off: u64) -> u32 {
        match off {
            0x00 => 0x0100_0000 | CAPLEN as u32,
            0x04 => (NPORTS as u32) << 24 | 1 << 8 | MAX_SLOTS as u32,
            0x08 => 0,
            0x0c => 0,
            0x10 => 1 | ((XECP / 4) as u32) << 16, // AC64, xECP
            0x14 => DB as u32,
            0x18 => RT as u32,
            0x1c => 0,
            o if o == XECP => 0x0200_0002,       // supported protocol: USB 2.0
            o if o == XECP + 4 => 0x2042_5355,   // "USB "
            o if o == XECP + 8 => (NPORTS as u32) << 8 | 1,
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
                if p < NPORTS && (o - 0x400) % 0x10 == 0 {
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
                    for p in 0..NPORTS {
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
                if p < NPORTS && (o - 0x400) % 0x10 == 0 {
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
        if v & PS_PR != 0 && s & PS_CCS != 0 {
            // Reset completes instantly.
            s = (s & !(0xf << PS_PLS_SHIFT)) | PS_PED | PS_PRC | PLS_U0 << PS_PLS_SHIFT;
            if let Some(d) = self.dev((p + 1) as u8) {
                d.reset();
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
            o if (XECP..XECP + 0x10).contains(&o) => self.cap_read(o) as u64,
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
            o if (XECP..XECP + 0x10).contains(&o) => {}
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
        while let Some(e) = self.deferred.pop_front() {
            self.input(e);
        }
    }

    fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>> {
        encode(&Snap {
            cfg: self.cfg.clone(),
            msix: self.msix.clone(),
            op: self.op.clone(),
            intr: self.intr.clone(),
            cmd: self.cmd,
            ports: self.ports,
            slots: self.slots.clone(),
            kbd: self.kbd.save(),
            tablet: self.tablet.save(),
        })
    }

    fn restore(&mut self, data: &[u8]) -> bulwark_snapshot::Result<()> {
        let s: Snap = decode(data)?;
        let mut cfg = s.cfg;
        if !cfg.rebuild_masks(&self.cfg) || s.msix.vectors() != self.msix.vectors() || s.slots.len() != MAX_SLOTS + 1 {
            return Err(bulwark_snapshot::Error::Invalid("xhci layout".into()));
        }
        if s.slots.iter().flatten().any(|sl| sl.port as usize > NPORTS || sl.eps.keys().any(|&d| d == 0 || d > 31)) {
            return Err(bulwark_snapshot::Error::Invalid("xhci slots".into()));
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
        self.ports = s.ports;
        self.slots = s.slots;
        self.kbd.restore(&s.kbd);
        self.tablet.restore(&s.tablet);
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
            let r = IntxRouter::new(vec![Arc::new(NoIrq); 4]);
            let x = Xhci::new(6, mem.clone(), r.pin(6, 1), Arc::new(NoMsi));
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
            assert_eq!(ports.len(), NPORTS);
            assert!(ports.iter().all(|e| trb_type(e) == E_PORT));
            let c = h.command([0, 0, 0, C_ENABLE_SLOT << 10]);
            assert_eq!(c[3] >> 24, 1);
            let mut input = vec![0u32; 3 * 8];
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
}
