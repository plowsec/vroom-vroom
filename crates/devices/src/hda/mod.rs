//! Intel High Definition Audio controller (PCI class 0403): the sound card
//! Windows and Linux (`snd-hda-intel`) drive with a built-in driver. One
//! input stream (microphone), one output stream, one codec ([`codec`]).
//!
//! The guest controls most of what happens here, and every piece is
//! checked: register accesses of any width and alignment; CORB/RIRB bases
//! and pointers (masked to the ring size, each CORB walked at most once per
//! doorbell); buffer descriptor lists (index masked to 256 entries, base
//! 128-byte aligned, an empty or unreachable entry stops the stream with a
//! descriptor error); DMA amounts (capped per tick); formats (we only play
//! and record what we offer). Guest memory only goes through GuestMemory.
//!
//! Streams move at real-time speed, from a clock started when they run:
//! a timer thread ([`spawn_timer`]) calls [`Hda::tick`].

pub mod codec;

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vroom_memory::{GuestAddress, GuestMemory};
use vroom_snapshot::{decode, encode};

use crate::audio::{AudioSink, AudioSource};
use crate::pci::{BarSpec, ConfigSpace, Intx, Msi, MsiSender, PciDevice, PciId, VendorIds};
use codec::{apply_gains, gains, Codec, CodecIds, CodecState};

const BAR_SIZE: u64 = 0x4000;
const N_IN: usize = 1;
const N_OUT: usize = 1;
const N_SD: usize = N_IN + N_OUT;
const SD_BASE: u64 = 0x80;
const SD_SIZE: u64 = 0x20;
/// Aliases of the wall clock and of each stream's LPIB.
const ALIAS: u64 = 0x2000;

const GCTL_CRST: u32 = 1;
const CORBCTL_RUN: u8 = 2;
const CORBCTL_MEIE: u8 = 1;
const RIRBCTL_INT: u8 = 1;
const RIRBCTL_DMA: u8 = 2;
const RIRBCTL_OIC: u8 = 4;
const RIRBSTS_INT: u8 = 1;
const RIRBSTS_OIS: u8 = 4;
const ICIS_BUSY: u16 = 1;
const ICIS_VALID: u16 = 2;
const INT_GLOBAL: u32 = 1 << 31;
const INT_CTRL: u32 = 1 << 30;

const SD_SRST: u32 = 1;
const SD_RUN: u32 = 1 << 1;
const SD_IOCE: u32 = 1 << 2;
const SD_FEIE: u32 = 1 << 3;
const SD_DEIE: u32 = 1 << 4;
const SD_CTL_MASK: u32 = 0xff_001f;
const STS_BCIS: u8 = 1 << 2;
const STS_FIFOE: u8 = 1 << 3;
const STS_DESE: u8 = 1 << 4;
const STS_FIFORDY: u8 = 1 << 5;
const FIFO_SIZE: u32 = 0xc0;

/// At most this much audio is moved per stream per tick; after a stall
/// (host asleep, VM paused) the stream skips ahead instead of bursting.
const MAX_TICK_BYTES: u64 = 256 << 10;
const MAX_LAG_DIV: u64 = 5; // 1/5 s
/// Silence queued ahead of the first output data, so host-side jitter
/// doesn't make the player run dry.
const OUTPUT_LEAD_MS: u64 = 30;
const TICK_RUNNING: Duration = Duration::from_millis(5);
const TICK_IDLE: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Regs {
    gctl: u32,
    wakeen: u16,
    statests: u16,
    gsts: u16,
    intctl: u32,
    ssync: u32,
    corb_lbase: u32,
    corb_ubase: u32,
    corb_wp: u16,
    corb_rp: u16,
    corb_rp_rst: bool,
    corb_ctl: u8,
    corb_sts: u8,
    corb_size: u8,
    rirb_lbase: u32,
    rirb_ubase: u32,
    rirb_wp: u16,
    rintcnt: u16,
    rirb_ctl: u8,
    rirb_sts: u8,
    rirb_size: u8,
    /// Responses since the guest last cleared the response interrupt.
    rirb_count: u16,
    icoi: u32,
    icii: u32,
    icis: u16,
    dpl: u32,
    dpu: u32,
}

