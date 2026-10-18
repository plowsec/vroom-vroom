//! Type 0 configuration space with BAR sizing and a capability list.

use serde::{Deserialize, Serialize};

pub const CFG_SIZE: usize = 4096;
const COMMAND: usize = 0x04;
const STATUS: usize = 0x06;
const BAR0: usize = 0x10;
const CAP_PTR: usize = 0x34;
const INT_LINE: usize = 0x3c;
const INT_PIN: usize = 0x3d;

pub const CMD_MEM: u16 = 1 << 1;
pub const CMD_MASTER: u16 = 1 << 2;
pub const CMD_INTX_DISABLE: u16 = 1 << 10;
const STATUS_CAP_LIST: u16 = 1 << 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarSpec {
    /// Power of two, at least 4 KiB.
    pub size: u64,
    pub is64: bool,
    pub prefetch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSpace {
    #[serde(with = "serde_bytes_4k")]
    regs: Vec<u8>,
    #[serde(skip, default = "empty")]
    wmask: Vec<u8>,
    bars: [Option<BarSpec>; 6],
    next_cap: usize,
    last_cap: usize,
}

fn empty() -> Vec<u8> {
    vec![0; CFG_SIZE]
}

mod serde_bytes_4k {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }
    pub fn deserialize<'d, D: Deserializer<'d>>(d: D) -> Result<Vec<u8>, D::Error> {
        let v = Vec::<u8>::deserialize(d)?;
        if v.len() != super::CFG_SIZE {
            return Err(serde::de::Error::custom("config space size"));
        }
        Ok(v)
    }
}

/// Identity of a function.
#[derive(Debug, Clone, Copy)]
pub struct PciId {
    pub vendor: u16,
    pub device: u16,
    /// class << 16 | subclass << 8 | prog_if
    pub class: u32,
    pub revision: u8,
    pub subsys_vendor: u16,
    pub subsys: u16,
}

/// Vendor, device and subsystem IDs: what a guest names a function by
/// (lspci, Device Manager). Class and revision stay with the device model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorIds {
    pub vendor: u16,
    pub device: u16,
    pub subsys_vendor: u16,
    pub subsys: u16,
}

impl ConfigSpace {
    /// `int_pin`: 0 none, 1..=4 INTA..INTD.
    pub fn new(id: PciId, int_pin: u8) -> Self {
        let mut c = ConfigSpace { regs: vec![0; CFG_SIZE], wmask: vec![0; CFG_SIZE], bars: [None; 6], next_cap: 0x40, last_cap: 0 };
        c.put16(0x00, id.vendor);
        c.put16(0x02, id.device);
        c.regs[0x08] = id.revision;
        c.regs[0x09] = id.class as u8;
        c.regs[0x0a] = (id.class >> 8) as u8;
        c.regs[0x0b] = (id.class >> 16) as u8;
        c.put16(0x2c, id.subsys_vendor);
        c.put16(0x2e, id.subsys);
        c.regs[INT_PIN] = int_pin;
        // Writable: command (mem, master, intx disable), interrupt line.
        let cmd_mask = CMD_MEM | CMD_MASTER | CMD_INTX_DISABLE;
        c.wmask[COMMAND..COMMAND + 2].copy_from_slice(&cmd_mask.to_le_bytes());
        c.wmask[INT_LINE] = 0xff;
        c.wmask[0x0c] = 0xff; // cache line size
        c.wmask[0x0d] = 0xff; // latency timer
        c
    }

    /// Replace the vendor, device and subsystem IDs (VM's `[identity]`).
    pub fn set_vendor_ids(&mut self, ids: VendorIds) {
        self.put16(0x00, ids.vendor);
        self.put16(0x02, ids.device);
        self.put16(0x2c, ids.subsys_vendor);
        self.put16(0x2e, ids.subsys);
    }

    pub fn vendor_ids(&self) -> VendorIds {
        VendorIds { vendor: self.get16(0x00), device: self.get16(0x02), subsys_vendor: self.get16(0x2c), subsys: self.get16(0x2e) }
    }

