use std::sync::{Arc, Mutex};

/// A device reachable through MMIO or port I/O.
/// `offset` is relative to the device's base. `data.len()` is 1, 2, 4 or 8.
pub trait BusDevice: Send {
    fn read(&mut self, offset: u64, data: &mut [u8]);
    fn write(&mut self, offset: u64, data: &[u8]);
}

pub type SharedDevice = Arc<Mutex<dyn BusDevice>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BusRange {
    pub base: u64,
    pub len: u64,
}

impl BusRange {
    fn end(&self) -> u64 {
        self.base + self.len
    }
    fn overlaps(&self, o: &BusRange) -> bool {
        self.base < o.end() && o.base < self.end()
    }
}

/// Collects devices, then freezes into an immutable [`Bus`].
#[derive(Default)]
pub struct BusBuilder {
    entries: Vec<(BusRange, SharedDevice)>,
}

impl std::fmt::Debug for BusBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.entries.iter().map(|(r, _)| r)).finish()
    }
}

impl BusBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, base: u64, len: u64, dev: SharedDevice) -> Result<(), String> {
        if len == 0 || base.checked_add(len).is_none() {
            return Err(format!("bad bus range {base:#x}+{len:#x}"));
        }
        let r = BusRange { base, len };
        if let Some((o, _)) = self.entries.iter().find(|(o, _)| o.overlaps(&r)) {
            return Err(format!("{base:#x}+{len:#x} overlaps {:#x}+{:#x}", o.base, o.len));
        }
        self.entries.push((r, dev));
        Ok(())
    }

    pub fn build(mut self) -> Bus {
        self.entries.sort_by_key(|(r, _)| r.base);
        Bus { entries: Arc::new(self.entries) }
    }
}

/// Immutable address -> device map. Lock-free lookup; each device has its
/// own lock so vCPUs touching different devices don't contend.
#[derive(Clone, Default)]
pub struct Bus {
    entries: Arc<Vec<(BusRange, SharedDevice)>>,
}

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.entries.iter().map(|(r, _)| r)).finish()
    }
}

impl Bus {
    fn find(&self, addr: u64) -> Option<(u64, &SharedDevice)> {
        let i = self.entries.partition_point(|(r, _)| r.base <= addr);
        if i == 0 {
            return None;
        }
        let (r, d) = &self.entries[i - 1];
        (addr < r.end()).then(|| (addr - r.base, d))
    }

    /// Returns false if nothing is mapped at `addr`.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        match self.find(addr) {
            Some((off, d)) => {
                d.lock().unwrap_or_else(|e| e.into_inner()).read(off, data);
                true
            }
            None => false,
        }
    }

    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.find(addr) {
            Some((off, d)) => {
                d.lock().unwrap_or_else(|e| e.into_inner()).write(off, data);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Reg(u64);
    impl BusDevice for Reg {
        fn read(&mut self, off: u64, data: &mut [u8]) {
            crate::put_le(data, self.0 + off);
        }
        fn write(&mut self, _off: u64, data: &[u8]) {
            self.0 = crate::get_le(data);
        }
    }

    #[test]
    fn dispatch_and_overlap() {
        let mut b = BusBuilder::new();
        b.insert(0x1000, 0x100, Arc::new(Mutex::new(Reg(10)))).unwrap();
        b.insert(0x2000, 0x100, Arc::new(Mutex::new(Reg(20)))).unwrap();
        assert!(b.insert(0x10ff, 2, Arc::new(Mutex::new(Reg(0)))).is_err());
        assert!(b.insert(u64::MAX, 2, Arc::new(Mutex::new(Reg(0)))).is_err());
        let bus = b.build();
        let mut d = [0u8; 4];
        assert!(bus.read(0x1004, &mut d));
        assert_eq!(u32::from_le_bytes(d), 14);
        assert!(bus.write(0x2000, &[5, 0, 0, 0]));
        assert!(bus.read(0x2001, &mut d));
        assert_eq!(u32::from_le_bytes(d), 6);
        assert!(!bus.read(0x1100, &mut d));
        assert!(!bus.read(0x0fff, &mut d));
    }
}