/// A stream descriptor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Sd {
    ctl: u32,
    sts: u8,
    lpib: u32,
    cbl: u32,
    lvi: u16,
    fifow: u16,
    fmt: u16,
    bdpl: u32,
    bdpu: u32,
    /// Current buffer descriptor and the offset in it.
    bde: u16,
    bdo: u32,
    /// Real-time pacing while running; not part of snapshots.
    #[serde(skip)]
    clock: Option<Clock>,
}

#[derive(Debug, Clone, Copy)]
struct Clock {
    t0: Instant,
    frames: u64,
}

impl Sd {
    fn running(&self) -> bool {
        self.ctl & SD_RUN != 0 && self.ctl & SD_SRST == 0
    }
    fn tag(&self) -> u8 {
        (self.ctl >> 20) as u8 & 0xf
    }
    fn bdl_base(&self) -> u64 {
        (self.bdpu as u64) << 32 | (self.bdpl & !0x7f) as u64
    }
    /// Interrupt condition, as the stream's enables say.
    fn int(&self) -> bool {
        (self.sts & STS_BCIS != 0 && self.ctl & SD_IOCE != 0)
            || (self.sts & STS_FIFOE != 0 && self.ctl & SD_FEIE != 0)
            || (self.sts & STS_DESE != 0 && self.ctl & SD_DEIE != 0)
    }
}

/// Stream format register: rate, channels, bytes per sample.
fn fmt_info(fmt: u16) -> Option<(u32, usize, usize)> {
    if fmt & 0x8000 != 0 {
        return None; // non-PCM
    }
    let base = if fmt & 0x4000 != 0 { 44100 } else { 48000 };
    let (mult, div) = (((fmt >> 11) & 7) as u32, ((fmt >> 8) & 7) as u32);
    if mult > 3 {
        return None;
    }
    let bytes = match (fmt >> 4) & 7 {
        0 => 1,
        1 => 2,
        2..=4 => 4,
        _ => return None,
    };
    Some((base * (mult + 1) / (div + 1), (fmt & 0xf) as usize + 1, bytes))
}

/// Which register a byte offset belongs to: (start, size).
fn reg_at(off: u64) -> Option<(u64, u64)> {
    const GLOBAL: &[(u64, u64)] = &[
        (0x00, 2),
        (0x02, 1),
        (0x03, 1),
        (0x04, 2),
        (0x06, 2),
        (0x08, 4),
        (0x0c, 2),
        (0x0e, 2),
        (0x10, 2),
        (0x12, 2),
        (0x18, 2),
        (0x1a, 2),
        (0x20, 4),
        (0x24, 4),
        (0x30, 4),
        (0x34, 4),
        (0x38, 4),
        (0x40, 4),
        (0x44, 4),
        (0x48, 2),
        (0x4a, 2),
        (0x4c, 1),
        (0x4d, 1),
        (0x4e, 1),
        (0x50, 4),
        (0x54, 4),
        (0x58, 2),
        (0x5a, 2),
        (0x5c, 1),
        (0x5d, 1),
        (0x5e, 1),
        (0x60, 4),
        (0x64, 4),
        (0x68, 2),
        (0x70, 4),
        (0x74, 4),
    ];
    let sd_end = SD_BASE + N_SD as u64 * SD_SIZE;
    if (SD_BASE..sd_end).contains(&off) || (ALIAS + SD_BASE..ALIAS + sd_end).contains(&off) {
        let alias = off >= ALIAS;
        let rel = off - if alias { ALIAS } else { 0 };
        let base = SD_BASE + (rel - SD_BASE) / SD_SIZE * SD_SIZE;
        let (s, n) = match rel - base {
            0..=2 => (0, 3),
            3 => (3, 1),
            4..=7 => (4, 4),
            8..=11 => (8, 4),
            12..=13 => (12, 2),
            14..=15 => (14, 2),
            16..=17 => (16, 2),
            18..=19 => (18, 2),
            24..=27 => (24, 4),
            28..=31 => (28, 4),
            _ => return None,
        };
        // Only LPIB is aliased.
        if alias && s != 4 {
            return None;
        }
        return Some((base + s + if alias { ALIAS } else { 0 }, n));
    }
    if (ALIAS + 0x30..ALIAS + 0x34).contains(&off) {
        return Some((ALIAS + 0x30, 4));
    }
    GLOBAL.iter().copied().find(|&(s, n)| off >= s && off < s + n)
}

