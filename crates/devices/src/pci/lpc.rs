//! Checked ICH9 LPC PM decode and free-running ACPI timer, cold boot only.
//!
//! This implements neither SMI nor power management. In particular, ACPI_EN
//! enables the PM port window, not an SCI or an ACPI guest mode transition.
//! The selected OVMF enables this window before PCI_COMMAND.IOSE, so IOSE must
//! not gate the timer. No host clock, scheduler or infallible device callback
//! is hidden here: the owner supplies elapsed monotonic nanoseconds.
//!
//! Sources: QEMU v11.1.2 hw/isa/lpc_ich9.c and hw/acpi/core.c; EDK2
//! 4dfdca63a93497203f197ec98ba20e2327e4afe4 OvmfPkg/Library/AcpiTimerLib.

use crate::BusRange;
use super::{ConfigSpace, PciId};

const PMBASE: usize = 0x40;
const ACPI_CNTL: usize = 0x44;
const ACPI_EN: u8 = 0x80;
const PM_MASK: u32 = 0xff80;
const PM_LEN: u64 = 0x80;
const TIMER_OFFSET: u64 = 8;
const TIMER_HZ: u128 = 3_579_545;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LpcError {
    #[error("invalid or overlapping LPC port layout")]
    Layout,
    #[error("unsupported LPC access width")]
    Width,
    #[error("unaligned LPC configuration access")]
    Alignment,
    #[error("LPC access crosses a register or routing boundary")]
    Boundary,
    #[error("LPC register is not implemented")]
    UnsupportedRegister,
    #[error("unsupported LPC configuration bits")]
    ReservedBits,
    #[error("enabled LPC PM window is outside its port reservation")]
    Window,
    #[error("LPC elapsed clock moved backwards")]
    Clock,
}

fn end(r: BusRange) -> Option<u64> {
    r.base.checked_add(r.len).filter(|&end| r.len != 0 && end <= 0x1_0000)
}
fn overlaps(r: BusRange, start: u64, finish: u64) -> bool {
    end(r).is_some_and(|limit| start < limit && r.base < finish)
}

/// Immutable reservation for the LPC's dynamically decoded PM ports.
///
/// There are one or two aligned 128-byte allowed windows and at most 64
/// nonoverlapping excluded intervals. The caller MUST include all other board
/// ports; this type cannot discover omitted devices. Common PC ports below
/// are excluded unconditionally, including the checked CF8/CFC transport.
/// A future board must bind this inventory to its actual PIO map.
#[derive(Debug)]
pub struct LpcPioLayout {
    allowed: Vec<BusRange>,
}

impl LpcPioLayout {
    pub fn new(allowed: Vec<BusRange>, excluded: Vec<BusRange>) -> Result<Self, LpcError> {
        if allowed.is_empty() || allowed.len() > 2 || excluded.len() > 64 {
            return Err(LpcError::Layout);
        }
        // PIC, PIT, keyboard/speaker, CMOS, A20, secondary PIC, APM,
        // common COM ports, fw_cfg, and the whole CF8/CFC selector/data span.
        const PC_PORTS: &[(u64, u64)] = &[(0x20, 2), (0x40, 4), (0x60, 2),
            (0x64, 1), (0x70, 2), (0x92, 1), (0xa0, 2), (0xb2, 2),
            (0x2f8, 8), (0x3f8, 8), (0x510, 2), (0xcf8, 8)];
        for (i, range) in excluded.iter().enumerate() {
            let limit = end(*range).ok_or(LpcError::Layout)?;
            if excluded[..i].iter().any(|r| overlaps(*r, range.base, limit)) {
                return Err(LpcError::Layout);
            }
        }
        for (i, range) in allowed.iter().enumerate() {
            let limit = end(*range).ok_or(LpcError::Layout)?;
            if range.len != PM_LEN || range.base == 0 || range.base % PM_LEN != 0
                || allowed[..i].iter().any(|r| overlaps(*r, range.base, limit))
                || excluded.iter().any(|r| overlaps(*r, range.base, limit))
                || PC_PORTS.iter().any(|&(base, len)| overlaps(BusRange { base, len }, range.base, limit)) {
                return Err(LpcError::Layout);
            }
        }
        Ok(Self { allowed })
    }

    fn accepts(&self, base: u64) -> bool {
        self.allowed.iter().any(|r| r.base == base)
    }
}

