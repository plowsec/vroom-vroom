//! Checked ICH9 LPC PM decode and free-running ACPI timer, cold boot only.
//!
//! APM SMI requests use an explicit stopped-CPU callback; normal S5 uses a
//! typed terminal result. SCI and other power management remain unsupported.
//! In particular, ACPI_EN
//! enables the PM port window, not an SCI or an ACPI guest mode transition.
//! The selected OVMF enables this window before PCI_COMMAND.IOSE, so IOSE must
//! not gate the timer. No host clock, scheduler or infallible device callback
//! is hidden here: the owner supplies elapsed monotonic nanoseconds.
//!
//! Sources: QEMU v11.1.2 hw/isa/lpc_ich9.c, hw/acpi/ich9.c and
//! hw/acpi/core.c; EDK2 4dfdca63a93497203f197ec98ba20e2327e4afe4
//! OvmfPkg/Library/AcpiTimerLib and OvmfPkg/SmmControl2Dxe. See ICH9_SMI.md
//! for the explicit virtual-profile limits (no physical EOS/source arbiter).

use crate::BusRange;
use super::{ConfigSpace, PciId};

const PMBASE: usize = 0x40;
const ACPI_CNTL: usize = 0x44;
const ACPI_EN: u8 = 0x80;
const PM_MASK: u32 = 0xff80;
const PM_LEN: u64 = 0x80;
const TIMER_OFFSET: u64 = 8;
const TIMER_HZ: u128 = 3_579_545;
const GEN_PMCON_1: usize = 0xa0;
const SMI_LOCK: u16 = 1 << 4;
const SMI_EN_OFFSET: u64 = 0x30;
const GBL_SMI_EN: u32 = 1;
const APMC_EN: u32 = 1 << 5;
const PM1_CNT_OFFSET: u64 = 4;
const SLP_EN: u16 = 1 << 13;

/// Describe THIS completed RUN's exit, not a cached mode or authorization
/// token. The injection backend must independently check the actual CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LpcIoContext { Normal, Smm }

/// An accepted request is not proof of delivery. That requires the private
/// handler and subsequent RSM/normal continuation to be observed separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LpcSmiRequest { pub command: u8, pub data: u8 }

/// The owner must consume AcpiS5 by stopping the actual exiting CPU without
/// another RUN. A bool cannot represent that effect. The device itself does
/// not own the CPU; its terminal latch only closes subsequent router access.
#[must_use = "AcpiS5 requires the VM owner to stop before another RUN"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LpcPioWriteOutcome { Unmapped, Handled, AcpiS5 }

pub(super) type Inject<'a> = &'a mut dyn FnMut(LpcSmiRequest) -> Result<(), LpcError>;

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
    #[error("APM command requires this exit's context and injection backend")]
    SmiBackendRequired,
    #[error("APM command from SMM is outside this single-CPU profile")]
    SmiContext,
    #[error("APMC enabled without global SMI is outside this profile")]
    SmiState,
    #[error("SMI injection backend failed")]
    SmiBackend,
    #[error("LPC router is poisoned after an ambiguous SMI request")]
    SmiPoisoned,
    #[error("S5 requires a typed terminal consumer for the actual exit")]
    PowerConsumerRequired,
    #[error("only normal-context S5 is supported")]
    PowerContext,
    #[error("LPC is terminal after an accepted ACPI S5 request")]
    PoweredOff,
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
    smi_en: u32,
    smi_locked: bool,
    apm_command: u8,
    apm_data: u8,
    poisoned: bool,
    powered_off: bool,
}

impl Ich9Lpc {
    pub(super) fn new(layout: LpcPioLayout) -> Self {
        let mut config = ConfigSpace::new(PciId { vendor: 0x8086, device: 0x2918,
            class: 0x06_01_00, revision: 2, subsys_vendor: 0, subsys: 0 }, 0);
        // No BARs or bus mastering. IOSE is writable but, as in ICH9/QEMU,
        // ACPI_EN owns the PM decode even before IOSE is set by PlatformInit.
        config.set_wmask(4, &[1, 0]);
        config.raw_mut()[PMBASE..PMBASE + 4].copy_from_slice(&1u32.to_le_bytes());
        Self { config, layout, last_ns: 0, smi_en: 0, smi_locked: false,
            apm_command: 0, apm_data: 0, poisoned: false, powered_off: false }
    }

