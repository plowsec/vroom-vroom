//! Bounded Windows ARM64 .pdata/.xdata unwinding. Unknown encodings stop the walk.
//! https://learn.microsoft.com/cpp/build/arm64-exception-handling
//! Context layouts: https://github.com/dotnet/runtime/blob/main/src/coreclr/unwinder/arm64/unwinder.cpp
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
    AllocVl(u64),
    RestoreVl(u64),
    Save {
        first: usize,
        second: Option<usize>,
        offset: u64,
        post: u64,
    },
    VectorSave {
        first: usize,
        pair: bool,
        width: u64,
        offset: u64,
        post: u64,
    },
    Next,
    Custom(u8),
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
                let pair = b <= 0xdb;
                let post = (0xda..=0xdb).contains(&b) || b == 0xde;
                let mask = if b == 0xde { 31 } else { 63 };
                let first = 8 + if b == 0xde {
                    n as usize >> 5
                } else {
                    ((b as usize & 1) << 2) | (n as usize >> 6)
                };
                if first + usize::from(pair) > 15 {
                    return Err(bad("invalid FP unwind register"));
                }
                Op::VectorSave {
                    first,
                    pair,
                    width: 8,
                    offset: if post { 0 } else { (n as u64 & mask) * 8 },
                    post: if post { ((n as u64 & mask) + 1) * 8 } else { 0 },
                }
            }
            0xdf => Op::AllocVl(next()? as u64),
            0xe0 => {
                Op::Alloc(((next()? as u64) << 16 | (next()? as u64) << 8 | next()? as u64) * 16)
            }
            0xe1 => Op::Fp(0),
            0xe2 => Op::Fp(next()? as u64 * 8),
            0xe3 => Op::Nop,
            0xe4 => {
                result.push(Op::End);
                // Unwind codes are the reverse of prolog instructions: a run
                // of save_next codes precedes the pair they extend.
                for i in (0..result.len() - 1).rev() {
                    if matches!(result[i], Op::Next) {
                        result[i] = match result[i + 1] {
                            Op::Save {
                                first,
                                second: Some(second),
                                offset,
                                ..
                            } if second == first + 1 && first + 3 <= 30 => {
                                save(first + 2, Some(first + 3), offset + 16, 0)
                            }
                            Op::VectorSave {
                                first,
                                pair: true,
                                width,
                                offset,
                                ..
                            } if first + 3 <= 31 => Op::VectorSave {
                                first: first + 2,
                                pair: true,
                                width,
                                offset: offset + 2 * width,
                                post: 0,
                            },
                            _ => return Err(bad("save_next without a following register pair")),
                        };
                    }
                }
                return Ok(result);
            }
            0xe5 => Op::Chain,
            0xe6 => Op::Next,
            0xe7 => {
                let reg = next()?;
                let encoded = next()?;
                let kind = encoded >> 6;
                if reg & 0x80 != 0 {
                    return Err(bad("reserved save_any encoding"));
                }
                if kind == 3 {
                    let number = reg & 15;
                    let op = if reg & 16 == 0 || number >= 4 {
                        // Z/P spills do not change integer state in a stack walk.
                        Op::Nop
                    } else if number == 0 {
                        Op::RestoreVl(((((reg as u64 >> 5) & 3) << 6) | (encoded as u64 & 63)) * 8)
                    } else {
                        return Err(bad("reserved SVE predicate register"));
                    };
                    result.push(op);
                    continue;
                }
                let first = (reg & 31) as usize;
                let pair = reg & 0x40 != 0;
                let writeback = reg & 0x20 != 0;
                if first + usize::from(pair) > if kind == 0 { 30 } else { 31 } {
                    return Err(bad("invalid save_any register"));
                }
                let scale = if pair || writeback || kind == 2 {
                    16
                } else {
                    8
                };
                let offset = (encoded as u64 & 63) * scale;
                let post = if writeback { offset + scale } else { 0 };
                let offset = if writeback { 0 } else { offset };
                if kind == 0 {
                    save(first, pair.then_some(first + 1), offset, post)
                } else {
                    Op::VectorSave {
                        first,
                        pair,
                        width: if kind == 2 { 16 } else { 8 },
                        offset,
                        post,
                    }
                }
            }
            0xe8..=0xec => Op::Custom(b),
            0xfc => Op::Pac,
            _ => return Err(bad(format!("unsupported unwind opcode {b:#x}"))),
        };
        result.push(op);
    }
    Err(bad("unterminated unwind sequence"))
}
fn length(ops: &[Op]) -> usize {
    ops.iter()
        .take_while(|op| !matches!(op, Op::Chain | Op::End))
        .filter(|op| !matches!(op, Op::Custom(_)))
        .count()
}
#[derive(Debug)]
pub struct Step {
    pub method: &'static str,
    pub return_address: bool,
    pub stack_switch: bool,
}
fn apply(m: &impl VirtualMemory, r: &mut Registers, ops: &[Op], mut skip: usize) -> Result<Step> {
    let mut result = Step {
        method: "xdata",
        return_address: true,
        stack_switch: false,
    };
    let mut pc_from_lr = true;
    for op in ops {
        if skip != 0 {
            if !matches!(op, Op::Custom(_) | Op::Chain) {
                skip -= 1;
            }
            continue;
        }
        match *op {
            Op::Alloc(n) => r.sp = add(r.sp, n)?,
            Op::AllocVl(n) => {
                let vl = r.sve_vl.ok_or_else(|| {
                    bad("SVE vector length unavailable; supply vector_length in bytes")
                })?;
                if !(16..=256).contains(&vl) || vl % 16 != 0 {
                    return Err(bad("invalid SVE vector length"));
                }
                r.sp = add(r.sp, n * vl as u64)?;
            }
            Op::RestoreVl(offset) => {
                let saved = m.u64(add(r.sp, offset)?)?;
                let vg = saved & 63;
                if saved & 0x7fff_ffff_ffff_ffc0 != 0 || !(2..=32).contains(&vg) || vg % 2 != 0 {
                    return Err(bad("invalid saved SVE VG/SM"));
                }
                r.sve_vl = Some(vg as u16 * 8);
            }
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
            Op::VectorSave { post, .. } => r.sp = add(r.sp, post)?,
            Op::Custom(kind) => {
                let base = r.sp;
                if base & 15 != 0 {
                    return Err(bad("unaligned exception/context frame"));
                }
                result.return_address = false;
                result.stack_switch = kind != 0xec;
                if result.stack_switch {
                    r.sve_vl = None;
                }
                pc_from_lr = false;
                match kind {
                    0xe8 => {
                        // ARM64 KTRAP_FRAME layout, shared with the Windows/.NET unwinder.
                        let mut data = [0u8; 0x150];
                        m.read(base, &mut data)?;
                        let q = |at| {
                            u64::from_le_bytes(data[at..at + 8].try_into().expect("frame field"))
                        };
                        for i in 0..19 {
                            r.x[i] = q(0xa0 + i * 8);
                        }
                        r.pstate =
                            u32::from_le_bytes(data[0x90..0x94].try_into().expect("SPSR")) as u64;
                        r.sp = q(0x98);
                        r.x[30] = q(0x138);
                        r.x[29] = q(0x140);
                        r.pc = q(0x148);
                    }
                    0xe9 => {
                        r.pc = m.u64(add(base, 8)?)?;
                        r.sp = m.u64(base)?;
                    }
                    0xea => {
                        let mut data = [0u8; 0x110];
                        m.read(base, &mut data)?;
                        let q = |at| {
                            u64::from_le_bytes(data[at..at + 8].try_into().expect("context field"))
                        };
                        let flags =
                            u32::from_le_bytes(data[..4].try_into().expect("context flags"));
                        if flags & 0x400003 != 0x400003 {
                            return Err(bad("context lacks ARM64 control/integer state"));
                        }
                        for i in 0..31 {
                            r.x[i] = q(8 + i * 8);
                        }
                        r.pstate = u32::from_le_bytes(data[4..8].try_into().expect("CPSR")) as u64;
                        r.sp = q(0x100);
                        r.pc = q(0x108);
                        result.return_address = flags & 0x20000000 != 0;
                    }
                    0xeb => {
                        // ARM64EC_NT_CONTEXT overlay in the Windows SDK. Only
                        // integer/control state is needed for the ARM64 stack.
                        let mut data = [0u8; 0x1a0];
                        m.read(base, &mut data)?;
                        let q = |at| {
                            u64::from_le_bytes(
                                data[at..at + 8].try_into().expect("EC context field"),
                            )
                        };
                        let flags = u32::from_le_bytes(
                            data[0x30..0x34].try_into().expect("EC context flags"),
                        );
                        if flags & 0x10000b != 0x10000b {
                            return Err(bad("EC context lacks control/integer/FP state"));
                        }
                        for (register, offset) in [
                            (8, 0x78),
                            (0, 0x80),
                            (1, 0x88),
                            (27, 0x90),
                            (29, 0xa0),
                            (25, 0xa8),
                            (26, 0xb0),
                            (2, 0xb8),
                            (3, 0xc0),
                            (4, 0xc8),
                            (5, 0xd0),
                            (19, 0xd8),
                            (20, 0xe0),
                            (21, 0xe8),
                            (22, 0xf0),
                            (30, 0x120),
                            (6, 0x130),
                            (7, 0x140),
                            (9, 0x150),
                            (10, 0x160),
                            (11, 0x170),
                            (12, 0x180),
                            (15, 0x190),
                        ] {
                            r.x[register] = q(offset);
                        }
                        for (register, start) in [(16, 0x128), (17, 0x168)] {
                            r.x[register] = (0..4)
                                .map(|i| {
                                    let at = start + i * 16;
                                    (u16::from_le_bytes(
                                        data[at..at + 2].try_into().expect("EC split register"),
                                    ) as u64)
                                        << (16 * i)
                                })
                                .fold(0, |a, b| a | b);
                        }
                        // EC records represent EL0. Flags don't affect unwind
                        // address calculations; this is not a context restore.
                        r.pstate = 0;
                        r.sp = q(0x98);
                        r.pc = q(0xf8);
                        result.return_address = flags & 0x20000000 != 0;
                    }
                    0xec => r.pc = r.x[30],
                    _ => unreachable!(),
                }
                if !matches!(r.pstate & 31, 0 | 4 | 5) || r.pc & 3 != 0 || r.sp & 15 != 0 {
                    return Err(bad("invalid exception/context registers"));
                }
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
    if pc_from_lr {
        r.pc = r.x[30];
    }
    Ok(result)
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
    let saves = (intsz + fpsz + if home && intsz + fpsz != 0 { 64 } else { 0 } + 15) & !15;
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
    if cr == 1 && int % 2 == 0 {
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
    if home && allocated {
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
            if word(m, add(module.base, pe + 4)?)? & 0xffff != 0xaa64 {
                return Err(bad(
                    "module does not contain a native ARM64 runtime-function table",
                ));
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
    ) -> Result<Step> {
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
            return Ok(Step {
                method: "leaf",
                return_address: true,
                stack_switch: false,
            });
        }
        let (start, mut data) = table[index - 1];
        let offset = (rva - start as u64) / 4;
        let linked = data & 3 == 3;
        let mut seen = HashSet::new();
        while data & 3 == 3 {
            let target = (data & !3) as u64;
            if target + 8 > module.size as u64 || !seen.insert(target) || seen.len() > 32 {
                return Err(bad("invalid/cyclic .pdata link"));
            }
            data = word(m, add(module.base, target + 4)?)?;
        }
        if data & 3 != 0 {
            let len = ((data >> 2) & 2047) as u64;
            if !linked && offset >= len {
                r.pc = r.x[30];
                return Ok(Step {
                    method: "leaf",
                    return_address: true,
                    stack_switch: false,
                });
            }
            let ops = packed(data)?;
            let prolog = length(&ops);
            let epilog: Vec<_> = ops
                .iter()
                .filter(|o| !matches!(o, Op::Fp(_) | Op::Home))
                .cloned()
                .collect();
            let mut result = if !linked && data & 3 == 1 && offset < (prolog as u64) {
                apply(m, r, &ops, prolog - offset as usize)?
            } else if !linked
                && data & 3 == 1
                && len >= epilog.len() as u64
                && offset >= len - epilog.len() as u64
            {
                apply(
                    m,
                    r,
                    &epilog,
                    (offset - (len - epilog.len() as u64)) as usize,
                )?
            } else {
                apply(m, r, &ops, 0)?
            };
            result.method = if linked { "packed link" } else { "packed" };
            return Ok(result);
        }
        if data as u64 + 4 > module.size as u64 {
            return Err(bad(".xdata outside module"));
        }
        let x = add(module.base, data as u64)?;
        let header = word(m, x)?;
        let len = (header & 0x3ffff) as u64;
        if !linked && offset >= len {
            r.pc = r.x[30];
            return Ok(Step {
                method: "leaf",
                return_address: true,
                stack_switch: false,
            });
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
            if ext >> 24 != 0 {
                return Err(bad("reserved .xdata extension bits"));
            }
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
        if !linked && offset < (length(&ops) as u64) {
            let mut result = apply(m, r, &ops, length(&ops) - offset as usize)?;
            result.method = "xdata prolog";
            return Ok(result);
        }
        if linked {
            let mut result = apply(m, r, &ops, 0)?;
            result.method = "xdata link";
            return Ok(result);
        }
        if single {
            let epilog = sequence(&code, epilogs as usize)?;
            let count = length(&epilog) + 1;
            if len >= count as u64 && offset >= len - count as u64 {
                let mut result = apply(m, r, &epilog, (offset - (len - count as u64)) as usize)?;
                result.method = "xdata epilog";
                return Ok(result);
            }
        } else {
            for i in 0..scopes {
                let scope = word(m, x + cursor + i as u64 * 4)?;
                if scope & 0x003c0000 != 0 {
                    return Err(bad("reserved epilog scope bits"));
                }
                let start = (scope & 0x3ffff) as u64;
                if start > offset {
                    break;
                }
                let epilog = sequence(&code, (scope >> 22) as usize)?;
                if offset - start <= length(&epilog) as u64 {
                    let mut result = apply(m, r, &epilog, (offset - start) as usize)?;
                    result.method = "xdata epilog";
                    return Ok(result);
                }
            }
        }
        apply(m, r, &ops, 0)
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
    let mut return_address = false;
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
        let stack_switch;
        match tables.step(m, module, &mut regs, return_address) {
            Ok(result) => {
                frame.method = result.method.into();
                return_address = result.return_address;
                stack_switch = result.stack_switch;
            }
            Err(e) => {
                frame.method = "stop".into();
                frames.push(frame);
                stop = Some(e.to_string());
                break;
            }
        }
        frames.push(frame);
        if (!stack_switch && (regs.sp < old || regs.sp - old > 16 * 1024 * 1024))
            || regs.sp & 15 != 0
        {
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
            sve_vl: None,
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
        assert!(sequence(&[0xed, 0xe4], 0).is_err());
        assert!(sequence(&[0xe3], 0).is_err());
        assert!(packed(1 | (15 << 16)).is_err());
    }
    #[test]
    fn save_next_expands_backwards_before_integer_and_vector_pairs() {
        let ops = sequence(&[0xe6, 0xe6, 0xcc, 7, 0xe4], 0).unwrap();
        let mut m = Memory(vec![0; 0x2000]);
        for i in 0..6 {
            put(&mut m, 0x1000 + i * 8, 0x19 + i as u64);
        }
        let mut r = regs();
        apply(&m, &mut r, &ops, 0).unwrap();
        assert_eq!(&r.x[19..25], &[0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e]);
        assert_eq!(r.sp, 0x1040);
        let ops = sequence(&[0xe6, 0xe7, 0x68, 0x83, 0xe4], 0).unwrap();
        assert!(matches!(
            ops[0],
            Op::VectorSave {
                first: 10,
                width: 16,
                offset: 32,
                post: 0,
                ..
            }
        ));
        assert!(matches!(
            ops[1],
            Op::VectorSave {
                first: 8,
                width: 16,
                post: 64,
                ..
            }
        ));
        assert!(sequence(&[0xe7, 0x5e, 0, 0xe4], 0).is_err()); // x30/x31
        assert!(sequence(&[0xe6, 0xe3, 0xe4], 0).is_err());
    }
    #[test]
    fn save_any_writeback_and_home_only_packed_frames() {
        let mut m = Memory(vec![0; 0x2000]);
        put(&mut m, 0x1000, 0x29);
        put(&mut m, 0x1008, 0x8000);
        let mut r = regs();
        apply(&m, &mut r, &sequence(&[0xe7, 0x7d, 3, 0xe4], 0).unwrap(), 0).unwrap();
        assert_eq!((r.x[29], r.pc, r.sp), (0x29, 0x8000, 0x1040));
        let mut r = regs();
        r.x[30] = 0x9000;
        let ops = packed(1 | (100 << 2) | (1 << 20) | (4 << 23)).unwrap();
        assert_eq!(length(&ops), 1);
        apply(&m, &mut r, &ops, 0).unwrap();
        assert_eq!((r.sp, r.pc), (0x1040, 0x9000));
    }
    #[test]
    fn exception_frames_restore_precise_pc_and_allow_stack_switches() {
        let mut m = Memory(vec![0; 0x2000]);
        put(&mut m, 0x1090, 5);
        put(&mut m, 0x1098, 0x800);
        put(&mut m, 0x1138, 0x9000);
        put(&mut m, 0x1140, 0x820);
        put(&mut m, 0x1148, 0x7000);
        put(&mut m, 0x10a0, 42);
        let mut r = regs();
        let result = apply(&m, &mut r, &sequence(&[0xe8, 0xe4], 0).unwrap(), 0).unwrap();
        assert_eq!((r.pc, r.sp, r.x[30], r.x[0]), (0x7000, 0x800, 0x9000, 42));
        assert!(result.stack_switch && !result.return_address);
        put(&mut m, 0x1000, 0x400003);
        put(&mut m, 0x1008, 99);
        put(&mut m, 0x1100, 0x800);
        put(&mut m, 0x1108, 0x6000);
        let mut r = regs();
        let result = apply(&m, &mut r, &sequence(&[0xea, 0xe4], 0).unwrap(), 0).unwrap();
        assert_eq!((r.pc, r.sp, r.x[0]), (0x6000, 0x800, 99));
        assert!(!result.return_address);
        put(&mut m, 0x1000, 0x800);
        put(&mut m, 0x1008, 0x5000);
        let mut r = regs();
        apply(&m, &mut r, &sequence(&[0xe9, 0xe4], 0).unwrap(), 0).unwrap();
        assert_eq!((r.pc, r.sp), (0x5000, 0x800));
    }
    #[test]
    fn jump_island_links_use_parent_body_and_reject_cycles() {
        let mut m = Memory(vec![0; 0x3000]);
        put(&mut m, 0x3c, 0x80);
        put(&mut m, 0x84, 0xaa64);
        put(&mut m, 0x120, 0x200 | (16u64 << 32));
        let packed = 1 | (100 << 2) | (1 << 23);
        put(&mut m, 0x200, 0x400 | ((packed as u64) << 32));
        put(&mut m, 0x208, 0x800 | (0x203u64 << 32));
        let module = Module {
            name: "example.sys".into(),
            path: "example.sys".into(),
            base: 0,
            size: 0x3000,
        };
        let mut r = regs();
        r.pc = 0x800;
        r.x[30] = 0x9000;
        let result = Tables::default().step(&m, &module, &mut r, false).unwrap();
        assert_eq!(result.method, "packed link");
        assert_eq!((r.pc, r.sp), (0x9000, 0x1010));
        put(&mut m, 0x200, 0x400 | (0x20bu64 << 32));
        let mut r = regs();
        r.pc = 0x800;
        assert!(Tables::default().step(&m, &module, &mut r, false).is_err());
    }

    #[test]
    fn walk_crosses_machine_frame_and_uses_precise_pc_in_next_prolog() {
        let mut m = Memory(vec![0; 0x3000]);
        put(&mut m, 0x3c, 0x80);
        put(&mut m, 0x84, 0xaa64);
        put(&mut m, 0x120, 0x200 | (16u64 << 32));
        put(&mut m, 0x200, 0x400 | (0x300u64 << 32));
        put(
            &mut m,
            0x208,
            0x500 | ((1u64 | (64 << 2) | (1 << 23)) << 32),
        );
        put(&mut m, 0x300, 64 | (1u64 << 27) | (0xe4e9u64 << 32));
        put(&mut m, 0x1000, 0x800);
        put(&mut m, 0x1008, 0x500);
        let module = Module {
            name: "example.sys".into(),
            path: "example.sys".into(),
            base: 0,
            size: 0x3000,
        };
        let mut r = regs();
        r.pc = 0x410;
        r.x[30] = 0;
        let stack = walk(&m, &r, &[module], &mut Tables::default());
        assert!(stack.stop.is_none(), "{:?}", stack.stop);
        assert_eq!(stack.frames.len(), 2);
        assert_eq!((stack.frames[1].pc, stack.frames[1].sp), (0x500, 0x800));
    }
    #[test]
    fn sve_allocations_use_known_or_saved_vector_length() {
        let mut m = Memory(vec![0; 0x2000]);
        let ops = sequence(&[0xdf, 2, 0xe4], 0).unwrap();
        assert!(apply(&m, &mut regs(), &ops, 0).is_err());
        let mut r = regs();
        r.sve_vl = Some(32);
        apply(&m, &mut r, &ops, 0).unwrap();
        assert_eq!(r.sp, 0x1040);
        put(&mut m, 0x1000, 8); // VG=8 -> VL=64 bytes
        let ops = sequence(&[0xe7, 0x10, 0xc0, 0xdf, 2, 0xe4], 0).unwrap();
        let mut r = regs();
        apply(&m, &mut r, &ops, 0).unwrap();
        assert_eq!(r.sp, 0x1080);
        put(&mut m, 0x1000, 0x100);
        assert!(apply(&m, &mut regs(), &ops, 0).is_err());
        assert!(sequence(&[0xe7, 0x11, 0xc0, 0xe4], 0).is_err());
    }
    #[test]
    fn ec_context_restores_arm64_overlay_and_split_registers() {
        let mut m = Memory(vec![0; 0x2000]);
        put(&mut m, 0x1030, 0x10000b);
        put(&mut m, 0x1080, 42);
        put(&mut m, 0x1098, 0x800);
        put(&mut m, 0x10a0, 0x900);
        put(&mut m, 0x10f8, 0x4000);
        put(&mut m, 0x1120, 0x5000);
        for i in 0..4 {
            m.0[0x1128 + i * 16..0x112a + i * 16].copy_from_slice(&(i as u16 + 1).to_le_bytes());
        }
        let mut r = regs();
        let result = apply(&m, &mut r, &sequence(&[0xeb, 0xe4], 0).unwrap(), 0).unwrap();
        assert_eq!(
            (r.pc, r.sp, r.x[29], r.x[30], r.x[0]),
            (0x4000, 0x800, 0x900, 0x5000, 42)
        );
        assert_eq!(r.x[16], 0x0004000300020001);
        assert!(result.stack_switch && !result.return_address);
    }
}