/// Private to the checked root. No clone, snapshot or raw configuration handle
/// escapes, and every accepted config write changes the same active decoder.
pub(super) struct Ich9Lpc {
    config: ConfigSpace,
    layout: LpcPioLayout,
    last_ns: u64,
}

impl Ich9Lpc {
    pub(super) fn new(layout: LpcPioLayout) -> Self {
        let mut config = ConfigSpace::new(PciId { vendor: 0x8086, device: 0x2918,
            class: 0x06_01_00, revision: 2, subsys_vendor: 0, subsys: 0 }, 0);
        // No BARs or bus mastering. IOSE is writable but, as in ICH9/QEMU,
        // ACPI_EN owns the PM decode even before IOSE is set by PlatformInit.
        config.set_wmask(4, &[1, 0]);
        config.raw_mut()[PMBASE..PMBASE + 4].copy_from_slice(&1u32.to_le_bytes());
        Self { config, layout, last_ns: 0 }
    }

    fn base(&self) -> u32 {
        u32::from_le_bytes(self.config.raw()[PMBASE..PMBASE + 4].try_into().unwrap())
    }

    fn decode(&self, base: u32, control: u8) -> Result<Option<u64>, LpcError> {
        if base & !(PM_MASK | 1) != 0 || control & !ACPI_EN != 0 {
            return Err(LpcError::ReservedBits);
        }
        let base = u64::from(base & PM_MASK);
        if control & ACPI_EN == 0 || base == 0 { return Ok(None); }
        if !self.layout.accepts(base) { return Err(LpcError::Window); }
        Ok(Some(base))
    }

    pub(super) fn config_access(&mut self, off: usize, data: &mut [u8], write: Option<&[u8]>) -> Result<(), LpcError> {
        let len = write.map_or(data.len(), |bytes| bytes.len());
        if !matches!(len, 1 | 2 | 4) { return Err(LpcError::Width); }
        if off % len != 0 { return Err(LpcError::Alignment); }
        let finish = off.checked_add(len).ok_or(LpcError::Boundary)?;
        if finish <= PMBASE {
            match write {
                Some(bytes) => self.config.write(off, bytes),
                None => self.config.read(off, data),
            }
            return Ok(());
        }
        let is_base = off >= PMBASE && finish <= PMBASE + 4;
        let is_control = off == ACPI_CNTL && len == 1;
        if !is_base && !is_control { return Err(LpcError::UnsupportedRegister); }
        match write {
            Some(bytes) => {
                let mut base = self.base().to_le_bytes();
                let mut control = self.config.raw()[ACPI_CNTL];
                if is_base {
                    base[off - PMBASE..finish - PMBASE].copy_from_slice(bytes);
                    // RTE is hardwired one, not a writable enable bit.
                    base[0] |= 1;
                } else { control = bytes[0]; }
                self.decode(u32::from_le_bytes(base), control)?;
                self.config.raw_mut()[PMBASE..PMBASE + 4].copy_from_slice(&base);
                self.config.raw_mut()[ACPI_CNTL] = control;
            }
            None => self.config.read(off, data),
        }
        Ok(())
    }

    pub(super) fn port_span(port: u16, len: usize) -> Result<(u64, u64), LpcError> {
        if !matches!(len, 1 | 2 | 4) { return Err(LpcError::Width); }
        let start = u64::from(port);
        let finish = start + len as u64;
        if finish > 0x1_0000 { return Err(LpcError::Boundary); }
        Ok((start, finish))
    }