    pub(super) fn check(&self) -> Result<(), LpcError> {
        if self.poisoned { Err(LpcError::SmiPoisoned) }
        else if self.powered_off { Err(LpcError::PoweredOff) } else { Ok(()) }
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
        self.check()?;
        let len = write.map_or(data.len(), |bytes| bytes.len());
        if !matches!(len, 1 | 2 | 4) { return Err(LpcError::Width); }
        if off % len != 0 { return Err(LpcError::Alignment); }
        let finish = off.checked_add(len).ok_or(LpcError::Boundary)?;
        if off == GEN_PMCON_1 && len == 2 {
            match write {
                Some(bytes) => {
                    let value = u16::from_le_bytes(bytes.try_into().unwrap());
                    if value & !SMI_LOCK != 0 { return Err(LpcError::ReservedBits); }
                    self.smi_locked |= value & SMI_LOCK != 0;
                }
                None => data.copy_from_slice(&(if self.smi_locked { SMI_LOCK } else { 0 }).to_le_bytes()),
            }
            return Ok(());
        }
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
        self.pio_with_smi(port, data, write, elapsed_ns, None, None)
    }

    fn pm1_span(&self, port: u16, len: usize) -> Result<bool, LpcError> {
        let (start, finish) = Self::port_span(port, len)?;
        let Some(base) = self.decode(self.base(), self.config.raw()[ACPI_CNTL])? else { return Ok(false) };
        if finish <= base || start >= base + PM_LEN { return Ok(false); }
        if start < base || finish > base + PM_LEN { return Err(LpcError::Boundary); }
        if start < base + PM1_CNT_OFFSET + 2 && finish > base + PM1_CNT_OFFSET {
            if start != base + PM1_CNT_OFFSET || len != 2 { return Err(LpcError::Boundary); }
            return Ok(true);
        }
        Ok(false)
    }

    fn pm1_value(bytes: &[u8]) -> Result<u16, LpcError> {
        let value = u16::from_le_bytes(bytes.try_into().map_err(|_| LpcError::Width)?);
        // First-EFI subset: SCI is disabled, SLP_TYP0 is the only sleep type,
        // no BM_RLD/GBL_RLS or event-service bits are fabricated as storage.
        if value & !SLP_EN != 0 { return Err(LpcError::ReservedBits); }
        Ok(value)
    }

    pub(super) fn pio_write_with_power(&mut self, port: u16, data: &[u8], elapsed_ns: u64,
        context: LpcIoContext) -> Result<LpcPioWriteOutcome, LpcError> {
        self.check()?;
        if self.pm1_span(port, data.len())? {
            let value = Self::pm1_value(data)?;
            if value == 0 { return Ok(LpcPioWriteOutcome::Handled); }
            if context != LpcIoContext::Normal { return Err(LpcError::PowerContext); }
            // Terminal before the outcome escapes. No callback, invisible
            // hardware stop or rollback is claimed; the real owner must stop.
            self.powered_off = true;
            return Ok(LpcPioWriteOutcome::AcpiS5);
        }
        // This API cannot inject an APM SMI: B2 still needs its own callback.
        self.pio(port, &mut [], Some(data), elapsed_ns)
            .map(|handled| if handled { LpcPioWriteOutcome::Handled } else { LpcPioWriteOutcome::Unmapped })
    }

