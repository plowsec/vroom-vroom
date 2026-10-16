//! Bounded Windows ARM64 .pdata/.xdata unwinding. Unknown encodings stop the walk.
//! https://learn.microsoft.com/cpp/build/arm64-exception-handling
use crate::arm64::Registers;
use crate::windows::{Module, VirtualMemory};
use crate::{Error, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

fn bad(s: impl Into<String>) -> Error {
    Error::Invalid(s.into())
}
fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| bad("unwind address overflow"))
}
fn word(m: &impl VirtualMemory, a: u64) -> Result<u32> {
    let mut b = [0; 4];
    m.read(a, &mut b)?;
    Ok(u32::from_le_bytes(b))
}
#[derive(Debug, Clone)]
enum Op {
    Alloc(u64),
    Save {
        first: usize,
        second: Option<usize>,
        offset: u64,
        post: u64,
    },
    Fp(u64),
    Nop,
    Home,
    Pac,
    Chain,
    End,
}
fn save(first: usize, second: Option<usize>, offset: u64, post: u64) -> Op {
    Op::Save {
        first,
        second,
        offset,
        post,
    }
}
fn sequence(code: &[u8], start: usize) -> Result<Vec<Op>> {
    let mut at = start;
    let mut result = Vec::new();
    let mut previous_pair = None;
    while at < code.len() {
        let b = code[at];
        at += 1;
        let mut next = || {
            let b = code
                .get(at)
                .copied()
                .ok_or_else(|| bad("truncated unwind code"))?;
            at += 1;
            Ok::<u8, Error>(b)
        };
        let op = match b {
            0x00..=0x1f => Op::Alloc((b as u64 & 31) * 16),
            0x20..=0x3f => save(19, Some(20), 0, (b as u64 & 31) * 8),
            0x40..=0x7f => save(29, Some(30), (b as u64 & 63) * 8, 0),
            0x80..=0xbf => save(29, Some(30), 0, ((b as u64 & 63) + 1) * 8),
            0xc0..=0xc7 => Op::Alloc((((b as u64 & 7) << 8) | next()? as u64) * 16),
            0xc8..=0xd3 => {
                let n = next()?;
                let reg = 19 + (((b as usize & 3) << 2) | (n as usize >> 6));
                let post = (0xcc..=0xcf).contains(&b);
                save(
                    reg,
                    if b < 0xd0 { Some(reg + 1) } else { None },
                    if post { 0 } else { (n as u64 & 63) * 8 },
                    if post { ((n as u64 & 63) + 1) * 8 } else { 0 },
                )
            }
            0xd4..=0xd5 => {
                let n = next()?;
                save(
                    19 + (((b as usize & 1) << 3) | (n as usize >> 5)),
                    None,
                    0,
                    ((n as u64 & 31) + 1) * 8,
                )
            }
            0xd6..=0xd7 => {
                let n = next()?;
                save(
                    19 + 2 * (((b as usize & 1) << 2) | (n as usize >> 6)),
                    Some(30),
                    (n as u64 & 63) * 8,
                    0,
                )
            }
            0xd8..=0xde => {
                let n = next()?;
                if (0xda..=0xdb).contains(&b) {
                    Op::Alloc(((n as u64 & 63) + 1) * 8)
                } else if b == 0xde {
                    Op::Alloc(((n as u64 & 31) + 1) * 8)
                } else {
                    Op::Nop
                }
            }
            0xe0 => {
                Op::Alloc(((next()? as u64) << 16 | (next()? as u64) << 8 | next()? as u64) * 16)
            }
            0xe1 => Op::Fp(0),
            0xe2 => Op::Fp(next()? as u64 * 8),
            0xe3 => Op::Nop,
            0xe4 => {
                result.push(Op::End);
                return Ok(result);
            }
            0xe5 => Op::Chain,
            0xe6 => {
                let (reg, offset) =
                    previous_pair.ok_or_else(|| bad("save_next without an integer pair"))?;
                save(reg + 2, Some(reg + 3), offset + 16, 0)
            }
            0xfc => Op::Pac,
            _ => return Err(bad(format!("unsupported unwind opcode {b:#x}"))),
        };
        if let Op::Save {
            first,
            second: Some(second),
            offset,
            ..
        } = op
        {
            if second == first + 1 {
                previous_pair = Some((first, offset));
            }
        } else if !matches!(op, Op::Chain) {
            previous_pair = None;
        }
        result.push(op);
    }
    Err(bad("unterminated unwind sequence"))
}
fn length(ops: &[Op]) -> usize {
    ops.iter()
        .take_while(|op| !matches!(op, Op::Chain | Op::End))
        .count()
}
fn apply(m: &impl VirtualMemory, r: &mut Registers, ops: &[Op], skip: usize) -> Result<()> {
    for op in ops.iter().skip(skip) {
        match *op {
            Op::Alloc(n) => r.sp = add(r.sp, n)?,
            Op::Fp(n) => {
                r.sp = r.x[29]
                    .checked_sub(n)
                    .ok_or_else(|| bad("frame pointer underflow"))?
            }
            Op::Save {
                first,
                second,
                offset,
                post,
            } => {
                if first > 30 || second.is_some_and(|r| r > 30) {
                    return Err(bad("invalid unwind register"));
                }
                let a = add(r.sp, offset)?;
                r.x[first] = m.u64(a)?;
                if let Some(second) = second {
                    r.x[second] = m.u64(add(a, 8)?)?;
                }
                r.sp = add(r.sp, post)?;
            }
            Op::Pac => {
                // Same sign-extension convention as LLDB's AArch64 ABI. TCR
                // supplies the actual VA width. Stripping does not authenticate.
                let upper = r.x[30] & (1 << 55) != 0;
                let sz = if upper {
                    (r.tcr >> 16) & 63
                } else {
                    r.tcr & 63
                };
                if !(16..=39).contains(&sz) {
                    return Err(bad("unsupported PAC address width"));
                }
                let mask = (1u64 << (64 - sz)) - 1;
                r.x[30] = (r.x[30] & mask) | if upper { !mask } else { 0 };
            }
            Op::End => break,
            _ => (),
        }
    }
    r.pc = r.x[30];
    Ok(())
}
fn packed(data: u32) -> Result<Vec<Op>> {
    let int = ((data >> 16) & 15) as usize;
    let fp = ((data >> 13) & 7) as usize;
    let home = (data >> 20) & 1 != 0;
    let cr = (data >> 21) & 3;
    if int > 10 {
        return Err(bad("unsupported packed register/PAC encoding"));
    }
    let intsz = (int as u64 + u64::from(cr == 1)) * 8;
    let fpsz = if fp == 0 { 0 } else { (fp as u64 + 1) * 8 };
    let saves = (intsz + fpsz + if home { 64 } else { 0 } + 15) & !15;
    let frame = ((data >> 23) & 511) as u64 * 16;
    let local = frame
        .checked_sub(saves)
        .ok_or_else(|| bad("packed frame smaller than saves"))?;
    let mut ops = Vec::new();
    if cr == 2 {
        ops.push(Op::Pac);
    }
    let mut allocated = false;
    let mut i = 0;
    while i < int {
        let pair = i + 1 < int;
        let lr = !pair && cr == 1;
        let first = i == 0;
        // A lone x19/lr pair uses a separate allocation instruction.
        if first && lr {
            ops.push(Op::Alloc(saves));
            allocated = true;
        }
        ops.push(save(
            19 + i,
            if pair {
                Some(20 + i)
            } else if lr {
                Some(30)
            } else {
                None
            },
            if first { 0 } else { i as u64 * 8 },
            if first && !allocated { saves } else { 0 },
        ));
        allocated = true;
        i += if pair { 2 } else { 1 };
    }
    if cr == 1 && int.is_multiple_of(2) {
        ops.push(save(
            30,
            None,
            if allocated { intsz - 8 } else { 0 },
            if allocated { 0 } else { saves },
        ));
        allocated = true;
    }
    if fp != 0 {
        for _ in 0..(fp + 1).div_ceil(2) {
            ops.push(if allocated { Op::Nop } else { Op::Alloc(saves) });
            allocated = true;
        }
    }
    if home {
        if !allocated && saves != 0 {
            return Err(bad("unsupported home-only packed frame"));
        }
        ops.extend([Op::Home, Op::Home, Op::Home, Op::Home]);
    }
    if cr >= 2 {
        if local < 16 {
            return Err(bad("packed chained frame too small"));
        }
        if local <= 512 {
            ops.push(save(29, Some(30), 0, local));
        } else {
            if local > 4080 {
                ops.push(Op::Alloc(4080));
                ops.push(Op::Alloc(local - 4080));
            } else {
                ops.push(Op::Alloc(local));
            }
            ops.push(save(29, Some(30), 0, 0));
        }
        ops.push(Op::Fp(0));
    } else if local != 0 {
        if local > 4080 {
            ops.push(Op::Alloc(4080));
            ops.push(Op::Alloc(local - 4080));
        } else {
            ops.push(Op::Alloc(local));
        }
    }
    ops.reverse();
    ops.push(Op::End);
    Ok(ops)
}

