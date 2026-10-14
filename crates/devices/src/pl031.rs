//! ARM PL031 real time clock. Seconds counter backed by host wall clock.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bulwark_snapshot::{decode, encode, Snapshottable};
use serde::{Deserialize, Serialize};

use crate::{get_le, put_le, BusDevice, IrqLine};

const DR: u64 = 0x00;
const MR: u64 = 0x04;
const LR: u64 = 0x08;
const CR: u64 = 0x0c;
const IMSC: u64 = 0x10;
const RIS: u64 = 0x14;
const MIS: u64 = 0x18;
const ICR: u64 = 0x1c;

const ID: [u8; 8] = [0x31, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Regs {
    /// Guest time minus host time, in seconds. Survives snapshots.
    offset: i64,
    mr: u32,
    imsc: u32,
    ris: u32,
}

pub struct Pl031 {
    base: u64,
    regs: Regs,
    irq: Arc<dyn IrqLine>,
}

impl std::fmt::Debug for Pl031 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pl031").field("base", &self.base).finish()
    }
}

fn host_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Pl031 {
    pub fn new(base: u64, irq: Arc<dyn IrqLine>) -> Self {
        Pl031 { base, regs: Regs::default(), irq }
    }

    fn now(&self) -> u32 {
        host_now().wrapping_add(self.regs.offset) as u32
    }

    fn update_irq(&self) {
        self.irq.set_level(self.regs.ris & self.regs.imsc & 1 != 0);
    }
}

impl BusDevice for Pl031 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let v = match offset {
            DR => self.now(),
            MR => self.regs.mr,
            LR => self.now(),
            CR => 1,
            IMSC => self.regs.imsc,
            RIS => self.regs.ris,
            MIS => self.regs.ris & self.regs.imsc,
            0xfe0..=0xffc if offset.is_multiple_of(4) => ID[((offset - 0xfe0) / 4) as usize] as u32,
            _ => 0,
        };
        put_le(data, v as u64);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let v = get_le(data) as u32;
        match offset {
            LR => self.regs.offset = v as i64 - host_now(),
            // Alarm match is accepted but never fires: guests use it rarely
            // and a wakeup alarm makes no sense for a VM.
            MR => self.regs.mr = v,
            IMSC => self.regs.imsc = v & 1,
            ICR => self.regs.ris &= !v,
            _ => {}
        }
        self.update_irq();
    }
}

impl Snapshottable for Pl031 {
    fn snapshot_id(&self) -> String {
        format!("pl031@{:x}", self.base)
    }

    fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>> {
        encode(&self.regs)
    }

    fn restore(&mut self, data: &[u8]) -> bulwark_snapshot::Result<()> {
        self.regs = decode(data)?;
        self.update_irq();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_sets_time() {
        let mut r = Pl031::new(0, Arc::new(crate::NoIrq));
        r.write(LR, &1000u32.to_le_bytes());
        let mut d = [0u8; 4];
        r.read(DR, &mut d);
        let t = u32::from_le_bytes(d);
        assert!((1000..1003).contains(&t));
    }
}