/// Ring entries for a CORB/RIRB size code.
fn ring_entries(size: u8) -> u16 {
    match size & 3 {
        0 => 2,
        1 => 16,
        _ => 256,
    }
}

#[derive(Serialize, Deserialize)]
struct Snap {
    cfg: ConfigSpace,
    regs: Regs,
    sds: Vec<Sd>,
    codec: CodecState,
}

pub struct Hda {
    id: String,
    cfg: ConfigSpace,
    msi: Msi,
    intx: Arc<Intx>,
    mem: GuestMemory,
    regs: Regs,
    sds: [Sd; N_SD],
    codec: Codec,
    sink: Box<dyn AudioSink>,
    source: Box<dyn AudioSource>,
    /// Host side started for each stream (sink / source).
    open: [bool; N_SD],
    irq_level: bool,
    paused: bool,
    born: Instant,
}

impl std::fmt::Debug for Hda {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hda").field("id", &self.id).finish_non_exhaustive()
    }
}

/// Names and numbers the guest sees.
#[derive(Debug, Clone, Copy)]
pub struct HdaIds {
    pub pci: VendorIds,
    pub codec: CodecIds,
}

impl Default for HdaIds {
    /// QEMU's intel-hda (ICH6) with hda-duplex.
    fn default() -> Self {
        HdaIds {
            pci: VendorIds { vendor: 0x8086, device: 0x2668, subsys_vendor: 0x1af4, subsys: 0x1100 },
            codec: CodecIds::default(),
        }
    }
}