    pub(super) fn pio_with_smi(&mut self, port: u16, data: &mut [u8], write: Option<&[u8]>, elapsed_ns: u64,
        context: Option<LpcIoContext>, inject: Option<Inject<'_>>) -> Result<bool, LpcError> {
        self.check()?;
        let len = write.map_or(data.len(), |bytes| bytes.len());
        let (start, finish) = Self::port_span(port, len)?;
        if self.pm1_span(port, len)? {
            match write {
                Some(bytes) => {
                    if Self::pm1_value(bytes)? != 0 { return Err(LpcError::PowerConsumerRequired); }
                }
                None => data.fill(0), // SLP_EN is a command, never retained storage.
            }
            return Ok(true);
        }
        // Fixed APM ports exist independently of PMBASE and ACPI_EN. Reject
        // an overlapping wide access before invoking any backend or mutation.
        if start < 0xb4 && finish > 0xb2 {
            if len != 1 || start < 0xb2 { return Err(LpcError::Boundary); }
            match (port, write) {
                (0xb3, Some(bytes)) => self.apm_data = bytes[0],
                (0xb3, None) => data[0] = self.apm_data,
                (0xb2, None) => data[0] = self.apm_command,
                (0xb2, Some(bytes)) => {
                    let command = bytes[0];
                    if matches!(command, 2 | 3) { return Err(LpcError::UnsupportedRegister); }
                    let context = context.ok_or(LpcError::SmiBackendRequired)?;
                    if context == LpcIoContext::Smm { return Err(LpcError::SmiContext); }
                    let inject = inject.ok_or(LpcError::SmiBackendRequired)?;
                    if self.smi_en & APMC_EN != 0 {
                        if self.smi_en & GBL_SMI_EN == 0 { return Err(LpcError::SmiState); }
                        // Latched before external work; Err or panic cannot
                        // roll an accepted/ambiguous injection back. Recovery
                        // is a new cold VM, never another RUN on this router.
                        self.poisoned = true;
                        inject(LpcSmiRequest { command, data: self.apm_data }).map_err(|_| LpcError::SmiBackend)?;
                        self.apm_command = command;
                        self.poisoned = false;
                    } else { self.apm_command = command; }
                }
                _ => unreachable!(),
            }
            return Ok(true);
        }
        let Some(base) = self.decode(self.base(), self.config.raw()[ACPI_CNTL])? else { return Ok(false) };
        if finish <= base || start >= base + PM_LEN { return Ok(false); }
        if start < base || finish > base + PM_LEN { return Err(LpcError::Boundary); }
        if start < base + SMI_EN_OFFSET + 4 && finish > base + SMI_EN_OFFSET {
            if start != base + SMI_EN_OFFSET || len != 4 { return Err(LpcError::Boundary); }
            match write {
                Some(bytes) => {
                    let mut value = u32::from_le_bytes(bytes.try_into().unwrap());
                    if value & !(GBL_SMI_EN | APMC_EN) != 0 { return Err(LpcError::ReservedBits); }
                    // QEMU's virtual ICH9 lock freezes GBL only. APMC remains
                    // writable; physical EOS/source arbitration is not modeled.
                    if self.smi_locked { value = (value & !GBL_SMI_EN) | (self.smi_en & GBL_SMI_EN); }
                    self.smi_en = value;
                }
                None => data.copy_from_slice(&self.smi_en.to_le_bytes()),
            }
            return Ok(true);
        }
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
            (0x600, 2, LpcError::UnsupportedRegister), (0x634, 4, LpcError::UnsupportedRegister),
            (0xffff, 2, LpcError::Boundary), (0x608, 3, LpcError::Width)] {
            let mut b = vec![0x55; len];
            assert_eq!(lpc.pio(port, &mut b, None, u64::MAX), Err(error));
            assert_eq!(b, vec![0x55; len]); assert_eq!(lpc.last_ns, 0);
            assert_eq!(lpc.pio(port, &mut [], Some(&b), u64::MAX), Err(error));
        }
        for (off, len, error) in [(0x44, 2, LpcError::UnsupportedRegister), (0x41, 2, LpcError::Alignment),
            (0x40, 8, LpcError::Width), (0xa2, 2, LpcError::UnsupportedRegister), (0xf0, 4, LpcError::UnsupportedRegister)] {
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

    fn smi_enable(lpc: &mut Ich9Lpc, value: u32) {
        assert!(lpc.pio(0x630, &mut [], Some(&value.to_le_bytes()), 0).unwrap());
    }
    fn command(lpc: &mut Ich9Lpc, value: u8, context: LpcIoContext, inject: Inject<'_>) -> Result<bool, LpcError> {
        lpc.pio_with_smi(0xb2, &mut [], Some(&[value]), 0, Some(context), Some(inject))
    }

    #[test]
    fn smi_lock_matches_ovmf_and_freezes_only_global_at_either_initial_value() {
        for initial in [0, GBL_SMI_EN] {
            let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
            smi_enable(&mut lpc, initial | APMC_EN);
            write(&mut lpc, GEN_PMCON_1, &SMI_LOCK.to_le_bytes()).unwrap();
            write(&mut lpc, GEN_PMCON_1, &[0, 0]).unwrap();
            for value in [0, APMC_EN, GBL_SMI_EN | APMC_EN] {
                smi_enable(&mut lpc, value);
                let mut got = [0; 4]; lpc.pio(0x630, &mut got, None, 0).unwrap();
                assert_eq!(u32::from_le_bytes(got), initial | (value & APMC_EN));
                let mut lock = [0; 2]; lpc.config_access(GEN_PMCON_1, &mut lock, None).unwrap();
                assert_eq!(u16::from_le_bytes(lock), SMI_LOCK);
            }
            assert_eq!(write(&mut lpc, GEN_PMCON_1, &0x11u16.to_le_bytes()), Err(LpcError::ReservedBits));
            assert!(lpc.smi_locked);
        }
    }

    #[test]
    fn every_enable_context_pair_has_a_truthful_callback_count() {
        for bits in [0, GBL_SMI_EN, APMC_EN, GBL_SMI_EN | APMC_EN] {
            for context in [LpcIoContext::Normal, LpcIoContext::Smm] {
                let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc); smi_enable(&mut lpc, bits);
                lpc.pio(0xb3, &mut [], Some(&[0xa5]), 0).unwrap();
                let mut requests = vec![];
                let result = command(&mut lpc, 0x42, context, &mut |r| { requests.push(r); Ok(()) });
                let expected = if context == LpcIoContext::Smm { Err(LpcError::SmiContext) }
                    else if bits == APMC_EN { Err(LpcError::SmiState) } else { Ok(true) };
                assert_eq!(result, expected);
                assert_eq!(requests.len(), usize::from(context == LpcIoContext::Normal && bits == 0x21));
                if !requests.is_empty() { assert_eq!(requests[0], LpcSmiRequest { command: 0x42, data: 0xa5 }); }
                assert_eq!(lpc.apm_command, if result.is_ok() { 0x42 } else { 0 });
                assert!(!lpc.poisoned);
            }
        }
    }

