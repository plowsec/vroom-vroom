//! Plain MSI (capability 0x05), one message, 64-bit address. For devices
//! whose real counterpart has MSI but not MSI-X (HD Audio).

use std::sync::Arc;

use super::{ConfigSpace, MsiSender};

pub const CAP_ID: u8 = 0x05;
const CTRL_ENABLE: u16 = 1;
const CTRL_64BIT: u16 = 1 << 7;

#[derive(Clone)]
pub struct Msi {
    cap: usize,
    sender: Arc<dyn MsiSender>,
}

impl std::fmt::Debug for Msi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Msi").field("cap", &self.cap).finish()
    }
}

impl Msi {
    /// Add the capability. Guest-writable: enable, the multiple message
    /// enable field (ignored: we have one message), address and data.
    pub fn new(cfg: &mut ConfigSpace, sender: Arc<dyn MsiSender>) -> Self {
        // control(2) addr_lo(4) addr_hi(4) data(2) reserved(2)
        let mut body = vec![0u8; 14];
        body[..2].copy_from_slice(&CTRL_64BIT.to_le_bytes());
        let mut wmask = vec![0u8; 14];
        wmask[0] = 0x71;
        wmask[2..6].copy_from_slice(&[0xfc, 0xff, 0xff, 0xff]);
        wmask[6..12].fill(0xff);
        let cap = cfg.add_capability(CAP_ID, &body, &wmask);
        Msi { cap, sender }
    }

    fn u32_at(cfg: &ConfigSpace, o: usize) -> u32 {
        let r = cfg.raw();
        u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]])
    }

    pub fn enabled(&self, cfg: &ConfigSpace) -> bool {
        let r = cfg.raw();
        u16::from_le_bytes([r[self.cap + 2], r[self.cap + 3]]) & CTRL_ENABLE != 0
    }

    /// Send the message, if enabled.
    pub fn notify(&self, cfg: &ConfigSpace) {
        if !self.enabled(cfg) {
            return;
        }
        let addr = Self::u32_at(cfg, self.cap + 4) as u64 | (Self::u32_at(cfg, self.cap + 8) as u64) << 32;
        let r = cfg.raw();
        let data = u16::from_le_bytes([r[self.cap + 12], r[self.cap + 13]]) as u32;
        self.sender.send(addr, data);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::pci::PciId;

    #[derive(Default)]
    struct Log(Mutex<Vec<(u64, u32)>>);
    impl MsiSender for Log {
        fn send(&self, a: u64, d: u32) {
            self.0.lock().unwrap().push((a, d));
        }
    }

    #[test]
    fn sends_only_when_enabled() {
        let mut cfg =
            ConfigSpace::new(PciId { vendor: 1, device: 2, class: 0x040300, revision: 1, subsys_vendor: 0, subsys: 0 }, 1);
        let log = Arc::new(Log::default());
        let m = Msi::new(&mut cfg, log.clone());
        let at = cfg.raw()[0x34] as usize;
        assert_eq!(cfg.raw()[at], CAP_ID);
        m.notify(&cfg);
        cfg.write(at + 4, &0x0802_0043u32.to_le_bytes());
        cfg.write(at + 8, &1u32.to_le_bytes());
        cfg.write(at + 12, &0x55u16.to_le_bytes());
        cfg.write(at + 2, &0xffffu16.to_le_bytes());
        m.notify(&cfg);
        // Address is dword aligned; 64-bit capable bit stays set.
        assert_eq!(*log.0.lock().unwrap(), vec![(0x1_0802_0040, 0x55)]);
        assert_eq!(cfg.raw()[at + 2] & 0x80, 0x80);
    }
}
