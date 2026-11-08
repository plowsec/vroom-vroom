//! Checked legacy configuration mechanism 1 for an explicitly wired PC board.
//!
//! One adapter supplies the shared CF8 address latch and CFC..CFF data lanes.
//! Use absolute port numbers; callers must serialize access to this adapter.
//! The latch/data pair is not an atomic transaction: guest PCI software owns
//! that synchronization. ECAM accesses share the same configuration dispatcher
//! but do not change the latch.
//!
//! The supported subset follows the pinned EDK2 BasePciCf8Lib: aligned 32-bit
//! CF8 accesses, aligned 1/2/4-byte data cycles, reserved selector bits zero,
//! and only the first 256 configuration bytes. Unsupported encodings fail before
//! mutation, rather than emulating additional chipset behavior. There is no
//! BusDevice adapter, machine registration, new PCI identity or snapshot claim.
//! Existing EcamPort remains the owner of the unchanged PCI device snapshot.
//!
//! Board routing must distinguish a 32-bit CF8 address access from the separate
//! ICH9 byte reset-control port at CF9; assigning the entire CF8..CFF window to
//! this adapter would swallow that reset port. CF9 accesses are rejected here.
//!
//! Sources:
//! https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/MdePkg/Library/BasePciCf8Lib/PciCf8Lib.c
//! https://github.com/qemu/qemu/blob/v11.1.2/hw/pci/pci_host.c
//! https://github.com/qemu/qemu/blob/v11.1.2/hw/isa/lpc_ich9.c
//! https://github.com/qemu/qemu/blob/v11.1.2/include/hw/southbridge/ich9.h

use std::sync::Arc;
use super::{PciConfigError, PciRoot};

const ADDRESS: u16 = 0xcf8;
const DATA: u16 = 0xcfc;
const ENABLE: u32 = 1 << 31;
const RESERVED: u32 = 0x7f00_0003;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PciCf8Error {
    #[error("unsupported PCI configuration port")]
    Port,
    #[error("unsupported PCI configuration access width")]
    Width,
    #[error("unaligned PCI configuration data access")]
    Alignment,
    #[error("PCI configuration access crosses a port window")]
    Boundary,
    #[error("unsupported reserved CF8 selector bits")]
    ReservedSelector,
    #[error("PCI configuration access crosses its function")]
    ConfigSpan,
    #[error("PCI host configuration: {0}")]
    Host(PciConfigError),
}

impl From<PciConfigError> for PciCf8Error {
    fn from(error: PciConfigError) -> Self {
        match error {
            PciConfigError::Span => Self::ConfigSpan,
            other => Self::Host(other),
        }
    }
}

#[derive(Debug)]
pub struct PciCf8 {
    root: Arc<PciRoot>,
    address: u32,
}

impl PciCf8 {
    /// Create the PC's single address/data adapter. The latch starts disabled.
    /// This does not register ports or make the root a Q35 chipset.
    pub fn new(root: Arc<PciRoot>) -> Self { Self { root, address: 0 } }

    fn check(port: u16, len: usize) -> Result<(), PciCf8Error> {
        if port == ADDRESS {
            return if len == 4 { Ok(()) } else { Err(PciCf8Error::Width) };
        }
        if !(DATA..=DATA + 3).contains(&port) { return Err(PciCf8Error::Port); }
        if !matches!(len, 1 | 2 | 4) { return Err(PciCf8Error::Width); }
        let lane = usize::from(port - DATA);
        if lane + len > 4 { return Err(PciCf8Error::Boundary); }
        if lane % len != 0 { return Err(PciCf8Error::Alignment); }
        Ok(())
    }

    fn config_offset(&self, port: u16) -> u64 {
        // CF8 bus/dev/function bits are four positions below their ECAM
        // positions; the low byte contains the dword register offset.
        (u64::from(self.address & 0x00ff_ff00) << 4)
            | u64::from(self.address & 0xfc) | u64::from(port - DATA)
    }

