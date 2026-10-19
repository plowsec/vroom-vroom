//! `vroom-fuzz blocks`: a driver's basic blocks from its file, as a block
//! list: every function `.pdata` lists, swept an instruction at a time;
//! blocks start at the function, at branch targets and after conditional
//! branches; named from its PDB when the symbol server has one. No
//! disassembler, no CFG recovery. What follows `b`, `br` or `ret` starts
//! no block unless a branch goes there: it may be data (MSVC puts switch
//! tables in the code), and a breakpoint there would corrupt it. Code only
//! a jump table reaches counts with the block before it.

use std::collections::BTreeSet;
use std::path::Path;

use vroom_kernel::binary::DiskImage;
use vroom_kernel::symbols::{self, Symbols};

use crate::{CoverageBlock, Error, Result};

/// How an instruction leaves the straight line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// B: only to the target.
    Jump(i64),
    /// B.cond, CBZ/CBNZ, TBZ/TBNZ: the target or the next instruction.
    Branch(i64),
    /// BR, RET, ERET, BRK: nowhere known.
    End,
    Next,
}

fn signed(value: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((value as i64) << shift) >> shift
}

fn flow(word: u32) -> Flow {
    if word & 0xfc00_0000 == 0x1400_0000 {
        Flow::Jump(signed(word & 0x03ff_ffff, 26) * 4)
    } else if word & 0xff00_0010 == 0x5400_0000 || word & 0x7e00_0000 == 0x3400_0000 {
        Flow::Branch(signed((word >> 5) & 0x7ffff, 19) * 4)
    } else if word & 0x7e00_0000 == 0x3600_0000 {
        Flow::Branch(signed((word >> 5) & 0x3fff, 14) * 4)
    } else if word & 0xfe00_0000 == 0xd600_0000 && (word >> 21) & 3 != 1 {
        Flow::End // BR, RET, ERET and their PAC forms (BLR returns)
    } else if word & 0xffe0_001f == 0xd420_0000 {
        Flow::End // BRK
    } else {
        Flow::Next
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("four bytes")))
        .ok_or_else(|| Error::Invalid(format!("image read at {at:#x} out of range")))
}

/// The functions `.pdata` lists: (start RVA, length in bytes).
fn functions(bytes: &[u8]) -> Result<Vec<(usize, usize)>> {
    let pe = u32_at(bytes, 0x3c)? as usize;
    let directory = pe + 24 + 112 + 3 * 8;
    let (rva, size) = (u32_at(bytes, directory)? as usize, u32_at(bytes, directory + 4)? as usize);
    let mut out = Vec::new();
    for entry in (rva..rva + size).step_by(8) {
        let begin = u32_at(bytes, entry)? as usize;
        let unwind = u32_at(bytes, entry + 4)?;
        let length = match unwind & 3 {
            // Packed unwind data: FunctionLength in instructions.
            1 | 2 => ((unwind >> 2) & 0x7ff) as usize * 4,
            0 => (u32_at(bytes, unwind as usize)? & 0x3ffff) as usize * 4,
            _ => continue,
        };
        if length > 0 {
            out.push((begin, length));
        }
    }
    Ok(out)
}

/// The block list of the driver file `path`, at `runtime_base` (its
/// base in the guest), labelled `module`. PDBs go to `cache`.
pub fn blocks(path: &Path, runtime_base: u64, module: &str, cache: &Path) -> Result<Vec<CoverageBlock>> {
    let image = DiskImage::open(path).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    let linked = image.image.base;
    let pdb = image.image.pdb.as_ref().and_then(|id| {
        let file = symbols::fetch(cache, id).ok()?;
        Symbols::load(&file, id, linked).ok()
    });
    let bytes = &image.bytes;
    let mut out = Vec::new();
    for (begin, length) in functions(bytes)? {
        let end = begin + length;
        let mut leaders = BTreeSet::from([begin]);
        let mut exits = Vec::new();
        for at in (begin..end).step_by(4) {
            let f = flow(u32_at(bytes, at)?);
            if f != Flow::Next {
                exits.push((at, f));
            }
            if let Flow::Branch(_) = f {
                leaders.insert(at + 4);
            }
            if let Flow::Jump(d) | Flow::Branch(d) = f {
                let target = at as i64 + d;
                if (begin as i64..end as i64).contains(&target) {
                    leaders.insert(target as usize);
                }
            }
        }
        let leaders: Vec<usize> = leaders.into_iter().filter(|&l| l < end).collect();
        let name = pdb
            .as_ref()
            .and_then(|s| s.addresses.get(&(linked + begin as u64)).cloned())
            .unwrap_or_else(|| format!("sub_{:x}", linked + begin as u64));
        let va = |rva: usize| runtime_base + rva as u64;
        for (i, &start) in leaders.iter().enumerate() {
            let stop = leaders.get(i + 1).copied().unwrap_or(end);
            let last = stop - 4;
            let within = |t: i64| (begin as i64..end as i64).contains(&t).then(|| va(t as usize));
            let successors: Vec<u64> = match exits.iter().find(|(at, _)| *at == last).map(|(_, f)| *f) {
                Some(Flow::Jump(d)) => within(last as i64 + d).into_iter().collect(),
                Some(Flow::Branch(d)) => within(last as i64 + d).into_iter().chain((stop < end).then(|| va(stop))).collect(),
                Some(Flow::End) => Vec::new(),
                _ => (stop < end).then(|| va(stop)).into_iter().collect(),
            };
            out.push(CoverageBlock {
                id: out.len() as u64 + 1,
                gpa: None,
                va: Some(va(start)),
                size: (stop - start) as u32,
                successors,
                module: module.to_string(),
                function: name.clone(),
                source: None,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_are_classified() {
        assert_eq!(flow(0x1400_0004), Flow::Jump(16)); // b +16
        assert_eq!(flow(0x17ff_ffff), Flow::Jump(-4)); // b -4
        assert_eq!(flow(0x5400_0040), Flow::Branch(8)); // b.eq +8
        assert_eq!(flow(0xb400_0060), Flow::Branch(12)); // cbz x0, +12
        assert_eq!(flow(0x3700_0080), Flow::Branch(16)); // tbnz w0, #0, +16
        assert_eq!(flow(0xd65f_03c0), Flow::End); // ret
        assert_eq!(flow(0xd61f_0100), Flow::End); // br x8
        assert_eq!(flow(0xd63f_0100), Flow::Next); // blr x8
        assert_eq!(flow(0xd73f_0900), Flow::Next); // blraa x8, x0
        assert_eq!(flow(0xd65f_0bff), Flow::End); // retaa
        assert_eq!(flow(0x9400_0010), Flow::Next); // bl
        assert_eq!(flow(0xd420_0000), Flow::End); // brk #0
    }
}
