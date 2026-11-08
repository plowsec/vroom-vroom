//! Fixed-Q35 SMRAM register model. Mapping ownership belongs to the caller.
//! QEMU v11.1.2 q35.c/q35.h and pinned OVMF SmmAccessPei/SmramInternal define
//! the masks and sequence; see MCH_SMRAM.md for the deliberately narrow subset.

use super::PciConfigError;

/// Device-neutral CPU visibility; this is not an authorization credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MchTsegState { Open, Closed, Locked }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MchSmramState {
    pub tseg: MchTsegState,
    pub default_smbase_locked: bool,
}

pub(super) type Apply<'a> = &'a mut dyn FnMut(MchSmramState) -> Result<MchSmramState, PciConfigError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SmramConfig {
    bytes: [u8; 0xb2],
    default_mask: u8,
    poisoned: bool,
}

impl SmramConfig {
    pub(super) fn new() -> Self {
        let mut bytes = [0; 0xb2];
        bytes[0x50..0x52].copy_from_slice(&0xffffu16.to_le_bytes());
        bytes[0x9d] = 2;
        bytes[0x9e] = 0x38;
        Self { bytes, default_mask: 0xff, poisoned: false }
    }

    pub(super) fn check(&self) -> Result<(), PciConfigError> {
        if self.poisoned { Err(PciConfigError::SmramPoisoned) } else { Ok(()) }
    }

    fn span(off: usize, len: usize) -> Result<(), PciConfigError> {
        let width = match off {
            0x50 | 0x52 | 0xb0 => 2,
            0x9c..=0x9e => 1,
            0xa4 | 0xa8 | 0xac => 4,
            _ => return Err(PciConfigError::UnsupportedRegister),
        };
        if len != width { return Err(PciConfigError::Width); }
        Ok(())
    }

    fn state(&self) -> MchSmramState {
        let closed = self.bytes[0x9d] & 8 != 0 && self.bytes[0x9e] & 1 != 0;
        MchSmramState {
            tseg: if self.bytes[0x9d] & 0x10 != 0 { MchTsegState::Locked }
                else if closed { MchTsegState::Closed } else { MchTsegState::Open },
            default_smbase_locked: self.bytes[0x9c] & 2 != 0,
        }
    }

    fn prepare(&self, off: usize, data: &[u8]) -> Result<Self, PciConfigError> {
        Self::span(off, data.len())?;
        let mut next = self.clone();
        let fixed: Option<&[u8]> = match off {
            0x50 => Some(&[0xff, 0xff]),
            0x52 => Some(&[2, 0]),
            0xa4 | 0xa8 => Some(&[0, 0, 0, 0x20]),
            0xac => Some(&[0, 0, 0x80, 0x1f]),
            0xb0 => Some(&[0, 0x20]),
            _ => None,
        };
        if let Some(expected) = fixed {
            if data != expected { return Err(PciConfigError::SmramProfile); }
            next.bytes[off..off + data.len()].copy_from_slice(data);
            if off == 0x50 { next.bytes[0x50..0x52].copy_from_slice(&8u16.to_le_bytes()); }
        } else {
            match off {
                0x9c => {
                    // Query is recognized AFTER the current write mask. The
                    // first FF queries, the second FF sets the remaining lock.
                    let value = (self.bytes[off] & !self.default_mask) | (data[0] & self.default_mask);
                    match value {
                        0xff => { next.bytes[off] = 1; next.default_mask = 2; }
                        0..=3 => {
                            if value & 2 != 0 { next.bytes[off] = 2; next.default_mask = 0; }
                            else if value == self.bytes[off] { next.bytes[off] = value; }
                            else { return Err(PciConfigError::SmramProfile); }
                        }
                        _ => return Err(PciConfigError::SmramProfile),
                    }
                }
                0x9d => {
                    let mask = if self.bytes[off] & 0x10 != 0 { 0x20 } else { 0x78 };
                    let mut value = (self.bytes[off] & !mask) | (data[0] & mask);
                    if value & 0x10 != 0 { value &= !0x40; }
                    if value & 0x40 != 0 || (value & 0x10 != 0 && (value & 8 == 0 || self.bytes[0x9e] & 1 == 0)) {
                        return Err(PciConfigError::SmramProfile);
                    }
                    next.bytes[off] = value;
                }
                0x9e => {
                    let mask = if self.bytes[0x9d] & 0x10 != 0 { 0 } else { 0x87 };
                    let value = (self.bytes[off] & !mask) | (data[0] & mask);
                    if value & 0x86 != 4 { return Err(PciConfigError::SmramProfile); }
                    next.bytes[off] = value;
                }
                _ => unreachable!("validated register"),
            }
        }
        Ok(next)
    }