    #[test]
    fn apm_is_fixed_but_never_invents_context_or_sci() {
        let mut lpc = Ich9Lpc::new(layout());
        assert!(lpc.pio(0xb3, &mut [], Some(&[0x91]), 0).unwrap());
        let mut data = [0]; assert!(lpc.pio(0xb3, &mut data, None, 0).unwrap()); assert_eq!(data, [0x91]);
        assert_eq!(lpc.pio(0xb2, &mut [], Some(&[0x42]), 0), Err(LpcError::SmiBackendRequired));
        let mut calls = 0;
        for context in [LpcIoContext::Normal, LpcIoContext::Smm] {
            for value in [2, 3] {
                assert_eq!(command(&mut lpc, value, context, &mut |_| { calls += 1; Ok(()) }), Err(LpcError::UnsupportedRegister));
            }
        }
        assert_eq!(calls, 0); assert_eq!(lpc.apm_command, 0);
        assert!(command(&mut lpc, 0x41, LpcIoContext::Normal, &mut |_| panic!("disabled injection")).unwrap());
        assert!(lpc.pio(0xb2, &mut data, None, 0).unwrap()); assert_eq!(data, [0x41]);
        enable(&mut lpc); smi_enable(&mut lpc, 0x21);
        write(&mut lpc, ACPI_CNTL, &[0]).unwrap();
        assert!(command(&mut lpc, 0x42, LpcIoContext::Normal, &mut |_| { calls += 1; Ok(()) }).unwrap());
        assert_eq!(calls, 1, "fixed APM decoding is not ACPI_EN gated");
    }

