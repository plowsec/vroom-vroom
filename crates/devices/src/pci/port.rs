//! PCI Express root ports with native hot plug: an empty slot each, on
//! the root bus, that a device can be plugged into and taken out of while
//! the guest runs.
//!
//! The guest's own hot plug driver (Linux's pciehp, Windows' pci.sys)
//! drives the slot through the port's Slot Control and Slot Status
//! registers, as on real hardware. Adding: the slot says a card is
//! present, the guest powers the slot on, sees the link come up and
//! scans the bus behind the port. Removing: we press the slot's
//! attention button, the guest lets go of the device and powers the slot
//! off, and only then does the device go. A guest that never answers
//! (no hot plug driver, or it ignores the button) gets a surprise
//! removal from the caller after a while ([`RootPort::surprise_remove`]).
//!
//! A port forwards config cycles for its secondary bus to the one device
//! behind it (device 0, function 0, while the slot has power). Its memory
//! windows are not enforced: devices' BARs are mapped wherever the guest
//! put them, as for devices on the root bus.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vroom_snapshot::{decode, encode};

use super::{ConfigSpace, Intx, Msi, MsiSender, PciDevice, PciId, SharedPciDevice, VendorIds};

pub const CAP_EXP: u8 = 0x10;

// Registers of the PCI Express capability (offsets from its start).
const EXP_FLAGS: usize = 0x02;
const EXP_LNKCAP: usize = 0x0c;
const EXP_LNKSTA: usize = 0x12;
const EXP_SLTCAP: usize = 0x14;
const EXP_SLTCTL: usize = 0x18;
const EXP_SLTSTA: usize = 0x1a;
const EXP_LNKCAP2: usize = 0x2c;
const EXP_LNKCTL2: usize = 0x30;
/// The capability's length, version 2.
const EXP_LEN: usize = 0x3c;

// Slot Capabilities: attention button, power controller, attention and
// power indicators, hot plug surprise, hot plug capable.
const SLTCAP: u32 = 1 | 1 << 1 | 1 << 3 | 1 << 4 | 1 << 5 | 1 << 6;

// Slot Control.
const ABPE: u16 = 1;
const PFDE: u16 = 1 << 1;
const MRLSCE: u16 = 1 << 2;
const PDCE: u16 = 1 << 3;
const CCIE: u16 = 1 << 4;
const HPIE: u16 = 1 << 5;
const AIC_OFF: u16 = 3 << 6;
const PIC_MASK: u16 = 3 << 8;
const PIC_ON: u16 = 1 << 8;
const PIC_OFF: u16 = 3 << 8;
const PCC: u16 = 1 << 10;
const DLLSCE: u16 = 1 << 12;
/// Everything but the electromechanical interlock (we have none).
const SLTCTL_WRITABLE: u16 = 0x17ff;

// Slot Status.
const ABP: u16 = 1;
const PDC: u16 = 1 << 3;
const CC: u16 = 1 << 4;
const PDS: u16 = 1 << 6;
const DLLSC: u16 = 1 << 8;
/// Event bits: write 1 to clear.
const SLTSTA_W1C: u16 = 0x1f | DLLSC;

// Link Status: 2.5 GT/s, x1, slot clock; Data Link Layer Link Active.
const LNKSTA_UP: u16 = 1 | 1 << 4 | 1 << 12 | 1 << 13;
const LNKSTA_DOWN: u16 = 1 | 1 << 12;

/// The device in the slot, and its state when it went in (what a guest
/// reset puts back).
struct Child {
    dev: SharedPciDevice,
    pristine: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Snap {
    cfg: ConfigSpace,
    /// The device in the slot: its snapshot id and state.
    child: Option<(String, vroom_snapshot::bytes::Bytes)>,
    removing: bool,
}

pub struct RootPort {
    id: String,
    cfg: ConfigSpace,
    exp: usize,
    msi: Msi,
    intx: Arc<Intx>,
    child: Option<Child>,
    /// The hot plug interrupt is raised (what the guest was last told).
    notified: bool,
    /// We asked for the device back; the guest hasn't let go yet.
    removing: bool,
    /// Devices taken out, for the caller to drop outside its locks.
    gone: Vec<SharedPciDevice>,
}

impl std::fmt::Debug for RootPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootPort").field("id", &self.id).field("occupied", &self.child.is_some()).finish()
    }
}