    pub(super) fn pio(&mut self, port: u16, data: &mut [u8], write: Option<&[u8]>, elapsed_ns: u64) -> Result<bool, LpcError> {
        let len = write.map_or(data.len(), |bytes| bytes.len());
        let (start, finish) = Self::port_span(port, len)?;
        let Some(base) = self.decode(self.base(), self.config.raw()[ACPI_CNTL])? else { return Ok(false) };
        if finish <= base || start >= base + PM_LEN { return Ok(false); }
        if start < base || finish > base + PM_LEN { return Err(LpcError::Boundary); }
        if start < base + TIMER_OFFSET || finish > base + TIMER_OFFSET + 4 {
            return Err(if start < base + TIMER_OFFSET + 4 && finish > base + TIMER_OFFSET {
                LpcError::Boundary
            } else { LpcError::UnsupportedRegister });
        }
        if elapsed_ns < self.last_ns { return Err(LpcError::Clock); }
        let timer = ((u128::from(elapsed_ns) * TIMER_HZ / 1_000_000_000) & 0xff_ffff) as u32;
        if write.is_none() {
            let offset = (start - base - TIMER_OFFSET) as usize;
            data.copy_from_slice(&timer.to_le_bytes()[offset..offset + len]);
        }
        // A successful ignored write to this read-only register still observes
        // the supplied clock; malformed/unmapped accesses have no side effects.
        self.last_ns = elapsed_ns;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> LpcPioLayout {
        LpcPioLayout::new(vec![BusRange { base: 0x600, len: PM_LEN }, BusRange { base: 0x800, len: PM_LEN }],
            vec![BusRange { base: 0x700, len: 0x20 }]).unwrap()
    }
    fn write(lpc: &mut Ich9Lpc, reg: usize, bytes: &[u8]) -> Result<(), LpcError> {
        lpc.config_access(reg, &mut [], Some(bytes))
    }
    fn enable(lpc: &mut Ich9Lpc) {
        write(lpc, PMBASE, &0x600u32.to_le_bytes()).unwrap();
        write(lpc, ACPI_CNTL, &[ACPI_EN]).unwrap();
    }
    fn timer(lpc: &mut Ich9Lpc, ns: u64) -> u32 {
        let mut b = [0; 4]; assert!(lpc.pio(0x608, &mut b, None, ns).unwrap()); u32::from_le_bytes(b)
    }

    #[test]
    fn layout_is_bounded_and_reserves_other_port_owners() {
        let valid = BusRange { base: 0x600, len: PM_LEN };
        for allowed in [vec![], vec![valid; 3], vec![valid; 2],
            vec![BusRange { base: 0x601, len: PM_LEN }], vec![BusRange { base: 0xff80, len: 129 }],
            vec![BusRange { base: 0, len: PM_LEN }], vec![BusRange { base: 0x600, len: 256 }]] {
            assert!(matches!(LpcPioLayout::new(allowed, vec![]), Err(LpcError::Layout)));
        }
        for base in [0x80, 0x280, 0x380, 0x500, 0xc80] {
            assert!(LpcPioLayout::new(vec![BusRange { base, len: PM_LEN }], vec![]).is_err());
        }
        for excluded in [vec![BusRange { base: 1, len: 1 }; 65],
            vec![BusRange { base: 0x600, len: 1 }], vec![BusRange { base: 0x5ff, len: 2 }],
            vec![BusRange { base: 0xffff, len: 2 }], vec![BusRange { base: 1, len: 0 }],
            vec![BusRange { base: 0x100, len: 2 }, BusRange { base: 0x101, len: 1 }]] {
            assert!(LpcPioLayout::new(vec![valid], excluded).is_err());
        }
        assert!(LpcPioLayout::new(vec![BusRange { base: 0xff80, len: PM_LEN }], vec![]).is_ok());
    }

    #[test]
    fn early_firmware_order_does_not_need_iose_and_clock_is_real_elapsed_time() {
        let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
        assert_eq!(lpc.config.raw()[4], 0);
        assert_eq!(timer(&mut lpc, 0), 0);
        assert_eq!(timer(&mut lpc, 1_000_000_000), 3_579_545);
        assert_eq!(timer(&mut lpc, 1_000_000_000), 3_579_545);
        assert_eq!(timer(&mut lpc, 5_000_000_000), 17_897_725 & 0xff_ffff);
        assert_eq!(timer(&mut lpc, u64::MAX), ((u128::from(u64::MAX) * TIMER_HZ / 1_000_000_000) & 0xff_ffff) as u32);
        let mut b = [0xa5; 4];
        assert_eq!(lpc.pio(0x608, &mut b, None, u64::MAX - 1), Err(LpcError::Clock));
        assert_eq!(b, [0xa5; 4]);
    }

    #[test]
    fn timer_wraps_at_the_exact_24_bit_tick_boundary() {
        let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
        let wrap_ns = ((1u128 << 24) * 1_000_000_000).div_ceil(TIMER_HZ) as u64;
        assert_eq!(timer(&mut lpc, wrap_ns - 1), 0xff_ffff);
        assert_eq!(timer(&mut lpc, wrap_ns), 0);
        assert_eq!(timer(&mut lpc, wrap_ns), 0, "reads do not increment the clock");
    }

    #[test]
    fn remapping_disabling_and_partial_config_writes_are_atomic() {
        let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc); timer(&mut lpc, 100);
        let before = lpc.config.raw().to_vec();
        for (off, bytes, error) in [(PMBASE, vec![2], LpcError::ReservedBits),
            (PMBASE + 2, vec![1], LpcError::ReservedBits), (ACPI_CNTL, vec![0x81], LpcError::ReservedBits),
            (PMBASE + 1, vec![9], LpcError::Window)] {
            assert_eq!(write(&mut lpc, off, &bytes), Err(error)); assert_eq!(lpc.config.raw(), before);
        }
        write(&mut lpc, PMBASE + 1, &[8]).unwrap();
        let mut b = [0x5a; 4]; assert!(!lpc.pio(0x608, &mut b, None, 200).unwrap()); assert_eq!(b, [0x5a; 4]);
        assert!(lpc.pio(0x808, &mut b, None, 1_000_000_000).unwrap()); assert_eq!(u32::from_le_bytes(b), 3_579_545);
        write(&mut lpc, ACPI_CNTL, &[0]).unwrap();
        assert!(!lpc.pio(0x808, &mut b, None, u64::MAX).unwrap());
        write(&mut lpc, PMBASE, &0x900u32.to_le_bytes()).unwrap();
        assert_eq!(write(&mut lpc, ACPI_CNTL, &[ACPI_EN]), Err(LpcError::Window));
        assert_eq!(lpc.config.raw()[ACPI_CNTL], 0);
        write(&mut lpc, PMBASE, &[0, 6]).unwrap(); write(&mut lpc, ACPI_CNTL, &[ACPI_EN]).unwrap();
        assert_eq!(timer(&mut lpc, 2_000_000_000), 7_159_090);
        write(&mut lpc, PMBASE, &0u32.to_le_bytes()).unwrap();
        assert_eq!(lpc.base(), 1); assert!(!lpc.pio(0x608, &mut b, None, 0).unwrap());
    }