    fn put16(&mut self, o: usize, v: u16) {
        self.regs[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn get16(&self, o: usize) -> u16 {
        u16::from_le_bytes([self.regs[o], self.regs[o + 1]])
    }

    fn get32(&self, o: usize) -> u32 {
        u32::from_le_bytes([self.regs[o], self.regs[o + 1], self.regs[o + 2], self.regs[o + 3]])
    }

    /// Declare BAR `i` (64-bit BARs use i and i+1).
    pub fn add_bar(&mut self, i: usize, spec: BarSpec) {
        assert!(spec.size.is_power_of_two() && spec.size >= 4096 && i < 6);
        assert!(!spec.is64 || i < 5);
        self.bars[i] = Some(spec);
        let o = BAR0 + 4 * i;
        let mut flags = 0u32;
        if spec.is64 {
            flags |= 0b100;
        }
        if spec.prefetch {
            flags |= 0b1000;
        }
        self.regs[o..o + 4].copy_from_slice(&flags.to_le_bytes());
        let mask = !(spec.size - 1);
        self.wmask[o..o + 4].copy_from_slice(&((mask as u32) & !0xf).to_le_bytes());
        if spec.is64 {
            self.wmask[o + 4..o + 8].copy_from_slice(&((mask >> 32) as u32).to_le_bytes());
        }
    }

    /// Append a capability. `body` starts after the id/next bytes;
    /// `wmask` (same length) marks guest-writable bits. Returns its offset.
    pub fn add_capability(&mut self, id: u8, body: &[u8], wmask: &[u8]) -> usize {
        let at = self.next_cap;
        assert!(at + 2 + body.len() <= 256 && wmask.len() == body.len());
        self.regs[at] = id;
        self.regs[at + 1] = 0;
        self.regs[at + 2..at + 2 + body.len()].copy_from_slice(body);
        self.wmask[at + 2..at + 2 + body.len()].copy_from_slice(wmask);
        if self.last_cap == 0 {
            self.regs[CAP_PTR] = at as u8;
        } else {
            self.regs[self.last_cap + 1] = at as u8;
        }
        self.last_cap = at;
        self.next_cap = (at + 2 + body.len() + 3) & !3;
        let st = self.get16(STATUS) | STATUS_CAP_LIST;
        self.put16(STATUS, st);
        at
    }

    pub fn read(&self, off: usize, data: &mut [u8]) {
        for (i, b) in data.iter_mut().enumerate() {
            *b = self.regs.get(off + i).copied().unwrap_or(0);
        }
    }

    /// Generic write through the writable-bit mask.
    pub fn write(&mut self, off: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            let o = off + i;
            if o >= CFG_SIZE {
                break;
            }
            let m = self.wmask[o];
            self.regs[o] = (self.regs[o] & !m) | (b & m);
        }
    }

    /// Raw access for device-owned fields (e.g. capability state).
    pub fn raw(&self) -> &[u8] {
        &self.regs
    }

    pub fn raw_mut(&mut self) -> &mut [u8] {
        &mut self.regs
    }

    /// Mark bits guest-writable (or not) from `off` on: a bridge's
    /// header, device-owned capabilities.
    pub fn set_wmask(&mut self, off: usize, mask: &[u8]) {
        if let Some(w) = self.wmask.get_mut(off..off + mask.len()) {
            w.copy_from_slice(mask);
        }
    }

    pub fn command(&self) -> u16 {
        self.get16(COMMAND)
    }

    pub fn mem_enabled(&self) -> bool {
        self.command() & CMD_MEM != 0
    }

    pub fn bus_master(&self) -> bool {
        self.command() & CMD_MASTER != 0
    }

    pub fn intx_disabled(&self) -> bool {
        self.command() & CMD_INTX_DISABLE != 0
    }

    pub fn int_pin(&self) -> u8 {
        self.regs[INT_PIN]
    }

    pub fn bar_spec(&self, i: usize) -> Option<BarSpec> {
        self.bars.get(i).copied().flatten()
    }

    /// Guest-assigned address of BAR `i`, if any.
    pub fn bar_addr(&self, i: usize) -> Option<u64> {
        let spec = self.bar_spec(i)?;
        let o = BAR0 + 4 * i;
        let lo = (self.get32(o) & !0xf) as u64;
        let hi = if spec.is64 { self.get32(o + 4) as u64 } else { 0 };
        let a = (hi << 32) | lo;
        (a != 0).then_some(a)
    }

    /// Re-apply masks after restore (wmask isn't serialized).
    pub fn rebuild_masks(&mut self, template: &ConfigSpace) -> bool {
        if self.bars != template.bars {
            return false;
        }
        self.wmask = template.wmask.clone();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs() -> ConfigSpace {
        let mut c = ConfigSpace::new(
            PciId { vendor: 0x1af4, device: 0x1041, class: 0x020000, revision: 1, subsys_vendor: 0x1af4, subsys: 1 },
            1,
        );
        c.add_bar(0, BarSpec { size: 0x4000, is64: true, prefetch: true });
        c.add_bar(2, BarSpec { size: 0x1000, is64: false, prefetch: false });
        c
    }

    fn r32(c: &ConfigSpace, o: usize) -> u32 {
        let mut d = [0u8; 4];
        c.read(o, &mut d);
        u32::from_le_bytes(d)
    }

    #[test]
    fn bar_sizing() {
        let mut c = cs();
        c.write(0x10, &u32::MAX.to_le_bytes());
        c.write(0x14, &u32::MAX.to_le_bytes());
        assert_eq!(r32(&c, 0x10), 0xffff_c00c); // size 16K, 64-bit, prefetch
        assert_eq!(r32(&c, 0x14), 0xffff_ffff);
        c.write(0x18, &u32::MAX.to_le_bytes());
        assert_eq!(r32(&c, 0x18), 0xffff_f000);
        c.write(0x10, &0x1000_4000u32.to_le_bytes());
        c.write(0x14, &0x80u32.to_le_bytes());
        assert_eq!(c.bar_addr(0), Some(0x80_1000_4000));
        // Unaligned address is masked down.
        c.write(0x18, &0x2000_0123u32.to_le_bytes());
        assert_eq!(c.bar_addr(2), Some(0x2000_0000));
        // Read-only fields stay put.
        c.write(0, &[0xff; 4]);
        assert_eq!(r32(&c, 0), 0x1041_1af4);
    }

    #[test]
    fn capability_chain() {
        let mut c = cs();
        let a = c.add_capability(0x09, &[1, 2, 3], &[0, 0, 0]);
        let b = c.add_capability(0x11, &[0; 10], &[0xff; 10]);
        assert_eq!(c.raw()[0x34] as usize, a);
        assert_eq!(c.raw()[a + 1] as usize, b);
        assert_eq!(c.raw()[b], 0x11);
        assert_ne!(c.raw()[6] & 0x10, 0);
    }
}
