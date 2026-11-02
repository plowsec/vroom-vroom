//! `vroom-fuzz replay`: every input of a corpus once more, each with every
//! block's breakpoint armed (blocks mode), so each gets its whole coverage,
//! not only what was new to the campaign. Written as drcov (Lighthouse,
//! Lightkeeper, bncov): one file per input and one for them all.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::{signature, CoverageMap, CoverageMode, Engine, Error, Result};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Replayed {
    pub input: String,
    pub blocks: usize,
    /// Blocks no input before this one reached.
    pub new: usize,
    pub elapsed_us: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
}

/// drcov 2, one module: `blocks` are (offset from `base`, size).
pub fn drcov(module: &str, base: u64, end: u64, blocks: &BTreeSet<(u32, u16)>) -> Vec<u8> {
    let mut out = format!(
        "DRCOV VERSION: 2\nDRCOV FLAVOR: drcov\nModule Table: version 2, count 1\n\
         Columns: id, base, end, entry, checksum, timestamp, path\n\
         0, {base:#018x}, {end:#018x}, 0x0000000000000000, 0x00000000, 0x00000000, {module}\n\
         BB Table: {} bbs\n",
        blocks.len()
    )
    .into_bytes();
    for &(offset, size) in blocks {
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out
}

/// Replay `inputs` (name, bytes) into `out`, after a pass that warms the
/// VM up: `<name>.drcov` each, `all.drcov`, and `replay.json` (what each
/// reached, in order).
pub fn replay(
    mut engine: Engine,
    inputs: &[(String, Vec<u8>)],
    out: &Path,
    mut progress: impl FnMut(&str),
) -> Result<Vec<Replayed>> {
    if engine.harness.coverage_mode != CoverageMode::Blocks {
        return Err(Error::Invalid("replay needs coverage_mode blocks".into()));
    }
    let (base, end) = engine.target_module()?;
    let module = engine.harness.target.clone().unwrap_or_else(|| {
        engine.harness.coverage.first().map(|b| b.module.clone()).unwrap_or_else(|| "target".into())
    });
    let index: HashMap<u64, usize> = engine.harness.coverage.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
    std::fs::create_dir_all(out)?;
    let mut all = BTreeSet::new();
    let mut report = Vec::new();
    // What an input touches first (snapshot pages loaded on demand) takes
    // host time the guest sees: replies come late, paths differ. A pass
    // over them all before the one that counts.
    for (n, (_, data)) in inputs.iter().enumerate() {
        engine = run(engine, data, &index)?.0;
        if (n + 1) % 100 == 0 {
            progress(&format!("warm-up: {}/{}", n + 1, inputs.len()));
        }
    }
    for (n, (name, data)) in inputs.iter().enumerate() {
        let (next, hit, result) = run(engine, data, &index)?;
        engine = next;
        let blocks: BTreeSet<(u32, u16)> = hit
            .iter()
            .filter_map(|&i| {
                let b = &engine.harness.coverage[i];
                let va = b.va.filter(|va| (base..end).contains(va))?;
                Some(((va - base) as u32, b.size.min(u32::from(u16::MAX)) as u16))
            })
            .collect();
        let new = blocks.difference(&all).count();
        all.extend(blocks.iter().copied());
        std::fs::write(out.join(format!("{name}.drcov")), drcov(&module, base, end, &blocks))?;
        let findings: Vec<String> = result.findings.iter().map(signature).collect();
        progress(&format!(
            "{}/{}: {name}: {} blocks, {new} new, {} in all{}",
            n + 1,
            inputs.len(),
            blocks.len(),
            all.len(),
            if findings.is_empty() { String::new() } else { format!(", {}", findings.join(" ")) }
        ));
        report.push(Replayed { input: name.clone(), blocks: blocks.len(), new, elapsed_us: result.elapsed_us, calls: result.calls, findings });
    }
    std::fs::write(out.join("all.drcov"), drcov(&module, base, end, &all))?;
    std::fs::write(out.join("replay.json"), serde_json::to_vec_pretty(&report)?)?;
    Ok(report)
}

/// One input; then the breakpoints of the blocks it took are back, so the
/// next one starts from all of them.
fn run(mut engine: Engine, data: &[u8], index: &HashMap<u64, usize>) -> Result<(Engine, Vec<usize>, crate::RunResult)> {
    let result = engine.run_input(data)?;
    let hit: Vec<usize> = result.block_hits.keys().filter_map(|id| index.get(id).copied()).collect();
    if result.reopen {
        engine = engine.reopen()?;
    }
    engine.coverage = CoverageMap::default();
    for &i in &hit {
        engine.rearm(i)?;
    }
    Ok((engine, hit, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drcov_is_a_header_and_packed_blocks() {
        let blocks = BTreeSet::from([(0x10, 4), (0x2000, 12)]);
        let d = drcov("app.dll", 0x7ff0_0000_0000, 0x7ff0_0010_0000, &blocks);
        let text = String::from_utf8_lossy(&d);
        assert!(text.starts_with("DRCOV VERSION: 2\n"));
        assert!(text.contains("0, 0x00007ff000000000, 0x00007ff000100000, 0x0000000000000000, 0x00000000, 0x00000000, app.dll\n"));
        assert!(text.contains("BB Table: 2 bbs\n"));
        let tail = &d[d.len() - 16..];
        assert_eq!(tail, [0x10, 0, 0, 0, 4, 0, 0, 0, 0, 0x20, 0, 0, 12, 0, 0, 0]);
    }
}
