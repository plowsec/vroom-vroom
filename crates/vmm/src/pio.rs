//! Checked scalar port I/O for an explicitly selected PC board.
//!
//! Missing PC ports read as an absent device (all ones); writes are ignored.
//! Boards without port I/O use `None` in VcpuShared and keep rejecting it.
//! KVM rejects string/multiple-element I/O before producing VcpuExit: that
//! interface carries one scalar value, not an element count.
use std::ops::Range;

use vroom_devices::SharedDevice;

use crate::{Error, Result};

pub(crate) struct PioRegion {
    ports: Range<u32>,
    widths: usize,
    device: SharedDevice,
}

impl std::fmt::Debug for PioRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PioRegion")
            .field("ports", &self.ports)
            .field("widths", &self.widths)
            .finish_non_exhaustive()
    }
}

impl PioRegion {
    /// Map a device and the scalar widths it actually implements. A UART
    /// with byte registers should explicitly use `&[1]`.
    #[allow(dead_code)] // The PC board remains gated; exercised by the tests.
    pub(crate) fn new(base: u16, len: u32, widths: &[usize], device: SharedDevice) -> Result<Self> {
        let start = u32::from(base);
        let end = start
            .checked_add(len)
            .filter(|&end| len != 0 && end <= 0x1_0000)
            .ok_or_else(|| {
                Error::Setup("PIO region exceeds the 16-bit port address space".into())
            })?;
        if widths.is_empty()
            || widths
                .iter()
                .any(|&width| !matches!(width, 1 | 2 | 4) || width as u32 > len)
        {
            return Err(Error::Setup(
                "PIO device widths must be scalar 1, 2 or 4 bytes and fit the region".into(),
            ));
        }
        let widths = widths.iter().fold(0, |mask, width| mask | width);
        Ok(Self {
            ports: start..end,
            widths,
            device,
        })
    }
}

#[derive(Debug)]
pub(crate) struct PcPioBus {
    regions: Vec<PioRegion>,
}

impl PcPioBus {
    /// Freeze a non-overlapping map. Constructing this bus opts into the
    /// absent-PC-device behavior; an empty map is distinct from no PIO bus.
    #[allow(dead_code)] // The PC board remains gated; exercised by the tests.
    pub(crate) fn new(mut regions: Vec<PioRegion>) -> Result<Self> {
        regions.sort_by_key(|region| region.ports.start);
        if regions
            .windows(2)
            .any(|pair| pair[0].ports.end > pair[1].ports.start)
        {
            return Err(Error::Setup("overlapping PIO regions".into()));
        }
        Ok(Self { regions })
    }

    fn device(&self, port: u16, len: usize) -> Result<Option<&PioRegion>> {
        if !matches!(len, 1 | 2 | 4) {
            return Err(Error::Setup(
                "PIO exit is not a 1, 2 or 4 byte scalar".into(),
            ));
        }
        let start = u32::from(port);
        let end = start
            .checked_add(len as u32)
            .filter(|&end| end <= 0x1_0000)
            .ok_or_else(|| Error::Setup("PIO access wraps the 16-bit port address space".into()))?;
        // An access starting outside a device may still cross into it.
        // Check the complete span before any device callback or read-buffer write.
        for region in &self.regions {
            if start < region.ports.end && region.ports.start < end {
                if start < region.ports.start || end > region.ports.end {
                    return Err(Error::Setup("PIO access crosses a device boundary".into()));
                }
                if region.widths & len == 0 {
                    return Err(Error::Setup(
                        "PIO width is unsupported by this device".into(),
                    ));
                }
                return Ok(Some(region));
            }
        }
        Ok(None)
    }

    pub(crate) fn read(&self, port: u16, data: &mut [u8]) -> Result<()> {
        match self.device(port, data.len())? {
            Some(region) => {
                let mut device = region
                    .device
                    .lock()
                    .map_err(|_| Error::Setup("PIO device lock poisoned".into()))?;
                device.read(u64::from(port) - u64::from(region.ports.start), data);
            }
            None => data.fill(0xff),
        }
        Ok(())
    }