    #[test]
    fn full_spans_and_unsupported_banks_fail_before_read_or_clock_changes() {
        let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
        for (port, len, error) in [(0x5ff, 2, LpcError::Boundary), (0x67f, 2, LpcError::Boundary),
            (0x607, 2, LpcError::Boundary), (0x609, 4, LpcError::Boundary),
            (0x604, 2, LpcError::UnsupportedRegister), (0x630, 4, LpcError::UnsupportedRegister),
            (0xffff, 2, LpcError::Boundary), (0x608, 3, LpcError::Width)] {
            let mut b = vec![0x55; len];
            assert_eq!(lpc.pio(port, &mut b, None, u64::MAX), Err(error));
            assert_eq!(b, vec![0x55; len]); assert_eq!(lpc.last_ns, 0);
            assert_eq!(lpc.pio(port, &mut [], Some(&b), u64::MAX), Err(error));
        }
        for (off, len, error) in [(0x44, 2, LpcError::UnsupportedRegister), (0x41, 2, LpcError::Alignment),
            (0x40, 8, LpcError::Width), (0xa0, 2, LpcError::UnsupportedRegister), (0xf0, 4, LpcError::UnsupportedRegister)] {
            let mut b = vec![0x55; len];
            assert_eq!(lpc.config_access(off, &mut b, None), Err(error)); assert_eq!(b, vec![0x55; len]);
        }
        assert_eq!(timer(&mut lpc, 0), 0);
    }

    #[test]
    fn timer_slices_and_readonly_writes_preserve_frequency() {
        let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
        let expected = 3_579_545u32.to_le_bytes();
        for len in [1, 2, 4] {
            for off in 0..=4 - len {
                let mut b = vec![0; len];
                assert!(lpc.pio(0x608 + off as u16, &mut b, None, 1_000_000_000).unwrap());
                assert_eq!(b, expected[off..off + len]);
                assert!(lpc.pio(0x608 + off as u16, &mut [], Some(&vec![0xff; len]), 1_000_000_000).unwrap());
            }
        }
        assert_eq!(timer(&mut lpc, 1_000_000_000), 3_579_545);
        write(&mut lpc, 4, &[1]).unwrap(); assert_eq!(lpc.config.raw()[4], 1);
        write(&mut lpc, 4, &[0]).unwrap(); assert_eq!(timer(&mut lpc, 2_000_000_000), 7_159_090);
    }
}
