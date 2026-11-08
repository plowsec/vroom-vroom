//! In-memory PC flash command subset used by OVMF's QemuFlash driver.
//!
//! The PC geometry is a byte-wide bank with 4 KiB sectors, unlike the ARM
//! flash in `pflash`. Images must be complete, sector-aligned, and <=4 MiB.
//! Supported commands are byte program (0x10), sector erase (0x20/0xd0),
//! clear/read status (0x50/0x70), and read array (0xff). This is not a full
//! CFI model: ID/query, buffered program, locking and timing are unsupported.
//!
//! [`PcFlashAccess`] is caller-provided emulation context, NOT authorization
//! or proof of SMM isolation. A future integration must derive it from the
//! hypervisor and enforce isolated SMRAM mappings. There is deliberately no
//! `BusDevice` implementation, persistence, snapshot or board registration.
//!
//! Protocol sources: QEMU `hw/i386/pc_sysfw.c`, `hw/block/pflash_cfi01.c`, and
//! EDK2 `OvmfPkg/QemuFlashFvbServicesRuntimeDxe/QemuFlash.c`.

use std::ops::Range;

pub const SECTOR_SIZE: usize = 4096;
pub const MAX_SIZE: usize = 4 << 20;
const READY: u8 = 0x80;

/// An emulated access attribute supplied by trusted host code. Naming a
/// context `SystemManagement` does not establish that a guest is in SMM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcFlashAccess {
    Normal,
    SystemManagement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PcFlashError {
    #[error("PC flash requires a nonempty, 4 KiB-aligned image no larger than 4 MiB")]
    ImageSize,
    #[error("unsupported PC flash access width")]
    Width,
    #[error("PC flash access is out of bounds")]
    Bounds,
    #[error("normal-context PC flash writes are denied")]
    WriteProtected,
    #[error("unsupported PC flash command")]
    Command,
    #[error("invalid PC flash command sequence")]
    Sequence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Array,
    Status,
    Program,
    EraseConfirm,
}

pub struct PcFlash {
    data: Vec<u8>,
    mode: Mode,
    status: u8,
}

impl std::fmt::Debug for PcFlash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcFlash").field("size", &self.data.len())
            .field("mode", &self.mode).field("status", &self.status).finish()
    }
}

impl PcFlash {
    /// Own an exact image; do not resize, pad, copy or persist its contents.
    pub fn new(contents: Vec<u8>) -> Result<Self, PcFlashError> {
        if contents.is_empty() || contents.len() > MAX_SIZE
            || !contents.len().is_multiple_of(SECTOR_SIZE) {
            return Err(PcFlashError::ImageSize);
        }
        Ok(Self { data: contents, mode: Mode::Array, status: READY })
    }

    fn range(&self, offset: u64, len: usize) -> Result<Range<usize>, PcFlashError> {
        let start = usize::try_from(offset).map_err(|_| PcFlashError::Bounds)?;
        let end = start.checked_add(len).filter(|&end| end <= self.data.len())
            .ok_or(PcFlashError::Bounds)?;
        Ok(start..end)
    }

    /// Array reads accept scalar widths 1/2/4/8. Status/probe reads are byte
    /// accesses only. Normal-context reads always see array bytes, even
    /// while the emulated management context is in a command/status mode.
    /// A rejected read leaves the caller's buffer unchanged.
    pub fn read(&self, access: PcFlashAccess, offset: u64, out: &mut [u8]) -> Result<(), PcFlashError> {
        if !matches!(out.len(), 1 | 2 | 4 | 8) { return Err(PcFlashError::Width); }
        let range = self.range(offset, out.len())?;
        if access == PcFlashAccess::Normal || self.mode == Mode::Array {
            out.copy_from_slice(&self.data[range]);
        } else {
            if out.len() != 1 { return Err(PcFlashError::Width); }
            out[0] = self.status;
        }
        Ok(())
    }