    pub(crate) fn write(&self, port: u16, data: &[u8]) -> Result<()> {
        if let Some(region) = self.device(port, data.len())? {
            let mut device = region
                .device
                .lock()
                .map_err(|_| Error::Setup("PIO device lock poisoned".into()))?;
            device.write(u64::from(port) - u64::from(region.ports.start), data);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use vroom_devices::BusDevice;

    #[derive(Default)]
    struct Probe {
        calls: usize,
        writes: Vec<(u64, Vec<u8>)>,
    }
    impl BusDevice for Probe {
        fn read(&mut self, offset: u64, data: &mut [u8]) {
            self.calls += 1;
            for (i, byte) in data.iter_mut().enumerate() {
                *byte = (offset as u8) + i as u8 + 0x40;
            }
        }
        fn write(&mut self, offset: u64, data: &[u8]) {
            self.calls += 1;
            self.writes.push((offset, data.to_vec()));
        }
    }

    #[test]
    fn mappings_reject_empty_overflow_overlap_and_invalid_widths() {
        let device = Arc::new(Mutex::new(Probe::default()));
        for (base, len) in [(0, 0), (0xffff, 2), (1, u32::MAX)] {
            assert!(PioRegion::new(base, len, &[1], device.clone()).is_err());
        }
        for widths in [&[][..], &[0], &[3], &[8], &[1, 8]] {
            assert!(PioRegion::new(0x3f8, 8, widths, device.clone()).is_err());
        }
        assert!(PioRegion::new(0x3f8, 1, &[4], device.clone()).is_err());
        let regions = vec![
            PioRegion::new(0x3f8, 8, &[1], device.clone()).unwrap(),
            PioRegion::new(0x3fc, 8, &[1], device.clone()).unwrap(),
        ];
        assert!(PcPioBus::new(regions).is_err());
        assert_eq!(device.lock().unwrap().calls, 0);
    }

    #[test]
    fn full_access_checked_before_device_or_output_side_effects() {
        let device = Arc::new(Mutex::new(Probe::default()));
        let bus = PcPioBus::new(vec![
            PioRegion::new(0x3f8, 8, &[1, 2, 4], device.clone()).unwrap(),
            PioRegion::new(0x400, 8, &[1], device.clone()).unwrap(),
        ])
        .unwrap();
        for (port, len) in [
            (0x3f7, 2),
            (0x3ff, 2),
            (0x3fe, 4),
            (0x400, 2),
            (0xffff, 2),
            (0xfffd, 4),
            (0x3f8, 0),
            (0x3f8, 3),
            (0x3f8, 8),
        ] {
            let mut bytes = vec![0xa5; len];
            assert!(
                bus.read(port, &mut bytes).is_err(),
                "port={port:#x} len={len}"
            );
            assert_eq!(bytes, vec![0xa5; len]);
            assert!(bus.write(port, &bytes).is_err());
        }
        assert_eq!(device.lock().unwrap().calls, 0);
    }

    #[test]
    fn scalar_routing_preserves_offset_width_and_absent_port_semantics() {
        let device = Arc::new(Mutex::new(Probe::default()));
        let bus = PcPioBus::new(vec![
            PioRegion::new(0x3f8, 8, &[1, 2, 4], device.clone()).unwrap()
        ])
        .unwrap();
        for len in [1, 2, 4] {
            let mut bytes = vec![0; len];
            bus.read(0x3fa, &mut bytes).unwrap();
            assert_eq!(bytes, (0..len).map(|n| 0x42 + n as u8).collect::<Vec<_>>());
            bus.write(0x3fa, &bytes).unwrap();
        }
        assert_eq!(
            device.lock().unwrap().writes,
            vec![
                (2, vec![0x42]),
                (2, vec![0x42, 0x43]),
                (2, vec![0x42, 0x43, 0x44, 0x45])
            ]
        );
        let calls = device.lock().unwrap().calls;
        for (port, len) in [(0xffff, 1), (0xfffe, 2), (0xfffc, 4), (0x200, 4)] {
            let mut bytes = vec![0; len];
            bus.read(port, &mut bytes).unwrap();
            assert_eq!(bytes, vec![0xff; len]);
            bus.write(port, &bytes).unwrap();
        }
        assert_eq!(device.lock().unwrap().calls, calls);
    }
}