impl Hda {
    /// `mic`: the codec has a microphone (the VM may record).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        slot: u8,
        mem: GuestMemory,
        intx: Arc<Intx>,
        msi: Arc<dyn MsiSender>,
        ids: HdaIds,
        mic: bool,
        sink: Box<dyn AudioSink>,
        source: Box<dyn AudioSource>,
    ) -> Self {
        let p = ids.pci;
        let mut cfg = ConfigSpace::new(
            PciId {
                vendor: p.vendor,
                device: p.device,
                class: 0x04_03_00,
                revision: 1,
                subsys_vendor: p.subsys_vendor,
                subsys: p.subsys,
            },
            1,
        );
        cfg.add_bar(0, BarSpec { size: BAR_SIZE, is64: true, prefetch: false });
        let msi = Msi::new(&mut cfg, msi);
        Hda {
            id: format!("hda@{slot:02x}"),
            cfg,
            msi,
            intx,
            mem,
            regs: Regs::default(),
            sds: Default::default(),
            codec: Codec::new(ids.codec, mic),
            sink,
            source,
            open: [false; N_SD],
            irq_level: false,
            paused: false,
            born: Instant::now(),
        }
    }

    fn is_out(i: usize) -> bool {
        i >= N_IN
    }

    fn in_reset(&self) -> bool {
        self.regs.gctl & GCTL_CRST == 0
    }

    /// Controller reset: everything but PCI config space and GCTL.
    fn reset(&mut self) {
        for i in 0..N_SD {
            self.sds[i] = Sd::default();
            self.sync_host(i);
        }
        let gctl = self.regs.gctl;
        self.regs = Regs { gctl, ..Regs::default() };
        self.codec.st = CodecState::default();
        self.update_irq(false);
    }

    fn intsts(&self) -> u32 {
        let r = &self.regs;
        let mut v = 0;
        for (i, sd) in self.sds.iter().enumerate() {
            if sd.int() {
                v |= 1 << i;
            }
        }
        let cis = (r.rirb_sts & RIRBSTS_INT != 0 && r.rirb_ctl & RIRBCTL_INT != 0)
            || (r.rirb_sts & RIRBSTS_OIS != 0 && r.rirb_ctl & RIRBCTL_OIC != 0)
            || (r.corb_sts & 1 != 0 && r.corb_ctl & CORBCTL_MEIE != 0)
            || r.statests & r.wakeen & 0x7fff != 0;
        if cis {
            v |= INT_CTRL;
        }
        if v & r.intctl & 0x7fff_ffff != 0 {
            v |= INT_GLOBAL;
        }
        v
    }

    /// Recompute the interrupt line. `kick`: something new happened or was
    /// acknowledged, so an MSI is due if the condition still holds.
    fn update_irq(&mut self, kick: bool) {
        let level = self.regs.intctl & INT_GLOBAL != 0 && self.intsts() & INT_GLOBAL != 0;
        if self.msi.enabled(&self.cfg) {
            self.intx.set(false);
            if level && (kick || !self.irq_level) {
                self.msi.notify(&self.cfg);
            }
        } else {
            self.intx.set(level && !self.cfg.intx_disabled());
        }
        self.irq_level = level;
    }

    fn walclk(&self) -> u32 {
        // 24 MHz.
        (self.born.elapsed().as_nanos() * 3 / 125) as u32
    }

    fn reg_read(&self, start: u64) -> u32 {
        let r = &self.regs;
        if start >= SD_BASE && start < SD_BASE + N_SD as u64 * SD_SIZE || start >= ALIAS + SD_BASE {
            let rel = start - if start >= ALIAS { ALIAS } else { 0 } - SD_BASE;
            let sd = &self.sds[(rel / SD_SIZE) as usize];
            return match rel % SD_SIZE {
                0 => sd.ctl,
                3 => (sd.sts | if sd.ctl & SD_SRST == 0 { STS_FIFORDY } else { 0 }) as u32,
                4 => sd.lpib,
                8 => sd.cbl,
                12 => sd.lvi as u32,
                14 => sd.fifow as u32,
                16 => FIFO_SIZE,
                18 => sd.fmt as u32,
                24 => sd.bdpl,
                28 => sd.bdpu,
                _ => 0,
            };
        }
        match start {
            0x00 => (N_OUT << 12 | N_IN << 8 | 1) as u32,
            0x03 => 1,
            0x04 => 0x3c,
            0x06 => 0x1d,
            0x08 => r.gctl,
            0x0c => r.wakeen as u32,
            0x0e => r.statests as u32,
            0x10 => r.gsts as u32,
            0x20 => r.intctl,
            0x24 => self.intsts(),
            0x30 | 0x2030 => self.walclk(),
            0x34 | 0x38 => r.ssync,
            0x40 => r.corb_lbase,
            0x44 => r.corb_ubase,
            0x48 => r.corb_wp as u32,
            0x4a => r.corb_rp as u32 | (r.corb_rp_rst as u32) << 15,
            0x4c => r.corb_ctl as u32,
            0x4d => r.corb_sts as u32,
            // Only 256-entry rings offered.
            0x4e => 0x40 | r.corb_size as u32,
            0x50 => r.rirb_lbase,
            0x54 => r.rirb_ubase,
            0x58 => r.rirb_wp as u32,
            0x5a => r.rintcnt as u32,
            0x5c => r.rirb_ctl as u32,
            0x5d => r.rirb_sts as u32,
            0x5e => 0x40 | r.rirb_size as u32,
            0x60 => r.icoi,
            0x64 => r.icii,
            0x68 => r.icis as u32,
            0x70 => r.dpl,
            0x74 => r.dpu,
            _ => 0,
        }
    }

    /// Write to the register at `start`: `val` holds the written bytes in
    /// place, `mask` says which bits were written.
    fn reg_write(&mut self, start: u64, val: u32, mask: u32) {
        let merge = |old: u32| (old & !mask) | (val & mask);
        let w = val & mask;
        if start == 0x08 {
            let old = self.regs.gctl;
            self.regs.gctl = merge(old) & 0x103;
            if old & GCTL_CRST != 0 && self.regs.gctl & GCTL_CRST == 0 {
                self.reset();
            } else if old & GCTL_CRST == 0 && self.regs.gctl & GCTL_CRST != 0 {
                // Out of reset: codec 0 reports in.
                self.regs.statests |= 1;
                self.update_irq(true);
            }
            return;
        }
        if self.in_reset() {
            return;
        }
        if (SD_BASE..SD_BASE + N_SD as u64 * SD_SIZE).contains(&start) {
            let i = ((start - SD_BASE) / SD_SIZE) as usize;
            self.sd_write(i, (start - SD_BASE) % SD_SIZE, val, mask);
            return;
        }
        let r = &mut self.regs;
        match start {
            0x0c => r.wakeen = merge(r.wakeen as u32) as u16 & 0x7fff,
            0x0e => r.statests &= !(w as u16),
            0x10 => r.gsts &= !(w as u16 & 2),
            0x20 => r.intctl = merge(r.intctl),
            0x34 | 0x38 => r.ssync = merge(r.ssync) & 0x3fff_ffff,
            0x40 => r.corb_lbase = merge(r.corb_lbase) & !0x7f,
            0x44 => r.corb_ubase = merge(r.corb_ubase),
            0x48 => {
                r.corb_wp = merge(r.corb_wp as u32) as u16 & 0xff;
                self.run_corb();
            }
            0x4a => {
                if merge((r.corb_rp_rst as u32) << 15) & 0x8000 != 0 {
                    r.corb_rp = 0;
                    r.corb_rp_rst = true;
                } else {
                    r.corb_rp_rst = false;
                }
            }
            0x4c => {
                r.corb_ctl = merge(r.corb_ctl as u32) as u8 & 3;
                self.run_corb();
            }
            0x4d => r.corb_sts &= !(w as u8 & 1),
            0x4e => r.corb_size = merge(r.corb_size as u32) as u8 & 3,
            0x50 => r.rirb_lbase = merge(r.rirb_lbase) & !0x7f,
            0x54 => r.rirb_ubase = merge(r.rirb_ubase),
            0x58 => {
                if w & 0x8000 != 0 {
                    r.rirb_wp = 0;
                }
            }
            0x5a => r.rintcnt = merge(r.rintcnt as u32) as u16 & 0xff,
            0x5c => {
                r.rirb_ctl = merge(r.rirb_ctl as u32) as u8 & 7;
                self.run_corb();
            }
            0x5d => {
                let old = r.rirb_sts;
                r.rirb_sts &= !(w as u8 & (RIRBSTS_INT | RIRBSTS_OIS));
                if old & RIRBSTS_INT != 0 && r.rirb_sts & RIRBSTS_INT == 0 {
                    r.rirb_count = 0;
                    self.run_corb();
                }
            }
            0x5e => r.rirb_size = merge(r.rirb_size as u32) as u8 & 3,
            0x60 => r.icoi = merge(r.icoi),
            0x68 => {
                if w & ICIS_VALID as u32 != 0 {
                    r.icis &= !ICIS_VALID;
                }
                if w & ICIS_BUSY as u32 != 0 && r.icis & ICIS_BUSY == 0 {
                    // Immediate command: answered at once (or never, for
                    // a codec address with nothing there).
                    let cmd = r.icoi;
                    if let Some(resp) = self.codec.verb(cmd) {
                        self.regs.icii = resp;
                        self.regs.icis |= ICIS_VALID;
                    }
                    self.sync_all();
                }
            }
            0x70 => r.dpl = merge(r.dpl) & !0x7e,
            0x74 => r.dpu = merge(r.dpu),
            _ => {}
        }
        self.update_irq(true);
    }

    fn sd_write(&mut self, i: usize, o: u64, val: u32, mask: u32) {
        let merge = |old: u32| (old & !mask) | (val & mask);
        let sd = &mut self.sds[i];
        let running = sd.running();
        match o {
            0 => {
                let new = merge(sd.ctl) & SD_CTL_MASK;
                if new & SD_SRST != 0 {
                    // Stream reset: back to defaults until SRST is cleared.
                    *sd = Sd { ctl: SD_SRST, ..Sd::default() };
                } else {
                    sd.ctl = new;
                    if !running && sd.running() {
                        sd.clock = Some(Clock { t0: Instant::now(), frames: 0 });
                    } else if !sd.running() {
                        sd.clock = None;
                    }
                }
                self.sync_host(i);
            }
            3 => sd.sts &= !((val & mask) as u8 & (STS_BCIS | STS_FIFOE | STS_DESE)),
            // Layout registers are fixed while the stream runs.
            8 if !running => sd.cbl = merge(sd.cbl),
            12 if !running => sd.lvi = merge(sd.lvi as u32) as u16 & 0xff,
            14 => sd.fifow = merge(sd.fifow as u32) as u16 & 7,
            18 if !running => sd.fmt = merge(sd.fmt as u32) as u16,
            24 if !running => sd.bdpl = merge(sd.bdpl) & !0x7f,
            28 if !running => sd.bdpu = merge(sd.bdpu),
            _ => {}
        }
        self.update_irq(true);
    }

    /// Process commands between the CORB read and write pointers.
    fn run_corb(&mut self) {
        if !self.cfg.bus_master() {
            return;
        }
        let n = ring_entries(self.regs.corb_size);
        // At most one pass over the ring per call.
        for _ in 0..n {
            let r = &self.regs;
            if r.corb_ctl & CORBCTL_RUN == 0 || r.corb_rp % n == r.corb_wp % n {
                break;
            }
            let limit = if r.rintcnt == 0 { 256 } else { r.rintcnt };
            if r.rirb_ctl & RIRBCTL_INT != 0 && r.rirb_count >= limit {
                // Wait for the guest to take the responses so far.
                break;
            }
            let rp = (r.corb_rp + 1) % n;
            let base = (r.corb_ubase as u64) << 32 | r.corb_lbase as u64;
            let mut b = [0u8; 4];
            if self.mem.read(GuestAddress(base.wrapping_add(rp as u64 * 4)), &mut b).is_err() {
                // Memory error: stop the CORB.
                self.regs.corb_sts |= 1;
                self.regs.corb_ctl &= !CORBCTL_RUN;
                self.update_irq(true);
                return;
            }
            self.regs.corb_rp = rp;
            if let Some(resp) = self.codec.verb(u32::from_le_bytes(b)) {
                self.respond(resp);
            }
        }
        self.sync_all();
    }

    fn respond(&mut self, resp: u32) {
        let r = &mut self.regs;
        if r.rirb_ctl & RIRBCTL_DMA == 0 {
            return;
        }
        let n = ring_entries(r.rirb_size);
        let wp = (r.rirb_wp + 1) % n;
        let base = (r.rirb_ubase as u64) << 32 | r.rirb_lbase as u64;
        let mut e = [0u8; 8];
        e[..4].copy_from_slice(&resp.to_le_bytes());
        // Extended: codec address 0, solicited.
        let _ = self.mem.write(GuestAddress(base.wrapping_add(wp as u64 * 8)), &e);
        r.rirb_wp = wp;
        r.rirb_count = r.rirb_count.saturating_add(1);
        let limit = if r.rintcnt == 0 { 256 } else { r.rintcnt };
        let corb_empty = r.corb_rp % ring_entries(r.corb_size) == r.corb_wp % ring_entries(r.corb_size);
        if (r.rirb_count >= limit || corb_empty) && r.rirb_ctl & RIRBCTL_INT != 0 {
            r.rirb_sts |= RIRBSTS_INT;
            self.update_irq(true);
        }
    }

    /// Is stream `i` connected to a converter?
    fn routed(&self, i: usize) -> bool {
        let tag = self.sds[i].tag();
        tag != 0 && tag == if Self::is_out(i) { self.codec.dac_tag() } else { self.codec.adc_tag() }
    }

    fn sync_all(&mut self) {
        for i in 0..N_SD {
            self.sync_host(i);
        }
    }

    /// Start or stop the host side (player, microphone) of stream `i` to
    /// match what the guest set up.
    fn sync_host(&mut self, i: usize) {
        let sd = &self.sds[i];
        let fmt = fmt_info(sd.fmt).filter(|f| f.2 == 2);
        let want = !self.paused && sd.running() && self.routed(i) && fmt.is_some();
        if want == self.open[i] {
            return;
        }
        self.open[i] = want;
        let out = Self::is_out(i);
        match (want, fmt) {
            (true, Some((rate, ch, _))) => {
                let ch = ch.min(2) as u8;
                if out {
                    self.sink.start(rate, ch);
                    let lead = rate as u64 * OUTPUT_LEAD_MS / 1000 * 2 * ch as u64;
                    self.sink.write(&vec![0; lead as usize]);
                } else {
                    self.source.start(rate, ch);
                }
            }
            _ if out => self.sink.stop(),
            _ => self.source.stop(),
        }
    }

    /// The stream ran into a bad descriptor or unreachable memory: stop
    /// moving data, flag it (like hardware: the guest resets the stream).
    fn desc_error(&mut self, i: usize) {
        self.sds[i].sts |= STS_DESE;
        self.sds[i].clock = None;
        self.update_irq(true);
    }

    /// Move `bytes` between stream `i`'s buffers and `data` (filled for an
    /// output stream, consumed for an input one). False on a bad
    /// descriptor. Raises the completion interrupt as entries finish.
    fn dma(&mut self, i: usize, data: &mut Vec<u8>, bytes: u64) -> bool {
        let out = Self::is_out(i);
        let (mut left, mut done) = (bytes, 0usize);
        // A pass over every entry twice at most (zero-length ones end it).
        for _ in 0..512 {
            if left == 0 {
                break;
            }
            let sd = &self.sds[i];
            let lvi = sd.lvi & 0xff;
            let bde = sd.bde.min(lvi);
            let mut e = [0u8; 16];
            let at = sd.bdl_base().wrapping_add(bde as u64 * 16);
            if self.mem.read(GuestAddress(at), &mut e).is_err() {
                return false;
            }
            let addr = u64::from_le_bytes(e[..8].try_into().unwrap_or([0; 8]));
            let len = u32::from_le_bytes(e[8..12].try_into().unwrap_or([0; 4]));
            let ioc = e[12] & 1 != 0;
            if len == 0 || sd.bdo >= len {
                return false;
            }
            let take = left.min((len - sd.bdo) as u64) as usize;
            let ga = GuestAddress(addr.wrapping_add(sd.bdo as u64));
            let ok = if out {
                let at = data.len();
                data.resize(at + take, 0);
                self.mem.read(ga, &mut data[at..]).is_ok()
            } else {
                let chunk = data.get(done..done + take).unwrap_or(&[]);
                chunk.len() == take && self.mem.write(ga, chunk).is_ok()
            };
            if !ok {
                return false;
            }
            done += take;
            left -= take as u64;
            let sd = &mut self.sds[i];
            sd.bdo += take as u32;
            sd.lpib = if sd.cbl == 0 { 0 } else { ((sd.lpib as u64 + take as u64) % sd.cbl as u64) as u32 };
            if sd.bdo >= len {
                sd.bdo = 0;
                sd.bde = if bde >= lvi { 0 } else { bde + 1 };
                if ioc {
                    sd.sts |= STS_BCIS;
                }
            }
        }
        true
    }

    /// Move audio for every running stream; returns when to call again.
    pub fn tick(&mut self, now: Instant) -> Duration {
        if self.paused || self.in_reset() || !self.cfg.bus_master() {
            return TICK_IDLE;
        }
        let mut any = false;
        for i in 0..N_SD {
            let (Some(mut clk), Some((rate, ch, bps))) = (self.sds[i].clock, fmt_info(self.sds[i].fmt)) else { continue };
            if !self.sds[i].running() || self.sds[i].cbl == 0 {
                continue;
            }
            any = true;
            let frame = (ch * bps) as u64;
            let due = now.saturating_duration_since(clk.t0).as_micros() as u64 * rate as u64 / 1_000_000;
            let max = (rate as u64 / MAX_LAG_DIV).min(MAX_TICK_BYTES / frame);
            if due.saturating_sub(clk.frames) > max {
                clk.frames = due - max;
            }
            let n = due.saturating_sub(clk.frames);
            clk.frames += n;
            self.sds[i].clock = Some(clk);
            if n == 0 {
                continue;
            }
            let bytes = n * frame;
            let before = self.sds[i].sts;
            let host = self.open[i];
            let ok = if Self::is_out(i) {
                let mut data = Vec::with_capacity(bytes as usize);
                let ok = self.dma(i, &mut data, bytes);
                if host {
                    let mut pcm = to_host(&data, ch);
                    apply_gains(&mut pcm, ch.min(2), gains(self.codec.st.dac_amp));
                    self.sink.write(&pcm);
                }
                ok
            } else {
                let mut data = vec![0u8; bytes as usize];
                if host {
                    let mut pcm = vec![0u8; (n * 2 * ch.min(2) as u64) as usize];
                    self.source.read(&mut pcm);
                    apply_gains(&mut pcm, ch.min(2), gains(self.codec.st.adc_amp));
                    from_host(&pcm, ch, &mut data);
                }
                self.dma(i, &mut data, bytes)
            };
            if !ok {
                self.desc_error(i);
            } else if self.sds[i].sts != before {
                self.update_irq(true);
            }
        }
        if self.regs.dpl & 1 != 0 {
            let base = (self.regs.dpu as u64) << 32 | (self.regs.dpl & !0x7f) as u64;
            for (i, sd) in self.sds.iter().enumerate() {
                let _ = self.mem.write(GuestAddress(base.wrapping_add(i as u64 * 8)), &sd.lpib.to_le_bytes());
            }
        }
        if any {
            TICK_RUNNING
        } else {
            TICK_IDLE
        }
    }
}