    /// Handle one byte command or program-data write. A rejected access
    /// does not change contents, status or the pending command.
    ///
    /// Like QEMU, erase changes the addressed sector on 0x20; 0xd0 ends
    /// that command, and 0xff returns to array mode without undoing it.
    /// A later rejected confirmation cannot undo an earlier valid erase.
    pub fn write(&mut self, access: PcFlashAccess, offset: u64, data: &[u8]) -> Result<(), PcFlashError> {
        if access != PcFlashAccess::SystemManagement { return Err(PcFlashError::WriteProtected); }
        if data.len() != 1 { return Err(PcFlashError::Width); }
        let offset = self.range(offset, 1)?.start;
        let value = data[0];
        match self.mode {
            Mode::Program => {
                // Match QEMU byte-program behavior, including writing 0xff.
                // Data is not decoded as a command in this cycle.
                self.data[offset] = value;
                self.status |= READY;
                self.mode = Mode::Status;
            }
            Mode::EraseConfirm => match value {
                0xd0 => self.mode = Mode::Status,
                0xff => self.mode = Mode::Array,
                _ => return Err(PcFlashError::Sequence),
            },
            Mode::Array | Mode::Status => match value {
                0x10 => self.mode = Mode::Program,
                0x20 => {
                    let start = offset & !(SECTOR_SIZE - 1);
                    self.data[start..start + SECTOR_SIZE].fill(0xff);
                    self.status |= READY;
                    self.mode = Mode::EraseConfirm;
                }
                0x50 => {
                    // QEMU clear-status also returns to read-array mode.
                    self.status = 0;
                    self.mode = Mode::Array;
                }
                0x70 => self.mode = Mode::Status,
                0xff => self.mode = Mode::Array,
                _ => return Err(PcFlashError::Command),
            },
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PcFlashAccess::{Normal, SystemManagement as Smm};

    fn read(flash: &PcFlash, access: PcFlashAccess, offset: u64) -> u8 {
        let mut byte = [0];
        flash.read(access, offset, &mut byte).unwrap();
        byte[0]
    }

    fn state(flash: &PcFlash) -> (Mode, u8, Vec<u8>) {
        (flash.mode, flash.status, flash.data.clone())
    }

    #[test]
    fn selected_pc_vars_geometry_is_exact_and_bounded() {
        let image = vec![0x5a; 0x84000]; // selected OVMF VARS: 132 sectors
        let flash = PcFlash::new(image.clone()).unwrap();
        assert_eq!(flash.data, image);
        assert_eq!(flash.data.len() / SECTOR_SIZE, 132);
        assert!(PcFlash::new(vec![0xff; MAX_SIZE]).is_ok());
        for size in [0, 1, SECTOR_SIZE - 1, SECTOR_SIZE + 1, MAX_SIZE + SECTOR_SIZE] {
            assert_eq!(PcFlash::new(vec![0xff; size]).unwrap_err(), PcFlashError::ImageSize);
        }
    }

    #[test]
    fn ovmf_flash_probe_preserves_the_original_byte_and_identifies_writable_flash() {
        // QemuFlashDetected: choose a byte other than 0x50/0x70/0x00.
        let mut flash = PcFlash::new(vec![0x5a; SECTOR_SIZE]).unwrap();
        let original = read(&flash, Smm, 0);
        flash.write(Smm, 0, &[0x50]).unwrap();
        assert_eq!(read(&flash, Smm, 0), original); // not RAM
        flash.write(Smm, 0, &[0x70]).unwrap();
        assert_eq!(read(&flash, Smm, 0), 0); // neither ROM nor RAM
        flash.write(Smm, 0, &[0x10]).unwrap();
        flash.write(Smm, 0, &[original]).unwrap();
        flash.write(Smm, 0, &[0x70]).unwrap();
        assert_eq!(read(&flash, Smm, 0), READY); // no programming-error bit
        flash.write(Smm, 0, &[0xff]).unwrap();
        assert_eq!(flash.data, vec![original; SECTOR_SIZE]);
        assert_eq!(read(&flash, Smm, 0), original);
    }

    #[test]
    fn ovmf_program_loop_is_byte_wide_and_data_is_not_a_command() {
        let mut flash = PcFlash::new(vec![0; SECTOR_SIZE]).unwrap();
        let bytes = [0x10, 0x20, 0x50, 0x70, 0xd0, 0xff];
        for (offset, byte) in bytes.iter().enumerate() {
            flash.write(Smm, offset as u64, &[0x10]).unwrap();
            flash.write(Smm, offset as u64, &[*byte]).unwrap();
            assert_eq!(read(&flash, Smm, offset as u64), READY);
        }
        flash.write(Smm, (bytes.len() - 1) as u64, &[0xff]).unwrap();
        assert_eq!(&flash.data[..bytes.len()], &bytes);
        assert!(flash.data[bytes.len()..].iter().all(|&b| b == 0));
        for width in [1, 2, 4, 8] {
            let mut out = vec![0xa5; width];
            flash.read(Smm, 0, &mut out).unwrap();
            assert_eq!(out, flash.data[..width]);
        }
    }

    #[test]
    fn ovmf_erase_affects_only_one_four_kib_sector_and_confirms() {
        let mut flash = PcFlash::new(vec![0x19; 3 * SECTOR_SIZE]).unwrap();
        let offset = (SECTOR_SIZE + 17) as u64;
        flash.write(Smm, offset, &[0x20]).unwrap();
        assert_eq!(read(&flash, Smm, offset), READY);
        assert!(flash.data[..SECTOR_SIZE].iter().all(|&b| b == 0x19));
        assert!(flash.data[SECTOR_SIZE..2 * SECTOR_SIZE].iter().all(|&b| b == 0xff));
        assert!(flash.data[2 * SECTOR_SIZE..].iter().all(|&b| b == 0x19));
        flash.write(Smm, offset, &[0xd0]).unwrap();
        assert_eq!(read(&flash, Smm, offset), READY);
        flash.write(Smm, offset, &[0xff]).unwrap();
        assert_eq!(read(&flash, Smm, offset), 0xff);
    }

    #[test]
    fn normal_context_reads_array_and_all_writes_leave_every_mode_unchanged() {
        for command in [None, Some(0x70), Some(0x10), Some(0x20)] {
            let mut flash = PcFlash::new(vec![0x35; SECTOR_SIZE]).unwrap();
            if let Some(command) = command { flash.write(Smm, 0, &[command]).unwrap(); }
            let prior = state(&flash);
            for value in 0..=255 {
                assert_eq!(flash.write(Normal, 0, &[value]), Err(PcFlashError::WriteProtected));
                assert_eq!(state(&flash), prior);
            }
            for width in [1, 2, 4, 8] {
                let mut out = vec![0; width];
                flash.read(Normal, 0, &mut out).unwrap();
                assert_eq!(out, flash.data[..width]);
                assert_eq!(state(&flash), prior);
            }
        }
    }

    #[test]
    fn invalid_widths_and_ranges_do_not_consume_commands_or_modify_read_buffers() {
        for command in [None, Some(0x70), Some(0x10), Some(0x20)] {
            let mut flash = PcFlash::new(vec![0x35; SECTOR_SIZE]).unwrap();
            if let Some(command) = command { flash.write(Smm, 0, &[command]).unwrap(); }
            let prior = state(&flash);
            for width in [0, 2, 3, 4, 8, 9] {
                assert_eq!(flash.write(Smm, 0, &vec![0xff; width]), Err(PcFlashError::Width));
                assert_eq!(state(&flash), prior);
            }
            for offset in [SECTOR_SIZE as u64, u64::MAX] {
                assert_eq!(flash.write(Smm, offset, &[0xff]), Err(PcFlashError::Bounds));
                assert_eq!(state(&flash), prior);
            }
            for access in [Normal, Smm] {
                for (offset, width) in [(0, 0), (0, 3), (0, 9), (SECTOR_SIZE as u64, 1),
                    (SECTOR_SIZE as u64 - 1, 2), (u64::MAX, 8)] {
                    let mut out = vec![0xa5; width];
                    assert!(flash.read(access, offset, &mut out).is_err());
                    assert_eq!(out, vec![0xa5; width]);
                    assert_eq!(state(&flash), prior);
                }
            }
            if flash.mode != Mode::Array {
                let mut out = [0xa5; 2];
                assert_eq!(flash.read(Smm, 0, &mut out), Err(PcFlashError::Width));
                assert_eq!(out, [0xa5; 2]);
            }
        }
    }

    #[test]
    fn rejected_commands_and_confirmation_leave_state_unchanged() {
        let mut flash = PcFlash::new(vec![0x35; 2 * SECTOR_SIZE]).unwrap();
        for mode in [Mode::Array, Mode::Status] {
            flash.mode = mode;
            let prior = state(&flash);
            for command in [0, 0x40, 0x60, 0x90, 0x98, 0xd0, 0xe8, 0xf0] {
                assert_eq!(flash.write(Smm, 0, &[command]), Err(PcFlashError::Command));
                assert_eq!(state(&flash), prior);
            }
        }
        flash.write(Smm, 0, &[0x20]).unwrap();
        let erased = state(&flash);
        for command in [0, 0x10, 0x20, 0x50, 0x70] {
            assert_eq!(flash.write(Smm, SECTOR_SIZE as u64, &[command]), Err(PcFlashError::Sequence));
            assert_eq!(state(&flash), erased); // no additional sector erased
        }
        flash.write(Smm, 0, &[0xff]).unwrap(); // exit without undoing valid erase
        assert_eq!(flash.mode, Mode::Array);
        assert_eq!(flash.data, erased.2);
    }
}