    pub(super) fn access(&mut self, off: usize, data: &mut [u8], write: Option<&[u8]>, apply: Option<Apply<'_>>) -> Result<(), PciConfigError> {
        self.check()?;
        let Some(bytes) = write else {
            Self::span(off, data.len())?;
            data.copy_from_slice(&self.bytes[off..off + data.len()]);
            return Ok(());
        };
        let next = self.prepare(off, bytes)?;
        let apply = apply.ok_or(PciConfigError::SmramBackendRequired)?;
        let target = next.state();
        // Keep poison on Err, an unexpected return state, or unwinding panic.
        // A partially applied mapping cannot be represented by old readback.
        self.poisoned = true;
        if apply(target)? != target { return Err(PciConfigError::SmramBackend); }
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(c: &mut SmramConfig, off: usize, data: &[u8]) -> MchSmramState {
        let mut target = None;
        c.access(off, &mut [], Some(data), Some(&mut |s| { target = Some(s); Ok(s) })).unwrap();
        target.unwrap()
    }
    #[test]
    fn fixed_sequence_and_independent_irreversible_locks() {
        let mut c = SmramConfig::new();
        for (off, value) in [(0x50, vec![255, 255]), (0x52, vec![2, 0]),
            (0xb0, vec![0, 0x20]), (0xa4, vec![0, 0, 0, 0x20]),
            (0xa8, vec![0, 0, 0, 0x20]), (0xac, vec![0, 0, 0x80, 0x1f])] {
            assert_eq!(write(&mut c, off, &value).tseg, MchTsegState::Open);
        }
        assert_eq!(&c.bytes[0x50..0x52], &[8, 0]);
        assert_eq!(write(&mut c, 0x9c, &[255]).default_smbase_locked, false);
        assert_eq!(c.bytes[0x9c], 1);
        assert_eq!(write(&mut c, 0x9e, &[0x3d]).tseg, MchTsegState::Open);
        assert_eq!(write(&mut c, 0x9d, &[0xa]).tseg, MchTsegState::Closed);
        assert_eq!(write(&mut c, 0x9e, &[0x3c]).tseg, MchTsegState::Open);
        assert_eq!(write(&mut c, 0x9e, &[0x3d]).tseg, MchTsegState::Closed);
        assert_eq!(write(&mut c, 0x9d, &[0x1a]).tseg, MchTsegState::Locked);
        assert!(write(&mut c, 0x9c, &[255]).default_smbase_locked);
        for value in [0, 255, 2, 1, 0x40] {
            assert!(write(&mut c, 0x9c, &[value]).default_smbase_locked);
            assert_eq!(write(&mut c, 0x9d, &[value]).tseg, MchTsegState::Locked);
            assert_eq!(write(&mut c, 0x9e, &[value]).tseg, MchTsegState::Locked);
            assert_eq!(c.bytes[0x9c], 2); assert_eq!(c.bytes[0x9e], 0x3d);
            assert_eq!(c.bytes[0x9d], 0x1a | (value & 0x20));
        }
    }
    #[test]
    fn rejection_preserves_registers_without_calling_backend() {
        let mut c = SmramConfig::new();
        for (off, value) in [(0x50, vec![8, 0]), (0x52, vec![0, 0]), (0xb0, vec![1, 0x20]),
            (0xac, vec![0, 0, 0, 0x20]), (0x9d, vec![0x40]), (0x9d, vec![0x1a]),
            (0x9c, vec![4]), (0x9e, vec![0xbc]), (0x9e, vec![0x3b]),
            (0x9c, vec![0, 0]), (0x90, vec![0]), (0x51, vec![0])] {
            let before = c.clone();
            assert!(c.access(off, &mut [], Some(&value), Some(&mut |_| panic!("invalid callback"))).is_err());
            assert_eq!(c, before);
        }
        let before = c.clone();
        assert_eq!(c.access(0x50, &mut [], Some(&[255, 255]), None), Err(PciConfigError::SmramBackendRequired));
        assert_eq!(c, before);
    }
    #[test]
    fn failed_apply_does_not_publish_candidate_bytes_and_reads_fail_closed() {
        let mut c = SmramConfig::new();
        let before = c.bytes;
        assert_eq!(c.access(0x50, &mut [], Some(&[255, 255]), Some(&mut |s| {
            assert_eq!(s, MchSmramState { tseg: MchTsegState::Open, default_smbase_locked: false });
            Err(PciConfigError::SmramBackend)
        })), Err(PciConfigError::SmramBackend));
        assert_eq!(c.bytes, before, "unapplied query result must not commit");
        let mut data = [0xa5; 2];
        assert_eq!(c.access(0x50, &mut data, None, None), Err(PciConfigError::SmramPoisoned));
        assert_eq!(data, [0xa5; 2]);
        let mut fresh = SmramConfig::new();
        for (off, len) in [(0x50, 4), (0x51, 1), (0x9c, 2), (0xac, 8), (0x90, 1)] {
            let mut data = vec![0xa5; len];
            assert!(fresh.access(off, &mut data, None, None).is_err());
            assert_eq!(data, vec![0xa5; len]);
        }
    }
}
