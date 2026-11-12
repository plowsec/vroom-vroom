//! AMD64 state decoding and bounded four-level long-mode page-table walking.
use crate::{Error, PhysicalMemory, Result};
use serde::{Deserialize, Serialize};
use vroom_hv::VcpuState;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct General {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Segment {
    pub base: u64,
    pub selector: u16,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Table {
    pub base: u64,
    pub limit: u16,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct System {
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
    pub cs: Segment,
    pub gs: Segment,
    pub idt: Table,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Msr {
    pub index: u32,
    pub data: u64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Registers {
    #[serde(default)]
    pub cpu: u32,
    pub regs: General,
    pub sregs: System,
    pub msrs: Vec<Msr>,
}
impl Registers {
    pub fn from_state(state: &VcpuState) -> Result<Self> {
        if state.backend != "kvm-x86_64-v1" || state.raw.len() != 1 || state.raw[0].0 != 0 {
            return Err(Error::Invalid("expected an AMD64 KVM CPU state".into()));
        }
        use vroom_hv::debug::{x86_state as S, X86Register as R};
        let get = |id| {
            state
                .regs
                .iter()
                .find(|(r, _)| *r == id)
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "missing AMD64 register {id:#x}; use a current VMM build"
                    ))
                })
        };
        let reg = |r: R| get(r as u32);
        let mut msrs = Vec::new();
        for (index, id) in [(0xc000_0082, S::LSTAR), (0xc000_0102, S::KERNEL_GS_BASE)] {
            if let Ok(data) = get(id) {
                msrs.push(Msr { index, data });
            }
        }
        Ok(Self {
            cpu: state.index,
            msrs,
            regs: General {
                rax: reg(R::Rax)?,
                rbx: reg(R::Rbx)?,
                rcx: reg(R::Rcx)?,
                rdx: reg(R::Rdx)?,
                rsi: reg(R::Rsi)?,
                rdi: reg(R::Rdi)?,
                rbp: reg(R::Rbp)?,
                rsp: reg(R::Rsp)?,
                r8: reg(R::R8)?,
                r9: reg(R::R9)?,
                r10: reg(R::R10)?,
                r11: reg(R::R11)?,
                r12: reg(R::R12)?,
                r13: reg(R::R13)?,
                r14: reg(R::R14)?,
                r15: reg(R::R15)?,
                rip: reg(R::Rip)?,
                rflags: reg(R::Rflags)?,
            },
            sregs: System {
                cr0: reg(R::Cr0)?,
                cr2: reg(R::Cr2)?,
                cr3: reg(R::Cr3)?,
                cr4: reg(R::Cr4)?,
                efer: reg(R::Efer)?,
                cs: Segment {
                    base: get(S::CS_BASE)?,
                    selector: get(S::CS_SELECTOR)? as u16,
                },
                gs: Segment {
                    base: get(S::GS_BASE)?,
                    selector: get(S::GS_SELECTOR)? as u16,
                },
                idt: Table {
                    base: get(S::IDT_BASE)?,
                    limit: get(S::IDT_LIMIT)? as u16,
                },
            },
        })
    }
    pub fn msr(&self, index: u32) -> Option<u64> {
        self.msrs.iter().find(|r| r.index == index).map(|r| r.data)
    }
}

pub fn canonical(address: u64) -> bool {
    ((address << 16) as i64 >> 16) as u64 == address
}
#[derive(Debug, Serialize)]
pub struct Translation {
    pub virtual_address: u64,
    pub physical: u64,
    pub page_size: u64,
    pub writable: bool,
    pub user: bool,
    pub executable: bool,
    pub entries: Vec<(u64, u64)>,
}
#[derive(Debug)]
pub struct AddressSpace<'a, M: PhysicalMemory + ?Sized> {
    pub memory: &'a M,
    pub registers: &'a Registers,
}
impl<M: PhysicalMemory + ?Sized> AddressSpace<'_, M> {
    pub fn translate(&self, va: u64) -> Result<Translation> {
        let bad = |s: &str| Error::Invalid(format!("translate {va:#x}: {s}"));
        let s = &self.registers.sregs;
        if !canonical(va) {
            return Err(bad("noncanonical address"));
        }
        if s.cr0 & (1 << 31) == 0 || s.cr4 & (1 << 5) == 0 || s.efer & (1 << 10) == 0 {
            return Err(bad("requires paged IA-32e long mode"));
        }
        if s.cr4 & (1 << 12) != 0 {
            return Err(bad("five-level paging is not supported"));
        }
        const MASK: u64 = 0x000f_ffff_ffff_f000;
        let mut table = s.cr3 & MASK;
        let (mut writable, mut user, mut executable) = (true, true, true);
        let mut entries = Vec::new();
        for shift in [39, 30, 21, 12] {
            let at = table + ((va >> shift) & 511) * 8;
            let mut bytes = [0; 8];
            self.memory.read_physical(at, &mut bytes)?;
            let entry = u64::from_le_bytes(bytes);
            entries.push((at, entry));
            if entry & 1 == 0 {
                return Err(bad("page is not present"));
            }
            if entry >> 63 != 0 && s.efer & (1 << 11) == 0 {
                return Err(bad("reserved page-table bits"));
            }
            writable &= entry & 2 != 0;
            user &= entry & 4 != 0;
            executable &= entry >> 63 == 0;
            let large = entry & (1 << 7) != 0;
            if shift == 39 && large {
                return Err(bad("reserved PML4 page-size bit"));
            }
            if shift == 12 || large {
                let page_size = 1u64 << shift;
                // Large-page bit 12 is PAT, not part of the physical address.
                if shift != 12 && entry & MASK & (page_size - 1) & !0x1000 != 0 {
                    return Err(bad("misaligned large page"));
                }
                return Ok(Translation {
                    virtual_address: va,
                    physical: (entry & MASK & !(page_size - 1)) | (va & (page_size - 1)),
                    page_size,
                    writable,
                    user,
                    executable,
                    entries,
                });
            }
            table = entry & MASK;
        }
        unreachable!()
    }
    pub fn read(&self, address: u64, data: &mut [u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let last = address
            .checked_add(data.len() as u64 - 1)
            .ok_or_else(|| Error::Invalid("virtual read overflow".into()))?;
        if !canonical(address) || !canonical(last) || address >> 47 != last >> 47 {
            return Err(Error::Invalid("noncanonical virtual read range".into()));
        }
        let mut offset = 0;
        while offset < data.len() {
            let va = address + offset as u64;
            let t = self.translate(va)?;
            let n = (4096 - (va & 4095) as usize).min(data.len() - offset);
            self.memory
                .read_physical(t.physical, &mut data[offset..offset + n])?;
            offset += n;
        }
        Ok(())
    }
}
impl<M: PhysicalMemory + ?Sized> crate::windows::VirtualMemory for AddressSpace<'_, M> {
    fn read(&self, address: u64, data: &mut [u8]) -> Result<()> {
        AddressSpace::read(self, address, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    struct Memory(RefCell<Vec<u8>>);
    impl PhysicalMemory for Memory {
        fn read_physical(&self, at: u64, out: &mut [u8]) -> Result<()> {
            let m = self.0.borrow();
            let bytes = m
                .get(at as usize..at as usize + out.len())
                .ok_or_else(|| Error::Invalid("outside RAM".into()))?;
            out.copy_from_slice(bytes);
            Ok(())
        }
    }
    fn regs() -> Registers {
        Registers {
            cpu: 0,
            regs: General::default(),
            msrs: vec![],
            sregs: System {
                cr0: 1 << 31,
                cr2: 0,
                cr3: 0x1005,
                cr4: 1 << 5,
                efer: (1 << 10) | (1 << 11),
                cs: Segment {
                    base: 0,
                    selector: 16,
                },
                gs: Segment {
                    base: 0,
                    selector: 0,
                },
                idt: Table { base: 0, limit: 0 },
            },
        }
    }
    fn entry(m: &Memory, at: usize, value: u64) {
        m.0.borrow_mut()[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    #[test]
    fn walks_4k_and_cross_page_reads() {
        let m = Memory(RefCell::new(vec![0; 0x8000]));
        let r = regs();
        for (at, v) in [
            (0x1000, 0x2007),
            (0x2000, 0x3007),
            (0x3000, 0x4007),
            (0x4000, 0x5007),
            (0x4008, 0x6007),
        ] {
            entry(&m, at, v);
        }
        m.0.borrow_mut()[0x5fff..0x6001].copy_from_slice(&[0xab, 0xcd]);
        let s = AddressSpace {
            memory: &m,
            registers: &r,
        };
        assert_eq!(s.translate(0x123).unwrap().physical, 0x5123);
        let mut out = [0; 2];
        s.read(0xfff, &mut out).unwrap();
        assert_eq!(out, [0xab, 0xcd]);
        assert!(s.translate(0x8000_0000_0000).is_err());
        assert!(s.translate(0x2000).is_err());
        assert!(s.read(u64::MAX, &mut out).is_err());
    }
    #[test]
    fn large_pages_permissions_and_reserved_bits() {
        let m = Memory(RefCell::new(vec![0; 0x5000]));
        let mut r = regs();
        entry(&m, 0x1000, 0x2007);
        entry(&m, 0x2000, 0x4000_1081 | (1 << 63));
        let t = AddressSpace {
            memory: &m,
            registers: &r,
        }
        .translate(0x1234)
        .unwrap();
        assert_eq!((t.physical, t.page_size), (0x4000_1234, 1 << 30));
        assert!(!t.executable);
        assert!(!t.user);
        assert!(!t.writable);
        entry(&m, 0x2000, 0x3007);
        entry(&m, 0x3000, 0x20_0083);
        assert_eq!(
            AddressSpace {
                memory: &m,
                registers: &r
            }
            .translate(0x1234)
            .unwrap()
            .physical,
            0x20_1234
        );
        entry(&m, 0x3000, 0x20_2083);
        assert!(AddressSpace {
            memory: &m,
            registers: &r
        }
        .translate(0)
        .is_err());
        r.sregs.cr4 |= 1 << 12;
        assert!(AddressSpace {
            memory: &m,
            registers: &r
        }
        .translate(0)
        .is_err());
    }
    #[test]
    fn rejects_other_architectures_and_truncated_state() {
        assert!(Registers::from_state(&VcpuState::default()).is_err());
        assert!(Registers::from_state(&VcpuState {
            backend: "kvm-x86_64-v1".into(),
            raw: vec![(0, b"{}".to_vec())],
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn pe_reader_keeps_machine_validation_architecture_specific() {
        struct Pe(Vec<u8>);
        impl crate::windows::VirtualMemory for Pe {
            fn read(&self, at: u64, out: &mut [u8]) -> Result<()> {
                out.copy_from_slice(
                    self.0
                        .get(at as usize..at as usize + out.len())
                        .ok_or_else(|| Error::Invalid("outside image".into()))?,
                );
                Ok(())
            }
        }
        let mut p = Pe(vec![0; 8192]);
        p.0[..2].copy_from_slice(b"MZ");
        p.0[60..64].copy_from_slice(&64u32.to_le_bytes());
        p.0[64..68].copy_from_slice(b"PE\0\0");
        p.0[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        p.0[70..72].copy_from_slice(&1u16.to_le_bytes());
        p.0[84..86].copy_from_slice(&240u16.to_le_bytes());
        p.0[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        p.0[144..148].copy_from_slice(&8192u32.to_le_bytes());
        assert!(crate::windows::Image::read_amd64(&p, 0).is_ok());
        assert!(crate::windows::Image::read(&p, 0).is_err());
        p.0[68..70].copy_from_slice(&0xaa64u16.to_le_bytes());
        assert!(crate::windows::Image::read(&p, 0).is_ok());
        assert!(crate::windows::Image::read_amd64(&p, 0).is_err());
    }
}
