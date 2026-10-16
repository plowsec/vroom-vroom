//! A bounded stage-1 walker for Windows ARM64's 4 KiB translation granule.
//! Unsupported translation formats are errors, never guessed addresses.
use crate::{Error, PhysicalMemory, Result};
use serde::Serialize;
use vroom_hv::VcpuState;

#[derive(Debug, Clone, Serialize)]
pub struct Registers {
    pub cpu: u32,
    pub x: [u64; 31],
    pub pc: u64,
    pub sp: u64,
    pub pstate: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub tcr: u64,
    pub sctlr: u64,
    pub vbar: u64,
}

impl Registers {
    pub fn from_hvf(state: &VcpuState) -> Result<Self> {
        if state.backend != "hvf" {
            return Err(Error::Invalid(format!(
                "unsupported register backend {}",
                state.backend
            )));
        }
        let reg = |id| {
            state
                .regs
                .iter()
                .find(|r| r.0 == id)
                .map(|r| r.1)
                .ok_or_else(|| Error::Invalid(format!("missing register {id:#x}")))
        };
        let mut x = [0; 31];
        for (i, v) in x.iter_mut().enumerate() {
            *v = reg(i as u32)?;
        }
        let pstate = reg(34)?;
        if pstate & 0x1c != 4 && pstate & 0x1f != 0 {
            return Err(Error::Invalid(
                "only EL0/EL1 AArch64 register contexts are supported".into(),
            ));
        }
        Ok(Self {
            cpu: state.index,
            x,
            pc: reg(31)?,
            pstate,
            sp: reg(0x10000 | if pstate & 1 == 0 { 0xc208 } else { 0xe208 })?,
            ttbr0: reg(0x1c100)?,
            ttbr1: reg(0x1c101)?,
            tcr: reg(0x1c102)?,
            sctlr: reg(0x1c080)?,
            vbar: reg(0x1c600)?,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Translation {
    pub physical: u64,
    pub page_size: u64,
    pub descriptor: u64,
    pub level: u8,
}

#[derive(Debug)]
pub struct AddressSpace<'a, M: PhysicalMemory + ?Sized> {
    pub memory: &'a M,
    pub registers: &'a Registers,
}

impl<M: PhysicalMemory + ?Sized> AddressSpace<'_, M> {
    pub fn translate(&self, address: u64) -> Result<Translation> {
        let invalid = |why: &str| Error::Invalid(format!("VA {address:#018x}: {why}"));
        let r = self.registers;
        if r.sctlr & 1 == 0 {
            return Ok(Translation {
                physical: address,
                page_size: 4096,
                descriptor: 0,
                level: 3,
            });
        }
        let ips = (r.tcr >> 32) & 7;
        let physical_bits = match ips {
            0 => 32,
            1 => 36,
            2 => 40,
            3 => 42,
            4 => 44,
            5 => 48,
            _ => {
                return Err(invalid(
                    "52-bit/reserved physical address format is not supported",
                ))
            }
        };
        if r.tcr & (1 << 59) != 0 {
            return Err(invalid("LPA2 is not supported"));
        }
        // Bit 55 selects the upper/lower range even when a top-byte tag exists.
        let upper = address & (1 << 55) != 0;
        let (sz, root, granule, epd, tbi) = if upper {
            (
                (r.tcr >> 16) & 63,
                r.ttbr1,
                (r.tcr >> 30) & 3,
                (r.tcr >> 23) & 1,
                (r.tcr >> 38) & 1,
            )
        } else {
            (
                r.tcr & 63,
                r.ttbr0,
                (r.tcr >> 14) & 3,
                (r.tcr >> 7) & 1,
                (r.tcr >> 37) & 1,
            )
        };
        if epd != 0 {
            return Err(invalid("translation table walk disabled by TCR.EPD"));
        }
        if granule != if upper { 2 } else { 0 } {
            return Err(invalid("only the 4 KiB translation granule is supported"));
        }
        if !(16..=39).contains(&sz) {
            return Err(invalid("unsupported virtual address size"));
        }
        let bits = 64 - sz;
        let va_mask = (1u64 << bits) - 1;
        let normalized = if tbi != 0 {
            if upper {
                address | 0xff00_0000_0000_0000
            } else {
                address & 0x00ff_ffff_ffff_ffff
            }
        } else {
            address
        };
        if normalized & !va_mask != if upper { !va_mask } else { 0 } {
            return Err(invalid("noncanonical virtual address"));
        }
        let pa_mask = (1u64 << physical_bits) - 1;
        let levels = (bits - 12).div_ceil(9);
        // A shortened first-level table can share a 4 KiB page. Windows
        // uses 47-bit VAs and places TTBR1 at a 2 KiB boundary.
        let root_alignment = 1u64 << (bits - 12 - 9 * (levels - 1) + 3);
        let mut table = root & pa_mask & !(root_alignment - 1);
        for level in (4 - levels)..4 {
            let shift = 12 + 9 * (3 - level);
            let index_mask = if level == 4 - levels {
                root_alignment / 8 - 1
            } else {
                511
            };
            let index = (normalized >> shift) & index_mask;
            let mut data = [0; 8];
            self.memory.read_physical(table + index * 8, &mut data)?;
            let desc = u64::from_le_bytes(data);
            if desc & 1 == 0 {
                return Err(invalid("unmapped page (invalid descriptor)"));
            }
            // Address bits beyond IPS must not be silently truncated.
            if desc & 0x0000_ffff_ffff_f000 & !pa_mask != 0 {
                return Err(invalid("descriptor exceeds physical address size"));
            }
            if level < 3 && desc & 2 != 0 {
                table = desc & pa_mask & !4095;
                continue;
            }
            if level == 0 || (level == 3 && desc & 2 == 0) {
                return Err(invalid("reserved block/page descriptor"));
            }
            let size = 1u64 << shift;
            if desc & (size - 1) & !4095 != 0 {
                return Err(invalid("misaligned block descriptor"));
            }
            return Ok(Translation {
                physical: (desc & pa_mask & !(size - 1)) | (normalized & (size - 1)),
                page_size: size,
                descriptor: desc,
                level: level as u8,
            });
        }
        Err(invalid("no leaf descriptor"))
    }

    pub fn read(&self, address: u64, data: &mut [u8]) -> Result<()> {
        address
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::Invalid("virtual read overflows".into()))?;
        let mut done = 0;
        while done < data.len() {
            let t = self.translate(address + done as u64)?;
            let remaining = t.page_size - (t.physical & (t.page_size - 1));
            let n = remaining.min((data.len() - done) as u64) as usize;
            self.memory
                .read_physical(t.physical, &mut data[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    pub fn u64(&self, address: u64) -> Result<u64> {
        let mut b = [0; 8];
        self.read(address, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Ram(Vec<u8>);
    impl PhysicalMemory for Ram {
        fn read_physical(&self, a: u64, b: &mut [u8]) -> Result<()> {
            let s = self
                .0
                .get(a as usize..a as usize + b.len())
                .ok_or_else(|| Error::Invalid("outside RAM".into()))?;
            b.copy_from_slice(s);
            Ok(())
        }
    }
    fn entry(m: &mut Ram, a: usize, v: u64) {
        m.0[a..a + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn fixture() -> (Ram, Registers) {
        let mut m = Ram(vec![0; 0x10000]);
        entry(&mut m, 0x1000, 0x2003);
        entry(&mut m, 0x2000, 0x3003);
        entry(&mut m, 0x3000, 0x4003);
        entry(&mut m, 0x4000, 0x8003);
        entry(&mut m, 0x4008, 0xa003);
        let r = Registers {
            cpu: 0,
            x: [0; 31],
            pc: 0,
            sp: 0,
            pstate: 5,
            ttbr0: 0xabcd000000001000,
            ttbr1: 0x1000,
            tcr: 16 | (16 << 16) | (2 << 30) | (5 << 32),
            sctlr: 1,
            vbar: 0,
        };
        (m, r)
    }
    #[test]
    fn noncontiguous_pages_and_asid() {
        let (mut m, r) = fixture();
        m.0[0x8ffe..0x9000].copy_from_slice(&[1, 2]);
        m.0[0xa000..0xa002].copy_from_slice(&[3, 4]);
        let a = AddressSpace {
            memory: &m,
            registers: &r,
        };
        let mut b = [0; 4];
        a.read(0xffe, &mut b).unwrap();
        assert_eq!(b, [1, 2, 3, 4]);
        assert_eq!(a.translate(0x1001).unwrap().physical, 0xa001);
        assert!(a.translate(0x2000).is_err());
        assert!(a.read(u64::MAX, &mut b).is_err());
    }
    #[test]
    fn kernel_range_tags_and_rejected_modes() {
        let (mut m, mut r) = fixture();
        // Upper 48-bit VA indices: 511,0,0,0.
        entry(&mut m, 0x1000 + 511 * 8, 0x2003);
        let va = 0xffff_ff80_0000_0000;
        assert_eq!(
            AddressSpace {
                memory: &m,
                registers: &r
            }
            .translate(va)
            .unwrap()
            .physical,
            0x8000
        );
        r.tcr |= 1 << 38;
        assert_eq!(
            AddressSpace {
                memory: &m,
                registers: &r
            }
            .translate(va & 0x12ff_ffff_ffff_ffff)
            .unwrap()
            .physical,
            0x8000
        );
        r.tcr |= 1 << 14;
        assert!(AddressSpace {
            memory: &m,
            registers: &r
        }
        .translate(0)
        .is_err());
        r.tcr &= !(1 << 14);
        r.tcr |= 1 << 23;
        assert!(AddressSpace {
            memory: &m,
            registers: &r
        }
        .translate(va)
        .is_err());
    }
    #[test]
    fn blocks_canonical_and_reserved_descriptors() {
        let (mut m, r) = fixture();
        entry(&mut m, 0x3000, 0x200001);
        assert_eq!(
            AddressSpace {
                memory: &m,
                registers: &r
            }
            .translate(0x1234)
            .unwrap()
            .physical,
            0x201234
        );
        entry(&mut m, 0x3000, 0x201001);
        assert!(AddressSpace {
            memory: &m,
            registers: &r
        }
        .translate(0)
        .is_err());
        assert!(AddressSpace {
            memory: &m,
            registers: &r
        }
        .translate(1 << 48)
        .is_err());
        entry(&mut m, 0x1000, 1);
        assert!(AddressSpace {
            memory: &m,
            registers: &r
        }
        .translate(0)
        .is_err());
    }

    #[test]
    fn windows_47_bit_root_at_half_page() {
        let (mut m, mut r) = fixture();
        r.tcr = (r.tcr & !(63 << 16)) | (17 << 16);
        r.ttbr1 = 0x1800;
        entry(&mut m, 0x1800 + 240 * 8, 0x2003);
        let va = 0xffff_f800_0000_0000;
        assert_eq!(
            AddressSpace {
                memory: &m,
                registers: &r
            }
            .translate(va)
            .unwrap()
            .physical,
            0x8000
        );
    }
}