    /// Read an absolute CF8/CFC port. On Err, `data` and all device state are
    /// unchanged. Valid disabled/absent cycles return all ones.
    pub fn read(&mut self, port: u16, data: &mut [u8]) -> Result<(), PciCf8Error> {
        Self::check(port, data.len())?;
        if port == ADDRESS {
            data.copy_from_slice(&self.address.to_le_bytes());
        } else if self.address & ENABLE == 0 {
            data.fill(0xff);
        } else {
            self.root.config_access(self.config_offset(port), data, None)?;
        }
        Ok(())
    }

    /// Write an absolute CF8/CFC port. On Err, the address latch and all device
    /// state are unchanged. Writes through a disabled selector do nothing.
    pub fn write(&mut self, port: u16, data: &[u8]) -> Result<(), PciCf8Error> {
        self.write_with_smram(port, data, None)
    }

    pub(super) fn write_with_smram(&mut self, port: u16, data: &[u8],
        apply: Option<super::smram::Apply<'_>>) -> Result<(), PciCf8Error> {
        Self::check(port, data.len())?;
        if port == ADDRESS {
            let value = u32::from_le_bytes(data.try_into().map_err(|_| PciCf8Error::Width)?);
            if value & RESERVED != 0 { return Err(PciCf8Error::ReservedSelector); }
            self.address = value;
        } else if self.address & ENABLE != 0 {
            self.root.config_access_with_smram(self.config_offset(port), &mut [], Some(data), apply)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use crate::{BusDevice, ConcurrentBusDevice};
    use crate::pci::{BarSpec, ConfigSpace, PciDevice, PciId};
    use vroom_snapshot::{decode, encode, Snapshottable};

    struct Probe {
        config: ConfigSpace,
        reads: Vec<(usize, usize)>,
        writes: Vec<(usize, Vec<u8>)>,
    }
    impl PciDevice for Probe {
        fn config(&self) -> &ConfigSpace { &self.config }
        fn config_mut(&mut self) -> &mut ConfigSpace { &mut self.config }
        fn config_read(&mut self, off: usize, data: &mut [u8]) {
            self.reads.push((off, data.len()));
            self.config.read(off, data);
        }
        fn config_write(&mut self, off: usize, data: &[u8]) {
            self.writes.push((off, data.to_vec()));
            if off == 0x80 && data.len() == 1 {
                self.config.raw_mut()[0x80] &= !data[0]; // device-owned W1C
            } else { self.config.write(off, data); }
        }
        fn bar_read(&mut self, bar: usize, off: u64, data: &mut [u8]) {
            crate::put_le(data, ((bar as u64) << 32) | off);
        }
        fn bar_write(&mut self, _: usize, _: u64, _: &[u8]) {}
        fn id(&self) -> String { "cf8-probe".into() }
        fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> { encode(&self.config) }
        fn restore(&mut self, bytes: &[u8]) -> vroom_snapshot::Result<()> {
            let mut config: ConfigSpace = decode(bytes)?;
            if !config.rebuild_masks(&self.config) {
                return Err(vroom_snapshot::Error::Invalid("probe config layout".into()));
            }
            self.config = config; Ok(())
        }
    }

    fn fixture() -> (Arc<PciRoot>, PciCf8, Arc<Mutex<Probe>>) {
        let mut config = ConfigSpace::new(PciId { vendor: 0x1234, device: 0x5678,
            class: 0x010802, revision: 0, subsys_vendor: 0, subsys: 0 }, 1);
        config.add_bar(0, BarSpec { size: 0x4000, is64: true, prefetch: false });
        config.set_wmask(0x40, &[0xff; 0xc0]);
        config.raw_mut()[0x80] = 0xff;
        let probe = Arc::new(Mutex::new(Probe { config, reads: Vec::new(), writes: Vec::new() }));
        let root = PciRoot::new(vec![(1, probe.clone())]);
        let cf8 = PciCf8::new(root.clone());
        (root, cf8, probe)
    }
    fn select(cf8: &mut PciCf8, bus: u8, slot: u8, function: u8, reg: u8) {
        let address = ENABLE | (u32::from(bus) << 16) | (u32::from(slot) << 11)
            | (u32::from(function) << 8) | u32::from(reg);
        cf8.write(ADDRESS, &address.to_le_bytes()).unwrap();
    }
    fn address(cf8: &mut PciCf8) -> [u8; 4] {
        let mut bytes = [0; 4]; cf8.read(ADDRESS, &mut bytes).unwrap(); bytes
    }

    #[test]
    fn transports_share_config_callbacks_and_little_endian_byte_lanes() {
        let (root, mut cf8, probe) = fixture();
        let mut ecam = root.ecam_port();
        select(&mut cf8, 0, 1, 0, 0x40);
        cf8.write(DATA, &[0x11, 0x22, 0x33, 0x44]).unwrap();
        let mut bytes = [0; 4]; ecam.read((1 << 15) + 0x40, &mut bytes);
        assert_eq!(bytes, [0x11, 0x22, 0x33, 0x44]);
        for lane in 0..4 {
            let mut byte = [0]; cf8.read(DATA + lane, &mut byte).unwrap();
            assert_eq!(byte, [bytes[lane as usize]]);
        }
        let mut word = [0; 2]; cf8.read(DATA + 2, &mut word).unwrap();
        assert_eq!(word, [0x33, 0x44]);
        cf8.write(DATA + 1, &[0xaa]).unwrap();
        cf8.write(DATA + 2, &[0xbb, 0xcc]).unwrap();
        ecam.read((1 << 15) + 0x40, &mut bytes);
        assert_eq!(bytes, [0x11, 0xaa, 0xbb, 0xcc]);
        ecam.write((1 << 15) + 0x40, &[0x55, 0x66, 0x77, 0x88]);
        cf8.read(DATA, &mut bytes).unwrap();
        assert_eq!(bytes, [0x55, 0x66, 0x77, 0x88]);
        let probe = probe.lock().unwrap();
        assert_eq!(probe.writes, vec![(0x40, vec![0x11, 0x22, 0x33, 0x44]),
            (0x41, vec![0xaa]), (0x42, vec![0xbb, 0xcc]), (0x40, vec![0x55, 0x66, 0x77, 0x88])]);
        assert_eq!(probe.reads, vec![(0x40, 4), (0x40, 1), (0x41, 1), (0x42, 1),
            (0x43, 1), (0x42, 2), (0x40, 4), (0x40, 4)]);
    }

    #[test]
    fn device_owned_w1c_is_not_bypassed_by_either_transport() {
        let (root, mut cf8, probe) = fixture();
        let mut ecam = root.ecam_port();
        select(&mut cf8, 0, 1, 0, 0x80);
        cf8.write(DATA, &[0x0f]).unwrap();
        let mut byte = [0]; ecam.read((1 << 15) + 0x80, &mut byte);
        assert_eq!(byte, [0xf0]);
        ecam.write((1 << 15) + 0x80, &[0x30]);
        cf8.read(DATA, &mut byte).unwrap();
        assert_eq!(byte, [0xc0]);
        assert_eq!(probe.lock().unwrap().writes, vec![(0x80, vec![0x0f]), (0x80, vec![0x30])]);
        assert_eq!(probe.lock().unwrap().reads, vec![(0x80, 1), (0x80, 1)]);
    }

    #[test]
    fn firmware_address_save_restore_and_disabled_cycles_have_no_config_side_effects() {
        let (root, mut cf8, probe) = fixture();
        let mut ecam = root.ecam_port();
        let snapshot = ecam.save().unwrap();
        assert_eq!(address(&mut cf8), [0; 4]);
        for (port, len) in [(DATA, 4), (DATA, 2), (DATA + 2, 2), (DATA + 3, 1)] {
            let mut bytes = vec![0; len]; cf8.read(port, &mut bytes).unwrap();
            assert_eq!(bytes, vec![0xff; len]); cf8.write(port, &vec![0; len]).unwrap();
        }
        assert!(probe.lock().unwrap().reads.is_empty());
        assert!(probe.lock().unwrap().writes.is_empty());
        select(&mut cf8, 0, 1, 0, 0x40);
        let saved = address(&mut cf8);
        select(&mut cf8, 0, 0, 0, 0);
        let mut bytes = [0; 4]; cf8.read(DATA, &mut bytes).unwrap();
        assert_eq!(bytes, 0x0008_1b36u32.to_le_bytes()); // existing host, no new IDs
        cf8.write(ADDRESS, &saved).unwrap();
        assert_eq!(address(&mut cf8), saved);
        ecam.read(0, &mut bytes);
        assert_eq!(address(&mut cf8), saved, "ECAM must not change CF8");
        assert_eq!(ecam.save().unwrap(), snapshot, "address latch is not a new field in legacy snapshots");
    }

    #[test]
    fn absent_buses_slots_and_functions_do_not_alias_present_devices() {
        let (_, mut cf8, probe) = fixture();
        for (bus, slot, function) in [(0, 2, 0), (0, 31, 7), (1, 1, 0), (255, 31, 7), (0, 0, 1), (0, 1, 1)] {
            select(&mut cf8, bus, slot, function, 0xfc);
            let mut bytes = [0; 4]; cf8.read(DATA, &mut bytes).unwrap();
            assert_eq!(bytes, [0xff; 4]);
            cf8.write(DATA, &[0; 4]).unwrap();
        }
        assert!(probe.lock().unwrap().reads.is_empty());
        assert!(probe.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn invalid_ports_widths_alignment_and_selectors_preserve_latch_buffers_and_devices() {
        let (_, mut cf8, probe) = fixture();
        select(&mut cf8, 0, 1, 0, 0x40);
        let saved = address(&mut cf8);
        for (port, len) in [(ADDRESS, 0), (ADDRESS, 1), (ADDRESS, 2), (ADDRESS, 8),
            (ADDRESS + 1, 1), (ADDRESS + 1, 4), (ADDRESS + 3, 2), (DATA, 0), (DATA, 3), (DATA, 8),
            (DATA + 1, 2), (DATA + 1, 4), (DATA + 2, 4), (DATA + 3, 2),
            (DATA + 4, 1), (0, 4), (u16::MAX, 4)] {
            let mut bytes = vec![0xa5; len];
            assert!(cf8.read(port, &mut bytes).is_err(), "port={port:#x} len={len}");
            assert_eq!(bytes, vec![0xa5; len]);
            assert!(cf8.write(port, &bytes).is_err());
            assert_eq!(address(&mut cf8), saved);
        }
        for bit in [0, 1, 24, 25, 26, 27, 28, 29, 30] {
            assert_eq!(cf8.write(ADDRESS, &(ENABLE | (1 << bit)).to_le_bytes()), Err(PciCf8Error::ReservedSelector));
            assert_eq!(address(&mut cf8), saved);
        }
        assert!(probe.lock().unwrap().reads.is_empty());
        assert!(probe.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn cf8_bar_probes_remapping_and_ecam_snapshot_restore_share_the_real_device() {
        let (root, mut cf8, _) = fixture();
        let mut ecam = root.ecam_port();
        let window = root.window_port(0x1000_0000);
        select(&mut cf8, 0, 1, 0, 0x10);
        cf8.write(DATA, &u32::MAX.to_le_bytes()).unwrap();
        let mut bytes = [0; 4]; cf8.read(DATA, &mut bytes).unwrap();
        assert_eq!(u32::from_le_bytes(bytes), 0xffff_c004);
        cf8.write(DATA, &0x1000_8000u32.to_le_bytes()).unwrap();
        select(&mut cf8, 0, 1, 0, 0x14);
        cf8.write(DATA, &[0; 4]).unwrap();
        let mut bar = [0; 8]; window.read(0x8010, &mut bar);
        assert_eq!(bar, [0xff; 8]); // command.MEM disabled
        select(&mut cf8, 0, 1, 0, 4);
        cf8.write(DATA, &2u16.to_le_bytes()).unwrap();
        window.read(0x8010, &mut bar); assert_eq!(u64::from_le_bytes(bar), 0x10);
        let snapshot = ecam.save().unwrap();
        cf8.write(DATA, &0u16.to_le_bytes()).unwrap();
        window.read(0x8010, &mut bar); assert_eq!(bar, [0xff; 8]);
        ecam.restore(&snapshot).unwrap();
        window.read(0x8020, &mut bar); assert_eq!(u64::from_le_bytes(bar), 0x20);
        cf8.read(DATA, &mut bytes[..2]).unwrap();
        assert_eq!(&bytes[..2], &2u16.to_le_bytes());
        assert_eq!(ecam.save().unwrap(), snapshot, "legacy snapshot bytes round-trip");
    }

    #[test]
    fn checked_dispatch_rejects_whole_malformed_spans_before_callbacks() {
        let (root, _, probe) = fixture();
        let mut ecam = root.ecam_port();
        let snapshot = ecam.save().unwrap();
        for (off, len) in [(1 << 15, 0), ((1 << 15) | 0xfff, 2),
            ((1 << 15) | 0xff8, 9), (1 << 28, 4), (u64::MAX, 4), (0xfff, 2)] {
            let mut data = vec![0xa5; len];
            assert_eq!(root.config_access(off, &mut data, None), Err(PciConfigError::Span));
            assert_eq!(data, vec![0xa5; len], "failed dispatch must not modify the read buffer");
            // Empty scratch must not hide the actual write's length.
            assert_eq!(root.config_access(off, &mut [], Some(&data)), Err(PciConfigError::Span));
            ecam.write(off, &data);
            ecam.read(off, &mut data);
            assert_eq!(data, vec![0xff; len], "infallible ECAM maps malformed reads to all ones");
        }
        assert!(probe.lock().unwrap().reads.is_empty());
        assert!(probe.lock().unwrap().writes.is_empty());
        assert_eq!(ecam.save().unwrap(), snapshot);
    }

    #[test]
    fn ecam_keeps_unaligned_bulk_and_extended_config_accesses() {
        let (root, mut cf8, probe) = fixture();
        let mut ecam = root.ecam_port();
        probe.lock().unwrap().config.set_wmask(0xff8, &[0xff; 8]);
        probe.lock().unwrap().config.set_wmask(0x100, &[0xff; 4]);
        for (off, bytes) in [(0x41, vec![1, 2, 3]), (0x43, vec![4, 5, 6, 7, 8, 9, 10, 11]),
            (0xff8, vec![12, 13, 14, 15, 16, 17, 18, 19]), (0x100, vec![20, 21, 22, 23])] {
            ecam.write((1 << 15) | off, &bytes);
            let mut read = vec![0; bytes.len()];
            ecam.read((1 << 15) | off, &mut read);
            assert_eq!(read, bytes);
        }
        // The last legal CF8 register/lane is byte 255, not extended config.
        select(&mut cf8, 0, 1, 0, 0xfc);
        cf8.write(DATA + 3, &[0xa5]).unwrap();
        let mut byte = [0]; ecam.read((1 << 15) | 0xff, &mut byte);
        assert_eq!(byte, [0xa5]);
        cf8.read(DATA + 3, &mut byte).unwrap(); assert_eq!(byte, [0xa5]);
        let mut extended = [0; 4]; ecam.read((1 << 15) | 0x100, &mut extended);
        assert_eq!(extended, [20, 21, 22, 23]);
        let probe = probe.lock().unwrap();
        assert_eq!(probe.writes.iter().map(|(off, bytes)| (*off, bytes.len())).collect::<Vec<_>>(),
            vec![(0x41, 3), (0x43, 8), (0xff8, 8), (0x100, 4), (0xff, 1)]);
    }

    struct NoMsi;
    impl crate::pci::MsiSender for NoMsi {
        fn send(&self, _: u64, _: u32) {}
    }

    #[test]
    fn cf8_reaches_real_root_port_buses_and_releases_hot_unplugged_devices() {
        use crate::pci::{IntxRouter, RootPort, VendorIds};
        let irq = IntxRouter::new((0..4).map(|_| Arc::new(crate::NoIrq) as Arc<dyn crate::IrqLine>).collect());
        let ids = VendorIds { vendor: 0x1b36, device: 0x000c, subsys_vendor: 0, subsys: 0 };
        let port = Arc::new(Mutex::new(RootPort::new(5, 0, ids, irq.pin(5, 1), Arc::new(NoMsi))));
        let root = PciRoot::with_ports(vec![], VendorIds { device: 8, ..ids }, vec![(5, port)]);
        let mut cf8 = PciCf8::new(root.clone());
        let mut ecam = root.ecam_port();
        let window = root.window_port(0x1000_0000);
        select(&mut cf8, 0, 5, 0, 0);
        let mut data = [0; 4]; cf8.read(DATA, &mut data).unwrap();
        assert_eq!(data, 0x000c_1b36u32.to_le_bytes());
        select(&mut cf8, 0, 5, 0, 0x18);
        cf8.write(DATA, &[0, 1, 1, 0]).unwrap(); // primary, secondary, subordinate
        let mut config = ConfigSpace::new(PciId { vendor: 0x1234, device: 0x5678,
            class: 0x010802, revision: 0, subsys_vendor: 0, subsys: 0 }, 1);
        config.add_bar(0, BarSpec { size: 0x4000, is64: true, prefetch: false });
        let dev = Arc::new(Mutex::new(Probe { config, reads: Vec::new(), writes: Vec::new() }));
        let weak = Arc::downgrade(&dev);
        root.plug(0, dev).unwrap();
        select(&mut cf8, 1, 0, 0, 0);
        cf8.read(DATA, &mut data).unwrap(); assert_eq!(data, [0xff; 4]);
        // Device-owned slot control powers the link through the same callback.
        select(&mut cf8, 0, 5, 0, 0x58);
        cf8.write(DATA, &(1u16 << 5 | 1 << 8).to_le_bytes()).unwrap();
        select(&mut cf8, 1, 0, 0, 0);
        cf8.read(DATA, &mut data).unwrap(); assert_eq!(data, 0x5678_1234u32.to_le_bytes());
        let mut same = [0; 4]; ecam.read(1 << 20, &mut same); assert_eq!(same, data);
        for (bus, slot, function) in [(1, 1, 0), (1, 0, 1), (2, 0, 0)] {
            select(&mut cf8, bus, slot, function, 0);
            cf8.read(DATA, &mut data).unwrap(); assert_eq!(data, [0xff; 4]);
        }
        select(&mut cf8, 1, 0, 0, 0x10);
        cf8.write(DATA, &0x1000_8000u32.to_le_bytes()).unwrap();
        ecam.write((1 << 20) | 4, &2u16.to_le_bytes());
        let mut bar = [0; 8]; window.read(0x8010, &mut bar);
        assert_eq!(u64::from_le_bytes(bar), 0x10);
        assert!(root.unplug(0)); assert!(root.removing(0));
        select(&mut cf8, 0, 5, 0, 0x58);
        cf8.write(DATA, &(1u16 << 5 | 3 << 8 | 1 << 10).to_le_bytes()).unwrap();
        assert!(!root.removing(0)); assert_eq!(root.free_port(), Some(0));
        assert!(weak.upgrade().is_none(), "both the port's gone queue and BAR map must release the device");
        window.read(0x8010, &mut bar); assert_eq!(bar, [0xff; 8]);
        select(&mut cf8, 1, 0, 0, 0);
        cf8.read(DATA, &mut data).unwrap(); assert_eq!(data, [0xff; 4]);
        ecam.read(1 << 20, &mut data); assert_eq!(data, [0xff; 4]);
    }
}