    #[test]
    fn smi_spans_and_reserved_bits_are_rejected_without_state_or_effect() {
        let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
        for (port, bytes) in [(0xb1, vec![0; 2]), (0xb2, vec![0; 2]), (0xb3, vec![0; 2]),
            (0x62f, vec![0; 2]), (0x630, vec![0; 1]), (0x631, vec![0; 4])] {
            let mut read = vec![0xa5; bytes.len()];
            assert_eq!(lpc.pio(port, &mut read, None, 0), Err(LpcError::Boundary)); assert!(read.iter().all(|b| *b == 0xa5));
            assert_eq!(lpc.pio_with_smi(port, &mut [], Some(&bytes), 0, Some(LpcIoContext::Normal),
                Some(&mut |_| panic!("invalid span invoked backend"))), Err(LpcError::Boundary));
        }
        assert_eq!(lpc.pio(0x630, &mut [], Some(&2u32.to_le_bytes()), 0), Err(LpcError::ReservedBits));
        for (off, len) in [(0xa0, 1), (0xa0, 4), (0xa1, 1)] {
            let mut read = vec![0xa5; len];
            assert_eq!(lpc.config_access(off, &mut read, None), Err(LpcError::UnsupportedRegister));
            assert!(read.iter().all(|b| *b == 0xa5));
        }
        assert_eq!(lpc.smi_en, 0); assert_eq!(lpc.apm_command, 0); assert_eq!(lpc.apm_data, 0); assert!(!lpc.poisoned);
    }

