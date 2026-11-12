//! Windows AMD64 table-based virtual unwinding. No stack scanning or RBP guessing.
use vroom_kernel::windows::{Image, VirtualMemory};
use vroom_kernel::x86_64::{canonical, General};

#[derive(Clone, Debug)]
pub(crate) struct Context {
    /// Register numbers are the Windows UNWIND_CODE encoding (RAX..R15).
    pub registers: [u64; 16],
    pub pc: u64,
}
impl From<&General> for Context {
    fn from(r: &General) -> Self {
        Self {
            registers: [
                r.rax, r.rcx, r.rdx, r.rbx, r.rsp, r.rbp, r.rsi, r.rdi, r.r8, r.r9, r.r10, r.r11,
                r.r12, r.r13, r.r14, r.r15,
            ],
            pc: r.rip,
        }
    }
}
impl Context {
    pub(crate) fn sp(&self) -> u64 {
        self.registers[4]
    }
}
fn add(a: u64, b: u64) -> Result<u64, String> {
    a.checked_add(b).ok_or("unwind address overflow".into())
}
fn read(m: &impl VirtualMemory, a: u64, n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0; n];
    m.read(a, &mut b).map_err(|e| e.to_string())?;
    Ok(b)
}
fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn short(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn ptr(m: &impl VirtualMemory, a: u64) -> Result<u64, String> {
    m.u64(a).map_err(|e| e.to_string())
}
fn nonvolatile(r: usize) -> bool {
    matches!(r, 3 | 5 | 6 | 7 | 12..=15)
}
fn rva(image: &Image, a: u32, length: u64) -> Result<u64, String> {
    if a as u64 + length > image.size as u64 {
        return Err("unwind RVA outside image".into());
    }
    add(image.base, a as u64)
}
fn pop(m: &impl VirtualMemory, c: &mut Context, reg: usize) -> Result<(), String> {
    let value = ptr(m, c.sp())?;
    c.registers[4] = add(c.sp(), 8)?;
    c.registers[reg] = value;
    Ok(())
}
fn ret(m: &impl VirtualMemory, c: &mut Context) -> Result<(), String> {
    c.pc = ptr(m, c.sp())?;
    c.registers[4] = add(c.sp(), 8)?;
    Ok(())
}
fn pop_instruction(code: &[u8], at: usize) -> Option<(usize, usize)> {
    let b = *code.get(at)?;
    if (0x58..=0x5f).contains(&b) {
        Some(((b - 0x58) as usize, 1))
    } else if (0x40..=0x4f).contains(&b) {
        let next = *code.get(at + 1)?;
        if (0x58..=0x5f).contains(&next) {
            Some(((next - 0x58 + (b & 1) * 8) as usize, 2))
        } else {
            None
        }
    } else {
        None
    }
}

/// Recognize complete legal v1 epilogs before applying prolog unwind codes.
/// Unsupported instruction forms are not guessed; metadata remains the fallback.
fn epilog(
    m: &impl VirtualMemory,
    image: &Image,
    begin: u32,
    end: u32,
    c: &Context,
) -> Result<Option<Context>, String> {
    let mut code = Vec::new();
    for i in 0..64u64.min(add(image.base, end as u64)?.saturating_sub(c.pc)) {
        match read(m, add(c.pc, i)?, 1) {
            Ok(b) => code.push(b[0]),
            Err(_) => break,
        }
    }
    let mut out = c.clone();
    let mut i = 0;
    if code.starts_with(&[0x48, 0x83, 0xc4]) && code.len() >= 4 {
        if code[3] > 127 {
            return Ok(None);
        }
        out.registers[4] = add(out.sp(), code[3] as u64)?;
        i = 4;
    } else if code.starts_with(&[0x48, 0x81, 0xc4]) && code.len() >= 7 {
        let n = word(&code, 3);
        if n > i32::MAX as u32 {
            return Ok(None);
        }
        out.registers[4] = add(out.sp(), n as u64)?;
        i = 7;
    } else if code.len() >= 4 && matches!(code[0], 0x48 | 0x49) && code[1] == 0x8d {
        let modrm = code[2];
        // An ordinary body LEA (including indexed addressing) is not an
        // epilog candidate unless it writes RSP. Check before SIB restrictions.
        if (modrm >> 3) & 7 != 4 {
            return Ok(None);
        }
        let mode = modrm >> 6;
        let (base, disp) = if modrm & 7 == 4 {
            let sib = code[3];
            if sib & 0xf8 != 0x20 {
                return Err("unsupported indexed epilog LEA".into());
            }
            ((sib & 7) as usize, 4)
        } else {
            ((modrm & 7) as usize, 3)
        };
        let base = base + if code[0] == 0x49 { 8 } else { 0 };
        if !nonvolatile(base) {
            return Ok(None);
        }
        let displacement = match mode {
            1 if code.len() > disp => {
                i = disp + 1;
                code[disp] as i8 as i64
            }
            2 if code.len() >= disp + 4 => {
                i = disp + 4;
                word(&code, disp) as i32 as i64
            }
            _ => return Ok(None),
        };
        out.registers[4] = out.registers[base]
            .checked_add_signed(displacement)
            .ok_or("epilog overflow")?;
    }
    let pop_start = i;
    loop {
        let Some(_) = code.get(i) else {
            return Ok(None);
        };
        let Some((reg, size)) = pop_instruction(&code, i) else {
            break;
        };
        if !nonvolatile(reg) {
            return Ok(None);
        }
        // Delay memory reads until the instruction sequence has been validated.
        i += size;
    }
    let tail = match code.get(i) {
        Some(0xc3) => true,
        Some(0xc2) => {
            if code.len() < i + 3 {
                return Err("truncated RET imm16 epilog".into());
            }
            true
        }
        Some(0xf3) if code.get(i + 1) == Some(&0xc3) => true,
        Some(0xe9) if code.len() >= i + 5 => {
            let target =
                add(c.pc, (i + 5) as u64)?.checked_add_signed(word(&code, i + 1) as i32 as i64);
            target.is_some_and(|t| t < image.base + begin as u64 || t >= image.base + end as u64)
        }
        Some(0xeb) if code.len() >= i + 2 => {
            let target = add(c.pc, (i + 2) as u64)?.checked_add_signed(code[i + 1] as i8 as i64);
            target.is_some_and(|t| t < image.base + begin as u64 || t >= image.base + end as u64)
        }
        Some(0xff) | Some(0x48..=0x4f) => {
            let at = i + usize::from(code[i] != 0xff);
            if code.get(at) != Some(&0xff) {
                false
            } else if let Some(&modrm) = code.get(at + 1) {
                if modrm & 0xf8 != 0x20 {
                    false
                }
                // JMP /4 with ModRM.mod == 00 only.
                else {
                    let length = match modrm & 7 {
                        5 => 6,
                        4 => match code.get(at + 2) {
                            Some(sib) if sib & 7 == 5 => 7,
                            Some(_) => 3,
                            None => usize::MAX,
                        },
                        _ => 2,
                    };
                    length != usize::MAX && code.len() >= at + length
                }
            } else {
                false
            }
        }
        _ => false,
    };
    if !tail {
        if i != 0 {
            return Err("unsupported or incomplete AMD64 epilog; refusing prolog fallback".into());
        }
        return Ok(None);
    }
    // Replay only the pops (the stack allocation was already undone above).
    let mut at = pop_start;
    while at < i {
        let (reg, n) = pop_instruction(&code, at).ok_or("invalid epilog pop")?;
        pop(m, &mut out, reg)?;
        at += n;
    }
    ret(m, &mut out)?;
    if code.get(i) == Some(&0xc2) {
        out.registers[4] = add(out.sp(), short(&code, i + 1) as u64)?;
    }
    Ok(Some(out))
}

type Operation = (u64, u8, usize, u64);
struct UnwindRecord {
    version: u8,
    flags: u8,
    prolog: u8,
    frame: u8,
    count: u32,
    operations: Vec<Operation>,
    epilog: Option<(u64, u64)>, // offset within the matching scope, scope size
}

/// V2 starts with an EPILOG size/flags slot, followed by end-relative 12-bit
/// scope offsets and optional zero-offset padding. These slots are NOT prolog
/// offsets and must not participate in the descending prolog-code order check.
/// Sources: LLVM MCWin64EH.cpp EmitUnwindInfo/MCUnwindV2EpilogTargetExpr and
/// dotnet/runtime src/coreclr/unwinder/amd64/unwinder.cpp VirtualUnwind (the
/// latter is Windows minkernel/ntos/rtl/amd64/exdsptch.c-derived code).
fn epilog_scopes(
    codes: &[u8],
    begin: u32,
    end: u32,
    pc: Option<u64>,
) -> Result<(usize, Option<(u64, u64)>), String> {
    if codes.len() < 2 || codes[1] & 15 != 6 {
        return Ok((0, None));
    }
    let size = codes[0] as u64;
    let flags = codes[1] >> 4;
    if size == 0 || flags > 1 || size > (end - begin) as u64 {
        return Err("invalid v2 epilog size/flags".into());
    }
    let mut matched = None;
    let mut scopes = 0;
    let mut check = |distance: u64| -> Result<(), String> {
        if distance < size || distance > (end - begin) as u64 {
            return Err("v2 epilog scope outside function".into());
        }
        let start = end as u64 - distance;
        if let Some(offset) = pc
            .and_then(|pc| pc.checked_sub(start))
            .filter(|&n| n < size)
        {
            if matched.is_some() {
                return Err("overlapping v2 epilog scopes".into());
            }
            matched = Some((offset, size));
        }
        scopes += 1;
        Ok(())
    };
    if flags & 1 != 0 {
        check(size)?;
    }
    let mut i = 1;
    let mut padded = false;
    while i * 2 < codes.len() && codes[i * 2 + 1] & 15 == 6 {
        let distance = codes[i * 2] as u64 | ((codes[i * 2 + 1] >> 4) as u64) << 8;
        i += 1;
        if padded {
            return Err("v2 epilog scope after padding".into());
        }
        if distance == 0 {
            padded = true;
        } else {
            check(distance)?;
        }
    }
    if scopes == 0 {
        return Err("v2 epilog header without a scope".into());
    }
    Ok((i, matched))
}

fn unwind_record(
    m: &impl VirtualMemory,
    image: &Image,
    begin: u32,
    end: u32,
    info: u32,
    pc: Option<u64>,
) -> Result<UnwindRecord, String> {
    if info & 3 != 0 {
        return Err("unaligned UNWIND_INFO".into());
    }
    let h = read(m, rva(image, info, 4)?, 4)?;
    let version = h[0] & 7;
    if !matches!(version, 1 | 2) {
        return Err(format!("unsupported AMD64 unwind version {version}"));
    }
    let flags = h[0] >> 3;
    if flags & !7 != 0 || (flags & 4 != 0 && flags & 3 != 0) {
        return Err("invalid unwind flags".into());
    }
    let count = h[2] as usize;
    let codes = read(m, rva(image, info + 4, count as u64 * 2)?, count * 2)?;
    let frame = (h[3] & 15) as usize;
    if (frame != 0 && !nonvolatile(frame)) || (frame == 0 && h[3] >> 4 != 0) {
        return Err("invalid unwind frame register".into());
    }
    let (mut i, epilog) = if version == 2 {
        epilog_scopes(&codes, begin, end, pc)?
    } else {
        (0, None)
    };
    let mut previous = 256;
    let mut operations = Vec::new();
    while i < count {
        let location = codes[i * 2] as u64;
        if location > previous || location > h[1] as u64 {
            return Err("invalid unwind code order".into());
        }
        previous = location;
        let op = codes[i * 2 + 1] & 15;
        let reg = (codes[i * 2 + 1] >> 4) as usize;
        let extra = match op {
            0 | 2 | 3 | 10 => 0,
            1 if reg == 0 => 1,
            1 if reg == 1 => 2,
            4 | 8 => 1,
            5 | 9 => 2,
            _ => return Err(format!("unsupported unwind opcode {op}/{reg}")),
        };
        if i + extra >= count {
            return Err("truncated unwind operands".into());
        }
        let operand = match extra {
            1 => short(&codes, (i + 1) * 2) as u64,
            2 => word(&codes, (i + 1) * 2) as u64,
            _ => 0,
        };
        if matches!(op, 0 | 4 | 5) && !nonvolatile(reg)
            || op == 3 && (reg != 0 || frame == 0)
            || matches!(op, 8 | 9) && reg < 6
            || op == 10 && (reg > 1 || i + 1 != count || flags & 4 != 0)
            || op == 1 && reg == 1 && operand & 7 != 0
        {
            return Err("invalid unwind register/operand encoding".into());
        }
        operations.push((location, op, reg, operand));
        i += extra + 1;
    }
    Ok(UnwindRecord {
        version,
        flags,
        prolog: h[1],
        frame: h[3],
        count: count as u32,
        operations,
        epilog,
    })
}

/// UnwindData bit 0 denotes an image-relative pointer to another
/// RUNTIME_FUNCTION, not an UNWIND_INFO address (Windows indirect entries).
/// The target need not cover the lookup PC: it can describe the primary part
/// of a separated function. Resolve before computing prolog/epilog offsets.
fn resolve_function(
    m: &impl VirtualMemory,
    image: &Image,
    mut begin: u32,
    mut end: u32,
    mut info: u32,
) -> Result<(u32, u32, u32), String> {
    let mut seen = Vec::new();
    for _ in 0..32 {
        if begin >= end || end > image.size {
            return Err("invalid indirect RUNTIME_FUNCTION range".into());
        }
        if info & 1 == 0 {
            return Ok((begin, end, info));
        }
        let target = info & !1;
        if target & 3 != 0 {
            return Err("unaligned indirect RUNTIME_FUNCTION".into());
        }
        if seen.contains(&target) {
            return Err("cyclic indirect RUNTIME_FUNCTION".into());
        }
        seen.push(target);
        let entry = read(m, rva(image, target, 12)?, 12)?;
        (begin, end, info) = (word(&entry, 0), word(&entry, 4), word(&entry, 8));
    }
    Err("excessive indirect RUNTIME_FUNCTION chain".into())
}

fn chained_function(
    m: &impl VirtualMemory,
    image: &Image,
    info: u32,
    count: u32,
) -> Result<(u32, u32, u32), String> {
    let chain = read(m, rva(image, info + 4 + ((count + 1) & !1) * 2, 12)?, 12)?;
    let (begin, end, info) = (word(&chain, 0), word(&chain, 4), word(&chain, 8));
    if begin >= end || end > image.size {
        return Err("invalid chained function".into());
    }
    resolve_function(m, image, begin, end, info)
}

/// V2 scopes exclude ADD/LEA stack cleanup. Windows UnwindEpilogue mirrors
/// the primary entry's trailing PUSH_NONVOLs, optional pushed FLAGS slot, and
/// machine frame. Low-register pops occupy one byte, R8..R15 pops two bytes.
/// No opcode scanning or full-prolog fallback is safe inside a declared scope.
fn epilog_v2(
    m: &impl VirtualMemory,
    image: &Image,
    c: &Context,
    mut begin: u32,
    mut end: u32,
    mut info: u32,
    offset: u64,
    size: u64,
) -> Result<Context, String> {
    for _ in 0..16 {
        let record = unwind_record(m, image, begin, end, info, None)?;
        let first = record
            .operations
            .iter()
            .position(|&(_, op, _, _)| matches!(op, 0 | 10));
        if first.is_none() && record.flags & 4 != 0 {
            (begin, end, info) = chained_function(m, image, info, record.count)?;
            continue;
        }
        let mut out = c.clone();
        let mut current = 0;
        let mut boundary = offset == 0;
        let tail = &record.operations[first.unwrap_or(record.operations.len())..];
        let mut i = 0;
        while let Some(&(_, 0, reg, _)) = tail.get(i) {
            if current >= offset {
                pop(m, &mut out, reg)?;
            }
            current += if reg >= 8 { 2 } else { 1 };
            boundary |= current == offset;
            i += 1;
        }
        if let Some(&(_, 2, 0, _)) = tail.get(i) {
            if current >= offset {
                out.registers[4] = add(out.sp(), 8)?;
            }
            current += 1;
            boundary |= current == offset;
            i += 1;
        }
        let machine = matches!(tail.get(i), Some(&(_, 10, _, _)));
        if machine {
            i += 1;
        }
        if i != tail.len() || current + 1 != size || !boundary {
            return Err("unsupported v2 epilog layout/instruction offset".into());
        }
        if machine {
            // Error-code cleanup, if any, precedes the v2 epilog scope.
            out.pc = ptr(m, out.sp())?;
            out.registers[4] = ptr(m, add(out.sp(), 24)?)?;
        } else {
            ret(m, &mut out)?;
        }
        return Ok(out);
    }
    Err("cyclic/excessive v2 epilog unwind chain".into())
}

/// Recover one caller. `lookup_pc` may be PC-1 for non-top return frames.
/// Context is unchanged on error. Handles v1/v2 prologs, chained records, leaf
/// functions, saved nonvolatile GPRs, stack allocations and machine frames.
pub(crate) fn step(
    m: &impl VirtualMemory,
    image: &Image,
    c: &mut Context,
    lookup_pc: u64,
) -> Result<&'static str, String> {
    if !(4096..=64 * 1024 * 1024).contains(&image.size)
        || image.base.checked_add(image.size as u64).is_none()
    {
        return Err("invalid unwind image range".into());
    }
    // Validate architecture even if a caller supplied an Image from elsewhere.
    let dos = read(m, image.base, 64)?;
    let pe = word(&dos, 60);
    if !(64..=4096).contains(&pe) {
        return Err("invalid AMD64 PE header".into());
    }
    let header = read(m, rva(image, pe, 264)?, 264)?;
    if &header[..4] != b"PE\0\0" || short(&header, 4) != 0x8664 || short(&header, 24) != 0x20b {
        return Err("stack module is not AMD64 PE32+".into());
    }
    let pc = lookup_pc
        .checked_sub(image.base)
        .filter(|&r| r < image.size as u64)
        .ok_or("PC outside module")?;
    let directories = word(&header, 24 + 108);
    if directories > 16 {
        return Err("invalid PE data directory count".into());
    }
    let (directory, size) = if directories >= 4 {
        (
            word(&header, 24 + 112 + 3 * 8),
            word(&header, 24 + 116 + 3 * 8),
        )
    } else {
        (0, 0)
    };
    if size % 12 != 0 || size > 12 * 1_000_000 {
        return Err("invalid AMD64 exception directory".into());
    }
    rva(image, directory, size as u64)?;
    let (mut lo, mut hi) = (0u32, size / 12);
    let mut found = None;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let entry = read(m, rva(image, directory + mid * 12, 12)?, 12)?;
        let (begin, end, info) = (word(&entry, 0), word(&entry, 4), word(&entry, 8));
        if begin >= end || end > image.size {
            return Err("invalid RUNTIME_FUNCTION range".into());
        }
        if pc < begin as u64 {
            hi = mid;
        } else if pc >= end as u64 {
            lo = mid + 1;
        } else {
            found = Some((begin, end, info));
            break;
        }
    }
    let mut out = c.clone();
    let method = if let Some((begin, end, info)) = found {
        let (mut begin, mut end, mut info) = resolve_function(m, image, begin, end, info)?;
        // PC-1 is only the RUNTIME_FUNCTION lookup key for a return frame.
        // Epilog progress must describe the live context at its actual RIP.
        let primary = unwind_record(m, image, begin, end, info, c.pc.checked_sub(image.base))?;
        let detected = if primary.version == 1 {
            epilog(m, image, begin, end, c)?
        } else if let Some((offset, size)) = primary.epilog {
            Some(epilog_v2(m, image, c, begin, end, info, offset, size)?)
        } else {
            None
        };
        if let Some(epilog) = detected {
            out = epilog;
            "epilog"
        } else {
            let mut offset = pc.checked_sub(begin as u64).unwrap_or(u64::MAX);
            let mut machine = false;
            let mut frame_encoding = None;
            let mut complete = false;
            for _ in 0..16 {
                let record = unwind_record(m, image, begin, end, info, None)?;
                let flags = record.flags;
                let frame = (record.frame & 15) as usize;
                if frame_encoding.is_some_and(|encoding| encoding != record.frame) {
                    return Err("inconsistent chained frame register".into());
                }
                frame_encoding = Some(record.frame);
                let operations = record.operations;
                // SAVE_NONVOL offsets use the establisher frame, not a
                // dynamically adjusted RSP (e.g. body alloca after SET_FPREG).
                let established = frame != 0
                    && (offset >= record.prolog as u64
                        || operations
                            .iter()
                            .any(|&(at, op, _, _)| op == 3 && at <= offset));
                let frame_base = if established {
                    out.registers[frame]
                        .checked_sub((record.frame >> 4) as u64 * 16)
                        .ok_or("frame offset underflow")?
                } else {
                    out.sp()
                };
                for (location, op, reg, operand) in operations {
                    if offset >= location {
                        match op {
                            0 if nonvolatile(reg) => pop(m, &mut out, reg)?,
                            0 => return Err("invalid PUSH_NONVOL register".into()),
                            1 => {
                                out.registers[4] =
                                    add(out.sp(), if reg == 0 { operand * 8 } else { operand })?
                            }
                            2 => out.registers[4] = add(out.sp(), reg as u64 * 8 + 8)?,
                            3 if reg == 0 && frame != 0 => {
                                out.registers[4] = out.registers[frame]
                                    .checked_sub((record.frame >> 4) as u64 * 16)
                                    .ok_or("frame offset underflow")?
                            }
                            3 => return Err("invalid SET_FPREG".into()),
                            4 | 5 if nonvolatile(reg) => {
                                out.registers[reg] = ptr(
                                    m,
                                    add(frame_base, if op == 4 { operand * 8 } else { operand })?,
                                )?
                            }
                            4 | 5 => return Err("invalid SAVE_NONVOL register".into()),
                            8 | 9 if reg >= 6 => (), // XMM restores do not affect control/GPR recovery.
                            8 | 9 => return Err("invalid nonvolatile XMM register".into()),
                            10 if reg <= 1 => {
                                let sp = add(out.sp(), reg as u64 * 8)?;
                                out.pc = ptr(m, sp)?;
                                out.registers[4] = ptr(m, add(sp, 24)?)?;
                                machine = true;
                            }
                            _ => return Err("invalid unwind operation".into()),
                        }
                    }
                }
                if flags & 4 == 0 {
                    complete = true;
                    break;
                }
                (begin, end, info) = chained_function(m, image, info, record.count)?;
                offset = u64::MAX;
            }
            if !complete {
                return Err("cyclic/excessive chained unwind records".into());
            }
            if !machine {
                ret(m, &mut out)?;
            }
            if machine {
                "machine_frame"
            } else {
                "unwind_info"
            }
        }
    } else {
        ret(m, &mut out)?;
        "leaf"
    };
    if !canonical(out.pc)
        || !canonical(out.sp())
        || out.sp() & 7 != 0
        || (out.pc == c.pc && out.sp() == c.sp())
    {
        return Err("invalid/nonprogressing AMD64 caller context".into());
    }
    *c = out;
    Ok(method)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory(Vec<u8>);
    impl VirtualMemory for Memory {
        fn read(&self, a: u64, b: &mut [u8]) -> vroom_kernel::Result<()> {
            let source = self
                .0
                .get(a as usize..a as usize + b.len())
                .ok_or_else(|| vroom_kernel::Error::Invalid("unmapped".into()))?;
            b.copy_from_slice(source);
            Ok(())
        }
    }
    fn put32(m: &mut Memory, a: usize, v: u32) {
        m.0[a..a + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(m: &mut Memory, a: usize, v: u64) {
        m.0[a..a + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn setup(codes: &[u8], prolog: u8, frame: u8) -> (Memory, Image, Context) {
        let mut m = Memory(vec![0; 0x10000]);
        m.0[0x1000..0x1002].copy_from_slice(b"MZ");
        put32(&mut m, 0x103c, 0x80);
        m.0[0x1080..0x1084].copy_from_slice(b"PE\0\0");
        m.0[0x1084..0x1086].copy_from_slice(&0x8664u16.to_le_bytes());
        m.0[0x1098..0x109a].copy_from_slice(&0x20bu16.to_le_bytes());
        put32(&mut m, 0x1080 + 24 + 112 + 24, 0x200);
        put32(&mut m, 0x1080 + 24 + 108, 16);
        put32(&mut m, 0x1080 + 24 + 116 + 24, 12);
        put32(&mut m, 0x1200, 0x500);
        put32(&mut m, 0x1204, 0x600);
        put32(&mut m, 0x1208, 0x300);
        m.0[0x1300..0x1304].copy_from_slice(&[1, prolog, (codes.len() / 2) as u8, frame]);
        m.0[0x1304..0x1304 + codes.len()].copy_from_slice(codes);
        m.0[0x1500..0x1600].fill(0x90);
        let im = Image {
            base: 0x1000,
            size: 0x1000,
            timestamp: 0,
            image_base: 0x1000,
            name: "test".into(),
            pdb: None,
            exports: Default::default(),
            relocation_directory: (0, 0),
        };
        let mut c = Context {
            registers: [0; 16],
            pc: 0x1520,
        };
        c.registers[4] = 0x8000;
        (m, im, c)
    }
    #[test]
    fn leaf_and_architecture() {
        let (mut m, im, mut c) = setup(&[], 0, 0);
        put32(&mut m, 0x1080 + 24 + 116 + 24, 0);
        put64(&mut m, 0x8000, 0x1777);
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "leaf");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x8008));
        m.0[0x1084..0x1086].copy_from_slice(&0xaa64u16.to_le_bytes());
        assert!(step(&m, &im, &mut c, 0x1520).unwrap_err().contains("AMD64"));
    }
    #[test]
    fn allocation_push_and_partial_prolog() {
        // sub rsp,32 completes at +5, push rbx completes at +1.
        let (mut m, im, mut c) = setup(&[5, 0x32, 1, 0x30], 5, 0);
        put64(&mut m, 0x8020, 0xbabe);
        put64(&mut m, 0x8028, 0x1777);
        step(&m, &im, &mut c, 0x1520).unwrap();
        assert_eq!((c.pc, c.sp(), c.registers[3]), (0x1777, 0x8030, 0xbabe));
        // At +1 only PUSH completed; ALLOC_SMALL must not run.
        c.pc = 0x1501;
        c.registers[4] = 0x8020;
        step(&m, &im, &mut c, 0x1501).unwrap();
        assert_eq!(c.sp(), 0x8030);
        // At entry none of the prolog has executed.
        c.pc = 0x1500;
        c.registers[4] = 0x8028;
        step(&m, &im, &mut c, 0x1500).unwrap();
        assert_eq!(c.pc, 0x1777);
    }
    #[test]
    fn frame_register_and_saved_gpr() {
        let (mut m, im, mut c) = setup(&[8, 3, 4, 0x34, 2, 0], 8, 0x25);
        c.registers[5] = 0x8040; // frame RBP = fixed RSP + 32
        put64(&mut m, 0x8030, 0x1234);
        put64(&mut m, 0x8020, 0x1777);
        step(&m, &im, &mut c, 0x1520).unwrap();
        assert_eq!((c.pc, c.sp(), c.registers[3]), (0x1777, 0x8028, 0x1234));
    }
    #[test]
    fn dynamic_alloca_saves_use_establisher_frame() {
        // Save RBX after SET_FPREG, so it is processed before SET_FPREG.
        let (mut m, im, mut c) = setup(&[10, 0x34, 2, 0, 8, 3], 10, 0x25);
        c.registers[5] = 0x8040;
        c.registers[4] = 0x7000;
        put64(&mut m, 0x8030, 0xbabe);
        put64(&mut m, 0x8020, 0x1777);
        step(&m, &im, &mut c, 0x1520).unwrap();
        assert_eq!((c.pc, c.sp(), c.registers[3]), (0x1777, 0x8028, 0xbabe));
    }
    #[test]
    fn r12_frame_epilog_sib_and_indirect_tail_jump() {
        let (mut m, im, mut c) = setup(&[1, 3], 1, 0x2c);
        // lea rsp,[r12-32]; rex.w pop rbx; jmp qword ptr [rax].
        m.0[0x1520..0x1529]
            .copy_from_slice(&[0x49, 0x8d, 0x64, 0x24, 0xe0, 0x48, 0x5b, 0xff, 0x20]);
        c.registers[12] = 0x8040;
        put64(&mut m, 0x8020, 0xbabe);
        put64(&mut m, 0x8028, 0x1777);
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "epilog");
        assert_eq!((c.pc, c.sp(), c.registers[3]), (0x1777, 0x8030, 0xbabe));
    }
    #[test]
    fn epilog_and_partial_epilog() {
        let (mut m, im, mut c) = setup(&[5, 0x32, 1, 0x30], 5, 0);
        m.0[0x1520..0x1526].copy_from_slice(&[0x48, 0x83, 0xc4, 0x20, 0x5b, 0xc3]);
        put64(&mut m, 0x8020, 0xbabe);
        put64(&mut m, 0x8028, 0x1777);
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "epilog");
        assert_eq!(c.sp(), 0x8030);
        c.pc = 0x1524;
        c.registers[4] = 0x8020;
        assert_eq!(step(&m, &im, &mut c, 0x1524).unwrap(), "epilog");
        assert_eq!(c.sp(), 0x8030);
        c.pc = 0x1525;
        c.registers[4] = 0x8028;
        assert_eq!(step(&m, &im, &mut c, 0x1525).unwrap(), "epilog");
        assert_eq!(c.pc, 0x1777);
    }
    #[test]
    fn chain_and_machine_frame() {
        let (mut m, im, mut c) = setup(&[], 0, 0);
        m.0[0x1300] = 1 | 4 << 3;
        put32(&mut m, 0x1304, 0x400);
        put32(&mut m, 0x1308, 0x500);
        put32(&mut m, 0x130c, 0x340);
        m.0[0x1340..0x1346].copy_from_slice(&[1, 0, 1, 0, 0, 0x1a]);
        put64(&mut m, 0x8008, 0x1777);
        put64(&mut m, 0x8020, 0x9000);
        step(&m, &im, &mut c, 0x1520).unwrap();
        assert_eq!((c.pc, c.sp()), (0x1777, 0x9000));
    }
    #[test]
    fn fails_closed_and_preserves_context() {
        for codes in [&[2, 6][..], &[2, 0x11][..], &[2, 0x10][..]] {
            let (m, im, mut c) = setup(codes, 2, 0);
            let original = c.clone();
            assert!(step(&m, &im, &mut c, 0x1520).is_err());
            assert_eq!((c.pc, c.sp()), (original.pc, original.sp()));
        }
        let (mut m, im, mut c) = setup(&[], 0, 0);
        m.0[0x1300] = 3;
        assert!(step(&m, &im, &mut c, 0x1520)
            .unwrap_err()
            .contains("version"));
    }
    #[test]
    fn indexed_body_lea_is_not_an_epilog() {
        let (mut m, im, mut c) = setup(&[5, 0x32, 1, 0x30], 5, 0);
        // lea rax,[rax+rax*4]: ordinary indexed body addressing, not RSP.
        m.0[0x1520..0x1524].copy_from_slice(&[0x48, 0x8d, 0x04, 0x80]);
        assert!(epilog(&m, &im, 0x500, 0x600, &c).unwrap().is_none());
        put64(&mut m, 0x8020, 0xbabe);
        put64(&mut m, 0x8028, 0x1777);
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "unwind_info");
        assert_eq!((c.pc, c.sp(), c.registers[3]), (0x1777, 0x8030, 0xbabe));
        // An indexed LEA actually writing RSP remains unsupported, not guessed.
        c.pc = 0x1520;
        c.registers[4] = 0x8000;
        m.0[0x1522] = 0x24;
        assert!(step(&m, &im, &mut c, 0x1520)
            .unwrap_err()
            .contains("indexed"));
        assert_eq!((c.pc, c.sp()), (0x1520, 0x8000));
    }
    #[test]
    fn ret_imm16_full_and_partial_epilogs_do_not_reapply_prolog() {
        let (mut m, im, initial) = setup(&[5, 0x32, 1, 0x30], 5, 0);
        for immediate in [0u16, 0x20, 0x100] {
            m.0[0x1520..0x1528].copy_from_slice(&[
                0x48,
                0x83,
                0xc4,
                0x20,
                0x5b,
                0xc2,
                immediate as u8,
                (immediate >> 8) as u8,
            ]);
            put64(&mut m, 0x8020, 0xbabe);
            put64(&mut m, 0x8028, 0x1777);
            for (pc, sp) in [(0x1520, 0x8000), (0x1524, 0x8020), (0x1525, 0x8028)] {
                let mut c = initial.clone();
                c.pc = pc;
                c.registers[4] = sp;
                assert_eq!(step(&m, &im, &mut c, pc).unwrap(), "epilog");
                assert_eq!((c.pc, c.sp()), (0x1777, 0x8030 + immediate as u64));
            }
        }
        // A truncated C2 must not silently select full prolog recovery.
        put32(&mut m, 0x1204, 0x526);
        let mut c = initial;
        c.pc = 0x1525;
        c.registers[4] = 0x8028;
        assert!(step(&m, &im, &mut c, 0x1525)
            .unwrap_err()
            .contains("truncated"));
        assert_eq!((c.pc, c.sp()), (0x1525, 0x8028));
    }
    #[test]
    fn indirect_runtime_functions_resolve_primary_ranges_and_prolog_offsets() {
        let (mut m, im, initial) = setup(&[5, 0x32, 1, 0x30], 5, 0);
        // Two indirections to a primary fragment outside the lookup range.
        put32(&mut m, 0x1208, 0x811);
        put32(&mut m, 0x1810, 0x500);
        put32(&mut m, 0x1814, 0x600);
        put32(&mut m, 0x1818, 0x801);
        put32(&mut m, 0x1800, 0x400);
        put32(&mut m, 0x1804, 0x410);
        put32(&mut m, 0x1808, 0x300);
        put64(&mut m, 0x8020, 0xbabe);
        put64(&mut m, 0x8028, 0x1777);
        let mut c = initial.clone();
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "unwind_info");
        assert_eq!((c.pc, c.sp(), c.registers[3]), (0x1777, 0x8030, 0xbabe));
        // A primary after the separated body also means the prolog completed;
        // unsigned subtraction must not panic or turn this into a partial frame.
        put32(&mut m, 0x1800, 0x700);
        put32(&mut m, 0x1804, 0x710);
        let mut c = initial.clone();
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "unwind_info");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x8030));
        // If the primary is before a separated body, all its prolog applies.
        // If the resolved entry covers RIP, retain partial-prolog semantics.
        put32(&mut m, 0x1800, 0x500);
        put32(&mut m, 0x1804, 0x600);
        let mut c = initial;
        c.pc = 0x1501;
        c.registers[4] = 0x8020;
        assert_eq!(step(&m, &im, &mut c, 0x1501).unwrap(), "unwind_info");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x8030));
    }
    #[test]
    fn chained_unwind_can_target_indirect_runtime_function() {
        let (mut m, im, mut c) = setup(&[], 0, 0);
        m.0[0x1300] = 0x21;
        put32(&mut m, 0x1304, 0x400);
        put32(&mut m, 0x1308, 0x410);
        put32(&mut m, 0x130c, 0x801);
        put32(&mut m, 0x1800, 0x400);
        put32(&mut m, 0x1804, 0x410);
        put32(&mut m, 0x1808, 0x340);
        m.0[0x1340..0x1346].copy_from_slice(&[1, 1, 1, 0, 1, 0x32]);
        put64(&mut m, 0x8020, 0x1777);
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "unwind_info");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x8028));
    }
    #[test]
    fn indirect_runtime_functions_fail_closed_on_cycles_bounds_and_alignment() {
        let (mut m, im, initial) = setup(&[], 0, 0);
        for (info, begin, end, next) in [
            (0x801, 0x500, 0x600, 0x801),  // Self-cycle.
            (0xff9, 0x500, 0x600, 0x300),  // 12-byte entry crosses image end.
            (0x803, 0x500, 0x600, 0x300),  // Pointer has a reserved alignment bit.
            (0x801, 0x600, 0x500, 0x300),  // Inverted target range.
            (0x801, 0x500, 0x1001, 0x300), // Range extends outside image.
        ] {
            put32(&mut m, 0x1208, info);
            put32(&mut m, 0x1800, begin);
            put32(&mut m, 0x1804, end);
            put32(&mut m, 0x1808, next);
            let mut c = initial.clone();
            assert!(step(&m, &im, &mut c, 0x1520).is_err());
            assert_eq!((c.pc, c.sp()), (initial.pc, initial.sp()));
        }
        put32(&mut m, 0x1208, 0x801);
        put32(&mut m, 0x1800, 0x500);
        put32(&mut m, 0x1804, 0x600);
        put32(&mut m, 0x1808, 0x811);
        put32(&mut m, 0x1810, 0x500);
        put32(&mut m, 0x1814, 0x600);
        put32(&mut m, 0x1818, 0x801);
        let mut c = initial.clone();
        assert!(step(&m, &im, &mut c, 0x1520)
            .unwrap_err()
            .contains("cyclic"));
        assert_eq!((c.pc, c.sp()), (initial.pc, initial.sp()));
        // Distinct entries still cannot exceed the fixed indirection budget.
        for n in 0..33 {
            let at = 0x1800 + n * 12;
            put32(&mut m, at, 0x500);
            put32(&mut m, at + 4, 0x600);
            put32(
                &mut m,
                at + 8,
                if n == 32 {
                    0x300
                } else {
                    (at + 12 - 0x1000) as u32 | 1
                },
            );
        }
        put32(&mut m, 0x1208, 0x801);
        let mut c = initial;
        assert!(step(&m, &im, &mut c, 0x1520)
            .unwrap_err()
            .contains("excessive"));
        assert_eq!((c.pc, c.sp()), (0x1520, 0x8000));
    }
    #[test]
    fn caller_lookup_uses_previous_byte() {
        let (mut m, im, mut c) = setup(&[1, 0x02], 1, 0);
        c.pc = 0x1600;
        put64(&mut m, 0x8008, 0x1777);
        step(&m, &im, &mut c, 0x15ff).unwrap();
        assert_eq!(c.pc, 0x1777);
    }
    #[test]
    fn v2_live_ke_query_performance_counter_partial_prolog() {
        // Exact ntoskrnl AFF6531F1450000 KeQueryPerformanceCounter root:
        // RUNTIME_FUNCTION 37e7e0..37e7ea, xdata 7b71c:
        // 020a04000a5206e004c00230. Prefix bytes 40 53 / 41 54 / 41 56 /
        // 48 83 ec 30 are push rbx/r12/r14 and sub rsp,48.
        let (mut m, im, initial) = setup(&[10, 0x52, 6, 0xe0, 4, 0xc0, 2, 0x30], 10, 0);
        m.0[0x1300] = 2;
        put32(&mut m, 0x1204, 0x50a);
        for (offset, sp) in [
            (0, 0x8048),
            (1, 0x8048),
            (2, 0x8040),
            (4, 0x8038),
            (6, 0x8030),
            (9, 0x8030),
        ] {
            let mut c = initial.clone();
            c.pc = 0x1500 + offset;
            c.registers[4] = sp;
            put64(&mut m, 0x8030, 0x14);
            put64(&mut m, 0x8038, 0x12);
            put64(&mut m, 0x8040, 0x3);
            put64(&mut m, 0x8048, 0x1777);
            let pc = c.pc;
            assert_eq!(step(&m, &im, &mut c, pc).unwrap(), "unwind_info");
            assert_eq!((c.pc, c.sp()), (0x1777, 0x8050), "offset {offset}");
        }
    }
    #[test]
    fn v2_at_end_scopes_before_during_and_outside() {
        // V2 scopes begin AFTER ADD RSP, not at the stack adjustment.
        // At-end epilog: pop rbx; ret. Include LLVM's zero-offset padding.
        let (mut m, im, initial) = setup(&[2, 0x16, 0, 6, 5, 0x32, 1, 0x30], 5, 0);
        m.0[0x1300] = 2;
        m.0[0x15fa..0x1600].copy_from_slice(&[0x48, 0x83, 0xc4, 0x20, 0x5b, 0xc3]);
        put64(&mut m, 0x8020, 0xbabe);
        put64(&mut m, 0x8028, 0x1777);
        for (pc, sp, method) in [
            (0x1520, 0x8000, "unwind_info"),
            (0x15fa, 0x8000, "unwind_info"),
            (0x15fe, 0x8020, "epilog"),
            (0x15ff, 0x8028, "epilog"),
        ] {
            let mut c = initial.clone();
            c.pc = pc;
            c.registers[4] = sp;
            assert_eq!(step(&m, &im, &mut c, pc).unwrap(), method);
            assert_eq!((c.pc, c.sp()), (0x1777, 0x8030));
        }
        // Return-frame lookup may use PC-1, which is outside the scope even
        // though actual RIP is its first pop. Never apply the full prolog.
        let mut c = initial.clone();
        c.pc = 0x15fe;
        c.registers[4] = 0x8020;
        assert_eq!(step(&m, &im, &mut c, 0x15fd).unwrap(), "epilog");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x8030));
        // A RET-looking byte outside declared scopes must NOT trigger v1
        // instruction scanning (the fixed allocation is still present).
        m.0[0x1520] = 0xc3;
        let mut c = initial;
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "unwind_info");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x8030));
    }
    #[test]
    fn v2_explicit_scopes_12bit_offsets_padding_and_scope_end() {
        // Explicit epilogs at end-0x20 and end-0x123; first slot is size,
        // not a prolog offset. Prolog order checks start after the padding.
        let (mut m, im, initial) =
            setup(&[2, 6, 0x20, 6, 0x23, 0x16, 0, 6, 5, 0x32, 1, 0x30], 5, 0);
        m.0[0x1300] = 2;
        put32(&mut m, 0x1200, 0x400);
        for pc in [0x15e0, 0x15e1, 0x14dd, 0x14de] {
            let mut c = initial.clone();
            c.pc = pc;
            c.registers[4] = if pc & 1 == 0 && pc < 0x1500 || pc == 0x15e1 {
                0x8028
            } else {
                0x8020
            };
            put64(&mut m, 0x8020, 0xbabe);
            put64(&mut m, 0x8028, 0x1777);
            assert_eq!(step(&m, &im, &mut c, pc).unwrap(), "epilog");
            assert_eq!((c.pc, c.sp()), (0x1777, 0x8030));
        }
        // Scope is [start,start+size), even for multi-byte terminators.
        let mut c = initial;
        c.pc = 0x15e2;
        assert_eq!(step(&m, &im, &mut c, 0x15e2).unwrap(), "unwind_info");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x8030));
    }
    #[test]
    fn v2_live_chained_epilog_mirrors_parent_pushes() {
        let (mut m, im, initial) = setup(&[10, 0x52, 6, 0xe0, 4, 0xc0, 2, 0x30], 10, 0);
        m.0[0x1300] = 2;
        // Relocated equivalent of live 2200020006060c06 + parent RF.
        m.0[0x1340..0x1348].copy_from_slice(&[0x22, 0, 2, 0, 6, 6, 12, 6]);
        put32(&mut m, 0x1348, 0x400);
        put32(&mut m, 0x134c, 0x40a);
        put32(&mut m, 0x1350, 0x300);
        put32(&mut m, 0x1208, 0x340);
        put64(&mut m, 0x8030, 0x14);
        put64(&mut m, 0x8038, 0x12);
        put64(&mut m, 0x8040, 0x3);
        put64(&mut m, 0x8048, 0x1777);
        for (pc, sp) in [
            (0x15f4, 0x8030),
            (0x15f6, 0x8038),
            (0x15f8, 0x8040),
            (0x15f9, 0x8048),
        ] {
            let mut c = initial.clone();
            c.pc = pc;
            c.registers[4] = sp;
            assert_eq!(step(&m, &im, &mut c, pc).unwrap(), "epilog");
            assert_eq!((c.pc, c.sp()), (0x1777, 0x8050));
        }
        // Body uses the same parent but unwinds its allocation as well.
        let mut c = initial;
        assert_eq!(step(&m, &im, &mut c, 0x1520).unwrap(), "unwind_info");
        assert_eq!(
            (
                c.pc,
                c.sp(),
                c.registers[14],
                c.registers[12],
                c.registers[3]
            ),
            (0x1777, 0x8050, 0x14, 0x12, 0x3)
        );
    }
    #[test]
    fn v2_epilog_optional_flags_and_machine_frame() {
        let (mut m, im, mut c) = setup(&[3, 0x16, 0, 6, 2, 0x30, 1, 2], 2, 0);
        m.0[0x1300] = 2;
        c.pc = 0x15fd;
        put64(&mut m, 0x8000, 0xbabe);
        put64(&mut m, 0x8010, 0x1777);
        assert_eq!(step(&m, &im, &mut c, 0x15fd).unwrap(), "epilog");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x8018));
        let (mut m, im, mut c) = setup(&[1, 0x16, 0, 6, 0, 0x1a], 0, 0);
        m.0[0x1300] = 2;
        c.pc = 0x15ff;
        // Error code is already removed before entering the declared scope.
        put64(&mut m, 0x8000, 0x1777);
        put64(&mut m, 0x8018, 0x9000);
        assert_eq!(step(&m, &im, &mut c, 0x15ff).unwrap(), "epilog");
        assert_eq!((c.pc, c.sp()), (0x1777, 0x9000));
    }
    #[test]
    fn malformed_v2_scopes_and_unknown_codes_preserve_context() {
        for codes in [
            &[0, 0x16][..],             // Zero size.
            &[1, 0x26][..],             // Reserved initial flags.
            &[1, 6][..],                // No explicit scope and no at-end scope.
            &[3, 6, 2, 6][..],          // Scope extends past function end.
            &[1, 6, 1, 0x16][..],       // Scope begins before function (distance 0x101).
            &[1, 0x16, 0, 6, 2, 6][..], // Descriptor follows padding.
            &[1, 0x16, 0, 6, 1, 7][..], // Spare opcode remains unsupported.
            &[1, 0x16, 0, 6, 1, 6][..], // EPILOG illegally occurs among prolog codes.
        ] {
            let (mut m, im, mut c) = setup(codes, 5, 0);
            m.0[0x1300] = 2;
            let original = c.clone();
            assert!(step(&m, &im, &mut c, 0x1520).is_err(), "{codes:x?}");
            assert_eq!((c.pc, c.sp()), (original.pc, original.sp()));
        }
        // Offset lies in the middle of a two-byte high-register pop.
        let (mut m, im, mut c) = setup(&[3, 0x16, 0, 6, 1, 0xc0], 1, 0);
        m.0[0x1300] = 2;
        c.pc = 0x15fe;
        let original = c.clone();
        assert!(step(&m, &im, &mut c, 0x15fe).is_err());
        assert_eq!((c.pc, c.sp()), (original.pc, original.sp()));
    }
}