fn get16(c: &ConfigSpace, o: usize) -> u16 {
    let r = c.raw();
    u16::from_le_bytes([r[o], r[o + 1]])
}

fn put16(c: &mut ConfigSpace, o: usize, v: u16) {
    c.raw_mut()[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

/// A PCI Express Endpoint capability (version 2, x1 link at 2.5 GT/s),
/// for devices behind a root port: what Windows and Linux expect there.
pub fn add_endpoint_cap(cfg: &mut ConfigSpace) -> usize {
    let mut body = vec![0u8; EXP_LEN - 2];
    let b = |o: usize| o - 2;
    body[b(EXP_FLAGS)] = 2; // version 2, endpoint
    body[b(EXP_LNKCAP)..b(EXP_LNKCAP) + 4].copy_from_slice(&(1u32 | 1 << 4).to_le_bytes());
    body[b(EXP_LNKSTA)..b(EXP_LNKSTA) + 2].copy_from_slice(&(1u16 | 1 << 4).to_le_bytes());
    body[b(EXP_LNKCAP2)] = 1 << 1;
    let mut wmask = vec![0u8; body.len()];
    wmask[b(0x08)..b(0x08) + 2].copy_from_slice(&0x7fffu16.to_le_bytes()); // device control
    wmask[b(0x10)..b(0x10) + 2].copy_from_slice(&0x0fdbu16.to_le_bytes()); // link control
    wmask[b(0x28)..b(0x28) + 2].copy_from_slice(&0xffffu16.to_le_bytes()); // device control 2
    cfg.add_capability(CAP_EXP, &body, &wmask)
}

impl RootPort {
    /// Root port `index` (0-based) in root bus slot `slot`, with PCI IDs
    /// `ids` (the VM's identity); `intx` is its INTA wiring, `msi` where
    /// its MSI goes.
    pub fn new(slot: u8, index: u8, ids: VendorIds, intx: Arc<Intx>, msi: Arc<dyn MsiSender>) -> Self {
        let mut cfg = ConfigSpace::new(
            PciId {
                vendor: ids.vendor,
                device: ids.device,
                class: 0x06_04_00,
                revision: 0,
                subsys_vendor: ids.subsys_vendor,
                subsys: ids.subsys,
            },
            1,
        );
        cfg.raw_mut()[0x0e] = 0x01; // type 1 header
        // Bus numbers and secondary latency.
        cfg.set_wmask(0x18, &[0xff; 4]);
        // No I/O window (base and limit read 0). Memory window, and a
        // 64-bit prefetchable one.
        cfg.set_wmask(0x20, &[0xf0, 0xff, 0xf0, 0xff, 0xf0, 0xff, 0xf0, 0xff]);
        cfg.set_wmask(0x28, &[0xff; 8]);
        cfg.raw_mut()[0x24] = 1;
        cfg.raw_mut()[0x26] = 1;
        // Bridge control: parity, SERR, ISA, VGA, master abort, bus reset.
        cfg.set_wmask(0x3e, &[0x7f, 0]);

        let mut body = vec![0u8; EXP_LEN - 2];
        let b = |o: usize| o - 2;
        // Version 2, root port, slot implemented.
        body[b(EXP_FLAGS)..b(EXP_FLAGS) + 2].copy_from_slice(&(2u16 | 4 << 4 | 1 << 8).to_le_bytes());
        // Link: 2.5 GT/s x1, reports Data Link Layer Link Active, port number.
        let lnkcap = 1u32 | 1 << 4 | 1 << 20 | (index as u32 + 1) << 24;
        body[b(EXP_LNKCAP)..b(EXP_LNKCAP) + 4].copy_from_slice(&lnkcap.to_le_bytes());
        body[b(EXP_LNKSTA)..b(EXP_LNKSTA) + 2].copy_from_slice(&LNKSTA_DOWN.to_le_bytes());
        // Physical slot number: index + 1.
        let sltcap = SLTCAP | (index as u32 + 1) << 19;
        body[b(EXP_SLTCAP)..b(EXP_SLTCAP) + 4].copy_from_slice(&sltcap.to_le_bytes());
        body[b(EXP_LNKCAP2)] = 1 << 1;
        body[b(EXP_LNKCTL2)] = 1;
        let mut wmask = vec![0u8; body.len()];
        wmask[b(0x08)..b(0x08) + 2].copy_from_slice(&0x7fffu16.to_le_bytes()); // device control
        wmask[b(0x10)..b(0x10) + 2].copy_from_slice(&0x0fdbu16.to_le_bytes()); // link control
        wmask[b(EXP_SLTCTL)..b(EXP_SLTCTL) + 2].copy_from_slice(&SLTCTL_WRITABLE.to_le_bytes());
        wmask[b(0x1c)..b(0x1c) + 2].copy_from_slice(&0x001fu16.to_le_bytes()); // root control
        wmask[b(0x28)..b(0x28) + 2].copy_from_slice(&0xffffu16.to_le_bytes()); // device control 2
        wmask[b(EXP_LNKCTL2)..b(EXP_LNKCTL2) + 2].copy_from_slice(&0x003fu16.to_le_bytes());
        let exp = cfg.add_capability(CAP_EXP, &body, &wmask);
        let msi = Msi::new(&mut cfg, msi);
        let mut p = RootPort {
            id: format!("pcie-port{index}@{slot:02x}"),
            cfg,
            exp,
            msi,
            intx,
            child: None,
            notified: false,
            removing: false,
            gone: Vec::new(),
        };
        p.settle();
        p
    }

    fn sltctl(&self) -> u16 {
        get16(&self.cfg, self.exp + EXP_SLTCTL)
    }

    fn sltsta(&self) -> u16 {
        get16(&self.cfg, self.exp + EXP_SLTSTA)
    }

    fn set_sltsta(&mut self, v: u16) {
        put16(&mut self.cfg, self.exp + EXP_SLTSTA, v);
    }

    /// Raise slot events.
    fn event(&mut self, bits: u16) {
        let s = self.sltsta() | bits;
        self.set_sltsta(s);
    }

    fn powered(&self) -> bool {
        self.sltctl() & PCC == 0
    }

    fn link_up(&self) -> bool {
        get16(&self.cfg, self.exp + EXP_LNKSTA) & (1 << 13) != 0
    }

    /// The link is up while a device is in a powered slot; a change is
    /// an event.
    fn update_link(&mut self) {
        let up = self.child.is_some() && self.powered();
        if up != self.link_up() {
            put16(&mut self.cfg, self.exp + EXP_LNKSTA, if up { LNKSTA_UP } else { LNKSTA_DOWN });
            self.event(DLLSC);
        }
    }

    /// The slot as at power-on, for what is in it now: a device powered
    /// and up, or an empty slot off. No events.
    fn settle(&mut self) {
        let occupied = self.child.is_some();
        let ctl = (self.sltctl() & !(PIC_MASK | PCC)) | AIC_OFF | if occupied { PIC_ON } else { PIC_OFF | PCC };
        put16(&mut self.cfg, self.exp + EXP_SLTCTL, ctl);
        self.set_sltsta(if occupied { PDS } else { 0 });
        put16(&mut self.cfg, self.exp + EXP_LNKSTA, if occupied { LNKSTA_UP } else { LNKSTA_DOWN });
        self.notify();
    }

    /// Raise or drop the hot plug interrupt: enabled events pending, and
    /// hot plug interrupts on. MSI is edge: one message per rise.
    fn notify(&mut self) {
        let ctl = self.sltctl();
        let sta = self.sltsta();
        let on = ctl & HPIE != 0
            && (sta & ctl & (ABPE | PFDE | MRLSCE | PDCE | CCIE) != 0 || (sta & DLLSC != 0 && ctl & DLLSCE != 0));
        let msi = self.msi.enabled(&self.cfg);
        if msi || self.cfg.intx_disabled() {
            self.intx.set(false);
        }
        if on == self.notified {
            return;
        }
        self.notified = on;
        log::debug!("{}: hot plug interrupt {} ({})", self.id, if on { "up" } else { "down" }, if msi { "msi" } else { "intx" });
        if msi {
            if on {
                self.msi.notify(&self.cfg);
            }
        } else if !self.cfg.intx_disabled() {
            self.intx.set(on);
        }
    }

    /// Secondary and subordinate bus numbers the guest gave the port.
    pub fn buses(&self) -> (u8, u8) {
        let r = self.cfg.raw();
        (r[0x19], r[0x1a])
    }

    /// The device config cycles reach: there, and the slot powered.
    pub fn target(&self) -> Option<SharedPciDevice> {
        self.child.as_ref().filter(|_| self.powered()).map(|c| c.dev.clone())
    }

    /// The device in the slot, powered or not.
    pub fn device(&self) -> Option<SharedPciDevice> {
        self.child.as_ref().map(|c| c.dev.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.child.is_none()
    }

    /// Waiting for the guest to let go of its device.
    pub fn removing(&self) -> bool {
        self.removing
    }

    /// Put `dev` in the empty slot. `pristine`: its state now, for guest
    /// resets. `at_start`: the VM is being built (a resumed VM's device):
    /// it is simply there, powered. Else the guest is told a card came.
    pub fn plug(&mut self, dev: SharedPciDevice, pristine: Vec<u8>, at_start: bool) -> Result<(), &'static str> {
        if self.child.is_some() {
            return Err("slot in use");
        }
        self.child = Some(Child { dev, pristine });
        self.removing = false;
        if at_start {
            self.settle();
            return Ok(());
        }
        // Presence changed, and the button pressed as well, as QEMU does
        // (Linux and Windows take either as "turn the slot on").
        self.event(PDC | ABP);
        let s = self.sltsta() | PDS;
        self.set_sltsta(s);
        self.update_link();
        self.notify();
        Ok(())
    }

    /// Ask the guest for the device back: press the attention button.
    /// A guest with hot plug interrupts off can't answer: it goes now.
    /// False if the slot is empty.
    pub fn request_unplug(&mut self) -> bool {
        if self.child.is_none() {
            return false;
        }
        if self.sltctl() & HPIE == 0 {
            self.surprise_remove();
            return true;
        }
        self.removing = true;
        self.event(ABP);
        self.notify();
        true
    }

    /// Take the device out without asking (the guest didn't let go).
    pub fn surprise_remove(&mut self) {
        let Some(c) = self.child.take() else { return };
        self.gone.push(c.dev);
        self.removing = false;
        let s = self.sltsta() & !PDS;
        self.set_sltsta(s);
        self.event(PDC);
        self.update_link();
        self.notify();
    }

    /// Devices taken out since the last call (drop them outside locks).
    pub fn take_gone(&mut self) -> Vec<SharedPciDevice> {
        std::mem::take(&mut self.gone)
    }

    /// A write to Slot Control: one command, done at once.
    fn slot_command(&mut self, old: u16) {
        let ctl = self.sltctl();
        if old & PCC == 0 && ctl & PCC != 0 && self.removing {
            // Powered off after the button: the guest let go.
            self.surprise_remove();
        }
        self.update_link();
        self.event(CC);
    }

    fn lock_child(&self) -> Option<std::sync::MutexGuard<'_, dyn PciDevice + 'static>> {
        self.child.as_ref().map(|c| c.dev.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl PciDevice for RootPort {
    fn config(&self) -> &ConfigSpace {
        &self.cfg
    }

    fn config_mut(&mut self) -> &mut ConfigSpace {
        &mut self.cfg
    }

    fn config_write(&mut self, off: usize, data: &[u8]) {
        let (ctl_at, sta_at) = (self.exp + EXP_SLTCTL, self.exp + EXP_SLTSTA);
        let touches = |at: usize| off < at + 2 && at < off + data.len();
        let old = self.sltctl();
        let mut clear = 0u16;
        for (i, &b) in data.iter().enumerate() {
            match (off + i).checked_sub(sta_at) {
                Some(0) => clear |= b as u16,
                Some(1) => clear |= (b as u16) << 8,
                _ => {}
            }
        }
        self.cfg.write(off, data);
        if clear & SLTSTA_W1C != 0 {
            let s = self.sltsta() & !(clear & SLTSTA_W1C);
            self.set_sltsta(s);
        }
        // A command: a write to Slot Control. Windows clears status bits
        // with a 32-bit write that carries Slot Control unchanged: that
        // alone is no command (else Command Completed comes right back,
        // and Windows clears it again, and again).
        if touches(ctl_at) && (self.sltctl() != old || !touches(sta_at)) {
            self.slot_command(old);
        }
        self.notify();
        if touches(ctl_at) || touches(sta_at) || off < 0x40 && off + data.len() > 0x3c {
            log::debug!("{}: write {off:#x} {data:02x?}: slot control {:#06x} status {:#06x}", self.id, self.sltctl(), self.sltsta());
        }
    }

    fn bar_read(&mut self, _bar: usize, _off: u64, data: &mut [u8]) {
        data.fill(0xff);
    }

    fn bar_write(&mut self, _bar: usize, _off: u64, _data: &[u8]) {}

    fn id(&self) -> String {
        self.id.clone()
    }

    fn pause(&mut self) {
        if let Some(mut c) = self.lock_child() {
            c.pause();
        }
    }

    fn resume(&mut self) {
        if let Some(mut c) = self.lock_child() {
            c.resume();
        }
    }

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        let child = match &self.child {
            Some(c) => {
                let mut d = c.dev.lock().unwrap_or_else(|e| e.into_inner());
                Some((d.id(), vroom_snapshot::bytes::Bytes(d.save()?)))
            }
            None => None,
        };
        encode(&Snap { cfg: self.cfg.clone(), child, removing: self.removing })
    }

    /// The device set must be the saved one: the VM was built with the
    /// device a resumed port had.
    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        let s: Snap = decode(data)?;
        let bad = |m: &str| vroom_snapshot::Error::Invalid(format!("{}: {m}", self.id));
        let mut cfg = s.cfg;
        if !cfg.rebuild_masks(&self.cfg) || cfg.raw()[self.exp] != CAP_EXP {
            return Err(bad("layout"));
        }
        match (&s.child, &self.child) {
            (Some((id, blob)), Some(c)) => {
                let mut d = c.dev.lock().unwrap_or_else(|e| e.into_inner());
                if d.id() != *id {
                    return Err(bad(&format!("has {}, the snapshot {id}", d.id())));
                }
                d.restore(&blob.0)?;
            }
            (None, None) => {}
            (Some((id, _)), None) => return Err(bad(&format!("the snapshot has {id} in it, this VM nothing"))),
            (None, Some(_)) => return Err(bad("the snapshot has an empty slot")),
        }
        self.cfg = cfg;
        self.removing = s.removing;
        // Raise what the guest hasn't seen yet.
        self.notified = false;
        self.notify();
        Ok(())
    }

    /// A guest reset: the port as at power-on; a device it holds stays in
    /// (as a card in a real slot), reset. One the guest was asked to give
    /// back goes.
    fn reset(&mut self, pristine: &[u8]) -> vroom_snapshot::Result<()> {
        let s: Snap = decode(pristine)?;
        let mut cfg = s.cfg;
        if !cfg.rebuild_masks(&self.cfg) {
            return Err(vroom_snapshot::Error::Invalid(format!("{}: layout", self.id)));
        }
        self.cfg = cfg;
        if self.removing {
            if let Some(c) = self.child.take() {
                self.gone.push(c.dev);
            }
            self.removing = false;
        }
        if let Some(c) = &self.child {
            c.dev.lock().unwrap_or_else(|e| e.into_inner()).reset(&c.pristine)?;
        }
        self.notified = false;
        self.intx.set(false);
        self.settle();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::pci::{BarSpec, IntxRouter};
    use crate::{IrqLine, NoIrq};

    const QEMU_PORT: VendorIds = VendorIds { vendor: 0x1b36, device: 0x000c, subsys_vendor: 0, subsys: 0 };

    #[derive(Default)]
    struct Log(Mutex<Vec<u32>>);
    impl MsiSender for Log {
        fn send(&self, _a: u64, d: u32) {
            self.0.lock().unwrap().push(d);
        }
    }

    struct Dev {
        cfg: ConfigSpace,
        resets: u32,
    }
    impl PciDevice for Dev {
        fn config(&self) -> &ConfigSpace {
            &self.cfg
        }
        fn config_mut(&mut self) -> &mut ConfigSpace {
            &mut self.cfg
        }
        fn bar_read(&mut self, _b: usize, _o: u64, d: &mut [u8]) {
            d.fill(0x5a);
        }
        fn bar_write(&mut self, _b: usize, _o: u64, _d: &[u8]) {}
        fn id(&self) -> String {
            "dev".into()
        }
        fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
            encode(&self.cfg)
        }
        fn restore(&mut self, d: &[u8]) -> vroom_snapshot::Result<()> {
            let mut c: ConfigSpace = decode(d)?;
            c.rebuild_masks(&self.cfg);
            self.cfg = c;
            self.resets += 1;
            Ok(())
        }
    }

    fn dev() -> Arc<Mutex<Dev>> {
        let mut cfg = ConfigSpace::new(PciId { vendor: 0x1af4, device: 0x1041, class: 0x020000, revision: 1, subsys_vendor: 0, subsys: 0 }, 1);
        cfg.add_bar(0, BarSpec { size: 0x4000, is64: true, prefetch: false });
        add_endpoint_cap(&mut cfg);
        Arc::new(Mutex::new(Dev { cfg, resets: 0 }))
    }

    fn port() -> (RootPort, Arc<Log>) {
        let r = IntxRouter::new((0..4).map(|_| Arc::new(NoIrq) as Arc<dyn IrqLine>).collect());
        let log = Arc::new(Log::default());
        (RootPort::new(9, 0, QEMU_PORT, r.pin(9, 1), log.clone()), log)
    }

    fn w16(p: &mut RootPort, o: usize, v: u16) {
        let at = p.exp + o;
        p.config_write(at, &v.to_le_bytes());
    }

    fn r16(p: &RootPort, o: usize) -> u16 {
        get16(&p.cfg, p.exp + o)
    }

    /// The guest's side, as Linux's pciehp does it: MSI on, hot plug
    /// events on, slot off while empty.
    fn guest_setup(p: &mut RootPort) {
        let msi = p.cfg.raw()[p.exp + 1] as usize;
        assert_eq!(p.cfg.raw()[msi], crate::pci::msi::CAP_ID);
        p.config_write(msi + 4, &0x0808_0040u32.to_le_bytes());
        p.config_write(msi + 12, &0x77u16.to_le_bytes());
        p.config_write(msi + 2, &1u16.to_le_bytes());
        p.config_write(0x19, &[1, 1]);
        w16(p, EXP_SLTCTL, ABPE | PDCE | CCIE | HPIE | DLLSCE | AIC_OFF | PIC_OFF | PCC);
        w16(p, EXP_SLTSTA, 0xffff);
    }

    #[test]
    fn header_and_capability() {
        let (p, _) = port();
        assert_eq!(p.cfg.raw()[0x0e], 1, "type 1");
        assert_eq!(p.cfg.raw()[0x0b], 0x06);
        assert_eq!(r16(&p, EXP_FLAGS), 0x142, "v2 root port, slot");
        assert_ne!(get16(&p.cfg, p.exp + EXP_SLTCAP) & 1 << 6, 0, "hot plug capable");
        assert_eq!(r16(&p, EXP_SLTSTA) & PDS, 0, "empty");
        assert_eq!(r16(&p, EXP_SLTCTL) & PCC, PCC, "an empty slot is off");
        // Bus numbers are the guest's; the I/O window isn't there.
        let mut p = p;
        p.config_write(0x18, &[0, 3, 3]);
        assert_eq!(p.buses(), (3, 3));
        p.config_write(0x1c, &[0xff, 0xff]);
        assert_eq!(&p.cfg.raw()[0x1c..0x1e], &[0, 0]);
        // Status bits can't be set by writing them.
        w16(&mut p, EXP_SLTSTA, PDS);
        assert_eq!(r16(&p, EXP_SLTSTA) & PDS, 0);
    }

    #[test]
    fn hot_add_then_remove() {
        let (mut p, log) = port();
        guest_setup(&mut p);
        log.0.lock().unwrap().clear();
        let d = dev();
        p.plug(d.clone(), Vec::new(), false).unwrap();
        assert!(p.plug(dev(), Vec::new(), false).is_err(), "one device");
        assert_eq!(r16(&p, EXP_SLTSTA) & (PDS | PDC | ABP), PDS | PDC | ABP);
        assert_eq!(log.0.lock().unwrap().len(), 1, "one MSI");
        assert!(p.target().is_none(), "slot still off");
        // The guest clears the events and powers the slot on: link up.
        w16(&mut p, EXP_SLTSTA, PDC | ABP);
        w16(&mut p, EXP_SLTCTL, ABPE | PDCE | CCIE | HPIE | DLLSCE | AIC_OFF | PIC_ON);
        assert!(p.link_up());
        assert_eq!(r16(&p, EXP_LNKSTA) & 0x3f0, 0x10, "x1");
        assert_eq!(r16(&p, EXP_SLTSTA) & (CC | DLLSC), CC | DLLSC);
        assert_eq!(log.0.lock().unwrap().len(), 2);
        assert!(p.target().is_some());
        w16(&mut p, EXP_SLTSTA, CC | DLLSC);
        // We want it back: the button. The guest powers off: it's gone.
        assert!(p.request_unplug());
        assert!(p.removing());
        assert_eq!(r16(&p, EXP_SLTSTA) & ABP, ABP);
        assert!(p.device().is_some(), "not before the guest lets go");
        w16(&mut p, EXP_SLTSTA, ABP);
        w16(&mut p, EXP_SLTCTL, ABPE | PDCE | CCIE | HPIE | DLLSCE | AIC_OFF | PIC_OFF | PCC);
        assert!(p.is_empty() && !p.removing() && !p.link_up());
        assert_eq!(r16(&p, EXP_SLTSTA) & PDS, 0);
        assert_eq!(p.take_gone().len(), 1);
        assert!(!p.request_unplug(), "nothing left");
    }

    /// Windows writes Slot Control and Slot Status as one dword: clearing
    /// Command Completed that way must leave it clear, and the interrupt
    /// must come again for the next event.
    #[test]
    fn windows_dword_writes() {
        let (mut p, log) = port();
        guest_setup(&mut p);
        let ctl = ABPE | PDCE | CCIE | HPIE | DLLSCE | AIC_OFF | PIC_OFF | PCC;
        let at = p.exp + EXP_SLTCTL;
        p.config_write(at, &(ctl as u32 | (CC as u32) << 16).to_le_bytes());
        assert_eq!(r16(&p, EXP_SLTSTA) & CC, 0, "cleared, no new command");
        let n = log.0.lock().unwrap().len();
        p.plug(dev(), Vec::new(), false).unwrap();
        assert_eq!(log.0.lock().unwrap().len(), n + 1, "the card's interrupt");
        // A real change in the same dword is a command.
        p.config_write(at, &((ctl & !PCC) as u32 | ((PDC | ABP) as u32) << 16).to_le_bytes());
        assert_eq!(r16(&p, EXP_SLTSTA) & (CC | PDC | ABP), CC);
        assert!(p.link_up());
    }

    #[test]
    fn power_off_without_asking_keeps_the_device() {
        let (mut p, _) = port();
        guest_setup(&mut p);
        p.plug(dev(), Vec::new(), false).unwrap();
        w16(&mut p, EXP_SLTCTL, HPIE | PIC_ON);
        w16(&mut p, EXP_SLTCTL, HPIE | PIC_OFF | PCC);
        assert!(p.device().is_some() && !p.link_up());
        assert!(p.target().is_none());
    }

    #[test]
    fn a_guest_without_hot_plug_loses_it_at_once() {
        let (mut p, _) = port();
        p.plug(dev(), Vec::new(), false).unwrap();
        assert!(p.request_unplug());
        assert!(p.is_empty());
        assert_eq!(p.take_gone().len(), 1);
    }

    #[test]
    fn snapshot_and_reset_keep_the_device() {
        let (mut p, _) = port();
        let pristine = p.save().unwrap();
        guest_setup(&mut p);
        let d = dev();
        let fresh = d.lock().unwrap().save().unwrap();
        p.plug(d.clone(), fresh, false).unwrap();
        w16(&mut p, EXP_SLTCTL, HPIE | PIC_ON);
        let s = p.save().unwrap();
        // A port built with the device restores; an empty one refuses.
        let (mut q, _) = port();
        assert!(q.restore(&s).is_err());
        q.plug(dev(), Vec::new(), true).unwrap();
        q.restore(&s).unwrap();
        assert!(q.link_up() && q.target().is_some());
        assert_eq!(q.buses(), (1, 1));
        // A reset: the port's registers from power-on, the device in.
        p.reset(&pristine).unwrap();
        assert_eq!(p.buses(), (0, 0));
        assert!(p.target().is_some() && p.link_up());
        assert_eq!(r16(&p, EXP_SLTSTA), PDS);
        assert_eq!(d.lock().unwrap().resets, 1);
        // One being removed goes at reset.
        p.request_unplug();
        p.reset(&pristine).unwrap();
        assert!(p.is_empty());
    }
}