    #[test]
    fn failed_or_panicking_backend_latches_poison_before_command_commit() {
        for panic in [false, true] {
            let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc); smi_enable(&mut lpc, 0x21);
            let mut calls = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| command(&mut lpc, 0x42,
                LpcIoContext::Normal, &mut |_| { calls += 1; if panic { panic!("injected backend panic") } Err(LpcError::SmiBackend) })));
            if panic { assert!(result.is_err()); } else { assert_eq!(result.unwrap(), Err(LpcError::SmiBackend)); }
            assert_eq!(calls, 1); assert!(lpc.poisoned); assert_eq!(lpc.apm_command, 0);
            let mut data = [0xa5; 4];
            assert_eq!(lpc.pio(0x608, &mut data, None, 0), Err(LpcError::SmiPoisoned));
            assert_eq!(lpc.config_access(0, &mut data, None), Err(LpcError::SmiPoisoned));
            assert_eq!(data, [0xa5; 4]);
            assert_eq!(command(&mut lpc, 0x43, LpcIoContext::Normal, &mut |_| panic!("retry")), Err(LpcError::SmiPoisoned));
        }
    }

    #[test]
    fn pm1_clear_and_readback_do_not_arm_or_fake_sci_before_terminal_s5() {
        for clear_first in [false, true] {
            let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
            let mut value = [0xa5; 2];
            assert!(lpc.pio(0x604, &mut value, None, 0).unwrap()); assert_eq!(value, [0, 0]);
            if clear_first {
                assert_eq!(lpc.pio_write_with_power(0x604, &[0, 0], 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::Handled));
                assert!(lpc.pio(0x604, &mut value, None, 0).unwrap()); assert_eq!(value, [0, 0]);
                assert!(!lpc.powered_off);
            }
            assert_eq!(lpc.pio_write_with_power(0x604, &SLP_EN.to_le_bytes(), 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::AcpiS5));
            assert!(lpc.powered_off); assert!(!lpc.poisoned, "normal poweroff is not an ambiguous SMI failure");
            value.fill(0xa5);
            assert_eq!(lpc.pio(0x604, &mut value, None, 0), Err(LpcError::PoweredOff));
            assert_eq!(lpc.config_access(PMBASE, &mut value, None), Err(LpcError::PoweredOff));
            assert_eq!(value, [0xa5; 2]);
            assert_eq!(lpc.pio_write_with_power(0x604, &SLP_EN.to_le_bytes(), 0, LpcIoContext::Normal), Err(LpcError::PoweredOff));
        }
    }

    #[test]
    fn missing_terminal_consumer_and_smm_context_refuse_before_mutation() {
        let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
        assert_eq!(lpc.pio(0x604, &mut [], Some(&SLP_EN.to_le_bytes()), 0), Err(LpcError::PowerConsumerRequired));
        assert_eq!(lpc.pio_with_smi(0x604, &mut [], Some(&SLP_EN.to_le_bytes()), 0,
            Some(LpcIoContext::Normal), Some(&mut |_| panic!("power write injected SMI"))), Err(LpcError::PowerConsumerRequired));
        assert_eq!(lpc.pio_write_with_power(0x604, &SLP_EN.to_le_bytes(), 0, LpcIoContext::Smm), Err(LpcError::PowerContext));
        assert_eq!(lpc.pio_write_with_power(0xb2, &[0x42], 0, LpcIoContext::Normal), Err(LpcError::SmiBackendRequired));
        assert!(!lpc.powered_off); assert!(!lpc.poisoned);
        assert_eq!(lpc.pio_write_with_power(0x604, &[0; 2], 0, LpcIoContext::Smm), Ok(LpcPioWriteOutcome::Handled));
        assert_eq!(lpc.pio_write_with_power(0x604, &SLP_EN.to_le_bytes(), 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::AcpiS5));
    }

    #[test]
    fn pm1_unsupported_controls_and_full_spans_have_no_side_effects() {
        let mut lpc = Ich9Lpc::new(layout()); enable(&mut lpc);
        for bit in (0..16).filter(|b| *b != 13) {
            for value in [1u16 << bit, (1u16 << bit) | SLP_EN] {
                assert_eq!(lpc.pio_write_with_power(0x604, &value.to_le_bytes(), 0, LpcIoContext::Normal), Err(LpcError::ReservedBits));
                assert!(!lpc.powered_off);
            }
        }
        for (port, len) in [(0x603, 2), (0x604, 1), (0x604, 4), (0x605, 1), (0x605, 2)] {
            let mut value = vec![0xa5; len];
            assert_eq!(lpc.pio(port, &mut value, None, 0), Err(LpcError::Boundary)); assert!(value.iter().all(|b| *b == 0xa5));
            assert_eq!(lpc.pio_write_with_power(port, &vec![0; len], 0, LpcIoContext::Normal), Err(LpcError::Boundary));
        }
        assert!(!lpc.powered_off); assert_eq!(lpc.last_ns, 0);
        for port in [0x600, 0x602, 0x606, 0x620] {
            assert_eq!(lpc.pio_write_with_power(port, &[0; 2], 0, LpcIoContext::Normal), Err(LpcError::UnsupportedRegister));
        }
    }

    #[test]
    fn power_decode_follows_actual_pm_window_and_never_old_ports() {
        let mut lpc = Ich9Lpc::new(layout());
        assert_eq!(lpc.pio_write_with_power(0x604, &SLP_EN.to_le_bytes(), 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::Unmapped));
        enable(&mut lpc); write(&mut lpc, PMBASE, &0x800u32.to_le_bytes()).unwrap();
        assert_eq!(lpc.pio_write_with_power(0x604, &SLP_EN.to_le_bytes(), 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::Unmapped));
        let mut value = [0xa5; 2]; assert!(!lpc.pio(0x604, &mut value, None, 0).unwrap()); assert_eq!(value, [0xa5; 2]);
        assert!(lpc.pio(0x804, &mut value, None, 0).unwrap()); assert_eq!(value, [0; 2]);
        write(&mut lpc, ACPI_CNTL, &[0]).unwrap();
        assert_eq!(lpc.pio_write_with_power(0x804, &SLP_EN.to_le_bytes(), 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::Unmapped));
        write(&mut lpc, ACPI_CNTL, &[ACPI_EN]).unwrap();
        assert_eq!(lpc.pio_write_with_power(0x804, &SLP_EN.to_le_bytes(), 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::AcpiS5));
    }
}