#[derive(Debug, Default)]
pub struct Tables(BTreeMap<(u64, u64), Vec<(u32, u32)>>);
impl Tables {
    pub fn clear(&mut self) {
        self.0.clear();
    }
    fn table(
        &mut self,
        m: &impl VirtualMemory,
        module: &Module,
        context: u64,
    ) -> Result<&[(u32, u32)]> {
        if let std::collections::btree_map::Entry::Vacant(entry) =
            self.0.entry((context, module.base))
        {
            let pe = word(m, add(module.base, 0x3c)?)? as u64;
            if !(64..=4096).contains(&pe) {
                return Err(bad("invalid unwind PE"));
            }
            let dir = add(module.base, pe + 24 + 112 + 3 * 8)?;
            let rva = word(m, dir)?;
            let size = word(m, dir + 4)?;
            if size % 8 != 0
                || size > 8 * 1_000_000
                || rva as u64 + size as u64 > module.size as u64
            {
                return Err(bad("invalid .pdata bounds"));
            }
            let mut bytes = vec![0; size as usize];
            m.read(add(module.base, rva as u64)?, &mut bytes)?;
            let entries: Vec<_> = bytes
                .chunks_exact(8)
                .map(|b| {
                    (
                        u32::from_le_bytes(b[..4].try_into().expect("four-byte slice")),
                        u32::from_le_bytes(b[4..].try_into().expect("four-byte slice")),
                    )
                })
                .collect();
            if entries.windows(2).any(|p| p[0].0 >= p[1].0)
                || entries.iter().any(|p| p.0 >= module.size)
            {
                return Err(bad("unsorted/out-of-range .pdata"));
            }
            entry.insert(entries);
        }
        Ok(&self.0[&(context, module.base)])
    }
    pub fn step(
        &mut self,
        m: &impl VirtualMemory,
        module: &Module,
        r: &mut Registers,
        return_address: bool,
    ) -> Result<&'static str> {
        let control =
            r.pc.checked_sub(if return_address { 4 } else { 0 })
                .ok_or_else(|| bad("invalid PC"))?;
        let rva = control
            .checked_sub(module.base)
            .ok_or_else(|| bad("PC below module"))?;
        let context = if module.base >> 63 != 0 {
            0
        } else {
            r.ttbr0 & 0x0000_ffff_ffff_f000
        };
        let table = self.table(m, module, context)?;
        let index = table.partition_point(|(start, _)| *start as u64 <= rva);
        if index == 0 {
            r.pc = r.x[30];
            return Ok("leaf");
        }
        let (start, data) = table[index - 1];
        let offset = (rva - start as u64) / 4;
        if data & 3 == 3 {
            return Err(bad(".pdata link records unsupported"));
        }
        if data & 3 != 0 {
            let len = ((data >> 2) & 2047) as u64;
            if offset >= len {
                r.pc = r.x[30];
                return Ok("leaf");
            }
            let ops = packed(data)?;
            let prolog = length(&ops);
            let epilog: Vec<_> = ops
                .iter()
                .filter(|o| !matches!(o, Op::Fp(_) | Op::Home))
                .cloned()
                .collect();
            if data & 3 == 1 && offset < (prolog as u64) {
                apply(m, r, &ops, prolog - offset as usize)?;
            } else if data & 3 == 1
                && len >= epilog.len() as u64
                && offset >= len - epilog.len() as u64
            {
                apply(
                    m,
                    r,
                    &epilog,
                    (offset - (len - epilog.len() as u64)) as usize,
                )?;
            } else {
                apply(m, r, &ops, 0)?;
            }
            return Ok("packed");
        }
        if data as u64 + 4 > module.size as u64 {
            return Err(bad(".xdata outside module"));
        }
        let x = add(module.base, data as u64)?;
        let header = word(m, x)?;
        let len = (header & 0x3ffff) as u64;
        if offset >= len {
            r.pc = r.x[30];
            return Ok("leaf");
        }
        if (header >> 18) & 3 != 0 {
            return Err(bad("unsupported .xdata version"));
        }
        let single = header & (1 << 21) != 0;
        let mut epilogs = (header >> 22) & 31;
        let mut words = header >> 27;
        let mut cursor = 4u64;
        if epilogs == 0 && words == 0 {
            let ext = word(m, x + 4)?;
            epilogs = ext & 65535;
            words = (ext >> 16) & 255;
            cursor += 4;
        }
        let scopes = if single { 0 } else { epilogs };
        let total = cursor + scopes as u64 * 4 + words as u64 * 4;
        if data as u64 + total > module.size as u64 || total > 300_000 {
            return Err(bad(".xdata bounds exceeded"));
        }
        let mut code = vec![0; words as usize * 4];
        m.read(x + cursor + scopes as u64 * 4, &mut code)?;
        let ops = sequence(&code, 0)?;
        if offset < (length(&ops) as u64) {
            apply(m, r, &ops, length(&ops) - offset as usize)?;
            return Ok("xdata prolog");
        }
        if single {
            let epilog = sequence(&code, epilogs as usize)?;
            let count = length(&epilog) + 1;
            if len >= count as u64 && offset >= len - count as u64 {
                apply(m, r, &epilog, (offset - (len - count as u64)) as usize)?;
                return Ok("xdata epilog");
            }
        } else {
            for i in 0..scopes {
                let scope = word(m, x + cursor + i as u64 * 4)?;
                let start = (scope & 0x3ffff) as u64;
                if start > offset {
                    break;
                }
                let epilog = sequence(&code, (scope >> 22) as usize)?;
                if offset - start <= length(&epilog) as u64 {
                    apply(m, r, &epilog, (offset - start) as usize)?;
                    return Ok("xdata epilog");
                }
            }
        }
        apply(m, r, &ops, 0)?;
        Ok("xdata")
    }
}
#[derive(Debug, Serialize)]
pub struct Frame {
    pub pc: u64,
    pub sp: u64,
    pub module: String,
    pub symbol: Option<String>,
    pub method: String,
}
#[derive(Debug, Serialize)]
pub struct Stack {
    pub frames: Vec<Frame>,
    pub stop: Option<String>,
}
pub fn walk(
    m: &impl VirtualMemory,
    registers: &Registers,
    modules: &[Module],
    tables: &mut Tables,
) -> Stack {
    let mut regs = registers.clone();
    let mut seen = HashSet::new();
    let mut frames = Vec::new();
    let mut stop = None;
    for i in 0..128 {
        if regs.pc == 0 {
            break;
        }
        if regs.pc & 3 != 0 || !seen.insert((regs.pc, regs.sp)) {
            stop = Some("unaligned PC or cyclic stack".into());
            break;
        }
        let module = modules
            .iter()
            .find(|m| regs.pc >= m.base && regs.pc - m.base < m.size as u64);
        let Some(module) = module else {
            stop = Some(format!("no loaded module at {:#x}", regs.pc));
            break;
        };
        let mut frame = Frame {
            pc: regs.pc,
            sp: regs.sp,
            module: module.name.clone(),
            symbol: None,
            method: String::new(),
        };
        let old = regs.sp;
        match tables.step(m, module, &mut regs, i != 0) {
            Ok(method) => frame.method = method.into(),
            Err(e) => {
                frame.method = "stop".into();
                frames.push(frame);
                stop = Some(e.to_string());
                break;
            }
        }
        frames.push(frame);
        if regs.sp < old || regs.sp - old > 16 * 1024 * 1024 || regs.sp & 15 != 0 {
            stop = Some("invalid caller stack pointer".into());
            break;
        }
        if i == 127 {
            stop = Some("frame limit reached".into());
        }
    }
    Stack { frames, stop }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory(Vec<u8>);
    impl VirtualMemory for Memory {
        fn read(&self, a: u64, b: &mut [u8]) -> Result<()> {
            b.copy_from_slice(
                self.0
                    .get(a as usize..a as usize + b.len())
                    .ok_or_else(|| bad("unmapped"))?,
            );
            Ok(())
        }
    }
    fn regs() -> Registers {
        Registers {
            cpu: 0,
            x: [0; 31],
            pc: 0,
            sp: 0x1000,
            pstate: 5,
            ttbr0: 0,
            ttbr1: 0,
            tcr: 0,
            sctlr: 0,
            vbar: 0,
        }
    }
    fn put(m: &mut Memory, a: usize, value: u64) {
        m.0[a..a + 8].copy_from_slice(&value.to_le_bytes());
    }
    #[test]
    fn xdata_partial_prolog_and_epilog_restore_only_executed_saves() {
        let mut m = Memory(vec![0; 0x3000]);
        put(&mut m, 0x1000, 0x2000);
        put(&mut m, 0x1008, 0x8000);
        // stp fp,lr,[sp,#-32]!; mov fp,sp; ...; mov sp,fp; ldp fp,lr,[sp],#32; ret
        let ops = sequence(&[0xe1, 0x83, 0xe4], 0).unwrap();
        let mut r = regs();
        r.x[29] = 0x1000;
        r.x[30] = 0x9000;
        apply(&m, &mut r, &ops, 2).unwrap();
        assert_eq!((r.sp, r.pc), (0x1000, 0x9000));
        let mut r = regs();
        r.x[29] = 0x1000;
        apply(&m, &mut r, &ops, 1).unwrap();
        assert_eq!((r.sp, r.pc, r.x[29]), (0x1020, 0x8000, 0x2000));
        let mut r = regs();
        r.x[29] = 0x1000;
        apply(&m, &mut r, &ops, 0).unwrap();
        assert_eq!((r.sp, r.pc), (0x1020, 0x8000));
    }
    #[test]
    fn microsoft_packed_example_restores_2080_byte_frame() {
        let ops = packed(0x416101ed).unwrap();
        assert_eq!(length(&ops), 4);
        let mut m = Memory(vec![0; 0x4000]);
        put(&mut m, 0x1000, 0x3000);
        put(&mut m, 0x1008, 0x8000);
        put(&mut m, 0x1810, 0x19);
        let mut r = regs();
        r.x[29] = 0x1000;
        apply(&m, &mut r, &ops, 0).unwrap();
        assert_eq!(
            (r.sp, r.pc, r.x[29], r.x[19]),
            (0x1820, 0x8000, 0x3000, 0x19)
        );
    }
    #[test]
    fn malformed_and_unknown_metadata_fail_closed() {
        assert!(sequence(&[0xe0, 0], 0).is_err());
        assert!(sequence(&[0xe6, 0xe4], 0).is_err());
        assert!(sequence(&[0xea, 0xe4], 0).is_err());
        assert!(sequence(&[0xe3], 0).is_err());
        assert!(packed(1 | (15 << 16)).is_err());
    }
}