/// 16-bit frames of `ch` channels to mono/stereo for the host.
fn to_host(data: &[u8], ch: usize) -> Vec<u8> {
    if ch <= 2 {
        return data[..data.len() / (2 * ch) * (2 * ch)].to_vec();
    }
    data.chunks_exact(2 * ch).flat_map(|f| f[..4].to_vec()).collect()
}

/// Host mono/stereo to the guest's channel count (extra channels silent).
fn from_host(pcm: &[u8], ch: usize, data: &mut [u8]) {
    if ch <= 2 {
        let n = pcm.len().min(data.len());
        data[..n].copy_from_slice(&pcm[..n]);
        return;
    }
    for (f, src) in data.chunks_exact_mut(2 * ch).zip(pcm.as_chunks::<4>().0) {
        f[..4].copy_from_slice(src);
    }
}

impl PciDevice for Hda {
    fn config(&self) -> &ConfigSpace {
        &self.cfg
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.cfg
    }

    fn config_written(&mut self, _off: usize, _len: usize) {
        self.update_irq(false);
    }

    fn bar_read(&mut self, _bar: usize, off: u64, data: &mut [u8]) {
        for (k, b) in data.iter_mut().enumerate() {
            let o = off + k as u64;
            *b = match reg_at(o) {
                Some((start, _)) => (self.reg_read(start) >> (8 * (o - start))) as u8,
                None => 0,
            };
        }
    }

