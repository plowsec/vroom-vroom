//! `vroom-fuzz triage`: whether a saved finding comes back when its input
//! runs again, and the least input that still makes it (same signature):
//! cut the tail, zero spans (the layout stays), drop chunks, zero single
//! bytes, keeping each change that still does.

use std::collections::BTreeMap;

use crate::{signature, Engine, Finding, Result};

#[derive(Debug, Clone)]
pub struct TriageOptions {
    /// Runs of the input as given.
    pub runs: usize,
    /// Executions for minimizing at most (0: don't).
    pub budget: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Triage {
    /// The finding triaged: the most frequent first finding over the runs.
    pub signature: String,
    /// Runs that made it, of `runs`.
    pub reproduced: usize,
    pub runs: usize,
    /// Every signature seen in the runs, with how many runs made it.
    pub signatures: BTreeMap<String, usize>,
    pub finding: Option<Finding>,
    pub length: usize,
    pub minimized_length: usize,
    /// Executions spent minimizing.
    pub executions: usize,
}

/// Runs inputs, restoring the snapshot when a run leaves the VM unusable.
struct Runner {
    engine: Option<Engine>,
    executions: usize,
    /// The slowest run, in microseconds.
    slowest: u64,
}

impl Runner {
    fn findings(&mut self, input: &[u8]) -> Result<Vec<Finding>> {
        let engine = self.engine.as_mut().expect("engine is back after a reopen");
        let result = engine.run_input(input)?;
        self.executions += 1;
        self.slowest = self.slowest.max(result.elapsed_us);
        if std::env::var_os("VROOM_FUZZ_TRACE").is_some() {
            eprintln!("{} bytes: {} us, reopen {}, {:?}", input.len(), result.elapsed_us, result.reopen, result.findings.iter().map(signature).collect::<Vec<_>>());
        }
        if result.reopen {
            let engine = self.engine.take().expect("engine is here");
            self.engine = Some(engine.reopen()?);
        }
        Ok(result.findings)
    }

    fn makes(&mut self, input: &[u8], wanted: &str) -> Result<bool> {
        Ok(self.findings(input)?.iter().any(|f| signature(f) == wanted))
    }
}

/// Triage `input`; the minimized input with the report (the input itself
/// when nothing reproduced). `progress` hears about each step.
pub fn triage(engine: Engine, input: &[u8], options: &TriageOptions, mut progress: impl FnMut(&str)) -> Result<(Triage, Vec<u8>)> {
    let keep = if engine.harness.ioctl_prefix { 4.min(input.len()) } else { 0 };
    let mut runner = Runner { engine: Some(engine), executions: 0, slowest: 0 };
    let mut signatures = BTreeMap::<String, usize>::new();
    let mut firsts = BTreeMap::<String, (usize, Finding)>::new();
    for run in 0..options.runs.max(1) {
        let found = runner.findings(input)?;
        let mut seen: Vec<String> = found.iter().map(signature).collect();
        seen.sort();
        seen.dedup();
        for s in seen {
            *signatures.entry(s).or_default() += 1;
        }
        if let Some(first) = found.first() {
            firsts.entry(signature(first)).or_insert((0, first.clone())).0 += 1;
        }
        progress(&format!("run {}: {}", run + 1, found.first().map_or("no finding".to_string(), signature)));
    }
    let runs = options.runs.max(1);
    let Some((wanted, (_, finding))) = firsts.into_iter().max_by_key(|(_, (n, _))| *n) else {
        let report = Triage {
            signature: String::new(),
            reproduced: 0,
            runs,
            signatures,
            finding: None,
            length: input.len(),
            minimized_length: input.len(),
            executions: 0,
        };
        return Ok((report, input.to_vec()));
    };
    let reproduced = signatures[&wanted];
    runner.executions = 0;
    // A candidate far slower than the input is another bug (or none),
    // and a hang costs a snapshot restore anyway: don't wait it out.
    if let Some(engine) = runner.engine.as_mut() {
        let cap = (runner.slowest * 20 / 1000).max(250);
        engine.harness.timeout_ms = engine.harness.timeout_ms.min(cap);
    }
    let mut best = input.to_vec();
    if options.budget > 0 {
        best = minimize(&mut runner, best, keep, &wanted, options.budget, &mut progress)?;
    }
    let report = Triage {
        signature: wanted,
        reproduced,
        runs,
        signatures,
        finding: Some(finding),
        length: input.len(),
        minimized_length: best.len(),
        executions: runner.executions,
    };
    Ok((report, best))
}

fn minimize(runner: &mut Runner, mut best: Vec<u8>, keep: usize, wanted: &str, budget: usize, progress: &mut impl FnMut(&str)) -> Result<Vec<u8>> {
    let spent = |r: &Runner| r.executions >= budget;
    // The shortest prefix that still makes it.
    let (mut lo, mut hi) = (keep, best.len());
    while lo < hi && !spent(runner) {
        let mid = lo + (hi - lo) / 2;
        if runner.makes(&best[..mid], wanted)? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    best.truncate(hi);
    progress(&format!("cut to {} bytes", best.len()));
    zero(runner, &mut best, keep, wanted, &[64, 8], budget)?;
    // Chunks that don't matter, halving.
    let mut chunk = (best.len() - keep) / 2;
    while chunk > 0 && !spent(runner) {
        let mut at = keep;
        while at + chunk <= best.len() && !spent(runner) {
            let mut candidate = best[..at].to_vec();
            candidate.extend_from_slice(&best[at + chunk..]);
            if runner.makes(&candidate, wanted)? {
                best = candidate;
            } else {
                at += chunk;
            }
        }
        chunk /= 2;
    }
    progress(&format!("{} bytes after dropping chunks", best.len()));
    // Bytes that don't matter, zeroed: what's left is what does.
    zero(runner, &mut best, keep, wanted, &[8, 1], budget)?;
    let nonzero = best[keep..].iter().filter(|&&b| b != 0).count();
    progress(&format!("{} bytes, {nonzero} not zero, {} executions", best.len(), runner.executions));
    Ok(best)
}

/// Zero spans of each width that don't matter.
fn zero(runner: &mut Runner, best: &mut Vec<u8>, keep: usize, wanted: &str, widths: &[usize], budget: usize) -> Result<()> {
    for &width in widths {
        let mut at = keep;
        while at < best.len() && runner.executions < budget {
            let end = (at + width).min(best.len());
            if best[at..end].iter().any(|&b| b != 0) {
                let mut candidate = best.clone();
                candidate[at..end].fill(0);
                if runner.makes(&candidate, wanted)? {
                    *best = candidate;
                }
            }
            at = end;
        }
    }
    Ok(())
}