    fn bar_write(&mut self, _bar: usize, off: u64, data: &[u8]) {
        // Split into registers; each gets the bytes that hit it.
        let mut k = 0;
        while k < data.len() {
            let o = off + k as u64;
            let Some((start, size)) = reg_at(o) else {
                k += 1;
                continue;
            };
            let (mut val, mut mask) = (0u32, 0u32);
            while k < data.len() && off + (k as u64) < start + size {
                let sh = 8 * (off + k as u64 - start);
                val |= (data[k] as u32) << sh;
                mask |= 0xff << sh;
                k += 1;
            }
            self.reg_write(start, val, mask);
        }
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn pause(&mut self) {
        self.paused = true;
        self.sync_all();
    }

    fn resume(&mut self) {
        self.paused = false;
        let now = Instant::now();
        for sd in &mut self.sds {
            if sd.running() {
                sd.clock = Some(Clock { t0: now, frames: 0 });
            }
        }
        self.sync_all();
    }

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        encode(&Snap { cfg: self.cfg.clone(), regs: self.regs.clone(), sds: self.sds.to_vec(), codec: self.codec.st.clone() })
    }

    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        let s: Snap = decode(data)?;
        let bad = |m: &str| vroom_snapshot::Error::Invalid(format!("hda: {m}"));
        let mut cfg = s.cfg;
        if !cfg.rebuild_masks(&self.cfg) {
            return Err(bad("layout"));
        }
        let sds: [Sd; N_SD] = s.sds.try_into().map_err(|_| bad("streams"))?;
        self.cfg = cfg;
        self.regs = s.regs;
        self.regs.corb_wp &= 0xff;
        self.regs.corb_rp &= 0xff;
        self.regs.rirb_wp &= 0xff;
        self.sds = sds;
        let now = Instant::now();
        for sd in &mut self.sds {
            sd.lvi &= 0xff;
            sd.bde &= 0xff;
            sd.ctl &= SD_CTL_MASK;
            sd.clock = sd.running().then_some(Clock { t0: now, frames: 0 });
        }
        self.codec.st = s.codec;
        self.sync_all();
        self.update_irq(false);
        Ok(())
    }
}

/// Run `dev`'s streams in real time until the device is dropped.
pub fn spawn_timer(dev: &Arc<Mutex<Hda>>) -> std::io::Result<()> {
    let weak: Weak<Mutex<Hda>> = Arc::downgrade(dev);
    std::thread::Builder::new().name("hda".into()).spawn(move || loop {
        let Some(d) = weak.upgrade() else { break };
        let next = d.lock().unwrap_or_else(|e| e.into_inner()).tick(Instant::now());
        drop(d);
        std::thread::sleep(next);
    })?;
    Ok(())
}

impl Hda {
    /// Value of the register at `off` as the guest reads it (tests).
    #[doc(hidden)]
    pub fn peek(&mut self, off: u64, len: usize) -> u64 {
        let mut b = [0u8; 8];
        self.bar_read(0, off, &mut b[..len.min(8)]);
        u64::from_le_bytes(b)
    }
}

#[cfg(test)]
mod tests;
