//! `vroom-fuzz triage`: whether a saved finding comes back when its input
//! runs again, and the least input that still makes it (same signature).
//! A sequence first loses whole calls (the shortest prefix, then each
//! call), then its arguments get simpler by type (described calls); then,
//! as any input: cut the tail, zero spans (the layout stays), drop
//! chunks, zero single bytes, keeping each change that still does.

use std::collections::BTreeMap;

use crate::calls::{Calls, Type, Value};
use crate::{sequence, signature, Engine, Finding, Result};

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
    /// The minimized program, a call a line (sequence harnesses).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub program: Vec<String>,
}

/// What minimizing asks of an input: does it still make the finding.
trait Oracle {
    fn makes(&mut self, input: &[u8]) -> Result<bool>;
    fn executions(&self) -> usize;
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

}

/// A runner asked about one signature.
struct Wanted<'a> {
    runner: &'a mut Runner,
    signature: &'a str,
}

impl Oracle for Wanted<'_> {
    fn makes(&mut self, input: &[u8]) -> Result<bool> {
        Ok(self.runner.findings(input)?.iter().any(|f| signature(f) == self.signature))
    }

    fn executions(&self) -> usize {
        self.runner.executions
    }
}

/// Triage `input`; the minimized input with the report (the input itself
/// when nothing reproduced). `progress` hears about each step.
pub fn triage(engine: Engine, input: &[u8], options: &TriageOptions, mut progress: impl FnMut(&str)) -> Result<(Triage, Vec<u8>)> {
    let keep = if engine.harness.ioctl_prefix { 4.min(input.len()) } else { 0 };
    let sequence = engine.harness.sequence;
    let calls = if engine.harness.calls.is_empty() { None } else { Some(Calls::new(engine.harness.calls.clone())?) };
    let program = |input: &[u8]| if sequence { crate::calls::show(calls.as_ref(), input) } else { Vec::new() };
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
            program: program(input),
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
        let mut oracle = Wanted { runner: &mut runner, signature: &wanted };
        if sequence {
            best = minimize_calls(&mut oracle, calls.as_ref(), best, options.budget, &mut progress)?;
        }
        best = minimize(&mut oracle, best, keep, options.budget, &mut progress)?;
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
        program: program(&best),
    };
    Ok((report, best))
}

/// A sequence with fewer calls (the shortest prefix, then each call
/// dropped, the last first), then simpler arguments.
fn minimize_calls(o: &mut impl Oracle, calls: Option<&Calls>, input: Vec<u8>, budget: usize, progress: &mut impl FnMut(&str)) -> Result<Vec<u8>> {
    let mut p = crate::calls(&input);
    if sequence(&p) != input && !o.makes(&sequence(&p))? {
        return Ok(input);
    }
    let (mut lo, mut hi) = (0, p.len());
    while lo < hi && o.executions() < budget {
        let mid = lo + (hi - lo) / 2;
        if o.makes(&sequence(&p[..mid]))? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    p.truncate(hi);
    let mut i = p.len();
    while i > 0 && o.executions() < budget {
        i -= 1;
        let mut q = p.clone();
        q.remove(i);
        if o.makes(&sequence(&q))? {
            p = q;
        }
    }
    progress(&format!("{} calls", p.len()));
    let mut best = sequence(&p);
    if let Some(c) = calls {
        best = simplify(o, c, &best, budget)?;
        progress(&format!("{} bytes after simpler arguments", best.len()));
    }
    Ok(best)
}

/// Each argument as simple as its type allows while the finding stays:
/// ints and inline values zero, flags fewer, bytes shorter, arrays'
/// elements zero, no bytes past the arguments. Lens keep their values.
fn simplify(o: &mut impl Oracle, c: &Calls, input: &[u8], budget: usize) -> Result<Vec<u8>> {
    let mut p = c.decode(input);
    for i in 0..p.len() {
        let Some(d) = p[i].desc else { continue };
        for j in 0..c.descs[d].args.len() {
            // Again from what was kept: bytes halve until they can't.
            'simpler: loop {
                for v in simpler(&c.descs[d].args[j].ty, &p[i].args[j]) {
                    if o.executions() >= budget {
                        return Ok(c.encode(&p));
                    }
                    let mut q = p.clone();
                    q[i].args[j] = v;
                    c.fit_arrays(&mut q[i], zero);
                    if o.makes(&c.encode(&q))? {
                        p = q;
                        continue 'simpler;
                    }
                }
                break;
            }
        }
        if !p[i].extra.is_empty() && o.executions() < budget {
            let mut q = p.clone();
            q[i].extra.clear();
            if o.makes(&c.encode(&q))? {
                p = q;
            }
        }
    }
    Ok(c.encode(&p))
}

/// Simpler values of an argument, simplest first.
fn simpler(ty: &Type, v: &Value) -> Vec<Value> {
    match (ty, v) {
        (Type::Int { .. }, Value::Int(raw)) if *raw != 0 => vec![Value::Int(0)],
        (Type::Flags { .. }, Value::Int(raw)) if *raw != 0 => {
            let mut out = vec![Value::Int(0)];
            if raw.count_ones() > 1 {
                out.extend((0..64).filter(|b| raw >> b & 1 != 0).map(|b| Value::Int(raw & !(1 << b))));
            }
            out
        }
        (Type::Res { .. }, Value::Res(raw, Some(value))) if *value != 0 => vec![Value::Res(*raw, Some(0))],
        (Type::Array { elem, .. }, Value::Array(items)) => (0..items.len())
            .filter(|&k| items[k] != zero(elem))
            .map(|k| {
                let mut items = items.clone();
                items[k] = zero(elem);
                Value::Array(items)
            })
            .collect(),
        (Type::Bytes { min, .. }, Value::Bytes(bytes)) if bytes.len() > *min => {
            vec![Value::Bytes(vec![0; *min]), Value::Bytes(bytes[..(bytes.len() / 2).max(*min)].to_vec())]
        }
        _ => Vec::new(),
    }
}

/// An argument's simplest value.
fn zero(ty: &Type) -> Value {
    match ty {
        Type::Const { value, .. } => Value::Int(*value),
        Type::Res { tag, .. } => Value::Res(*tag, None),
        Type::Array { .. } => Value::Array(Vec::new()),
        Type::Bytes { min, .. } => Value::Bytes(vec![0; *min]),
        _ => Value::Int(0),
    }
}

fn minimize(o: &mut impl Oracle, mut best: Vec<u8>, keep: usize, budget: usize, progress: &mut impl FnMut(&str)) -> Result<Vec<u8>> {
    let spent = |o: &dyn Oracle| o.executions() >= budget;
    // The shortest prefix that still makes it.
    let (mut lo, mut hi) = (keep, best.len());
    while lo < hi && !spent(o) {
        let mid = lo + (hi - lo) / 2;
        if o.makes(&best[..mid])? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    best.truncate(hi);
    progress(&format!("cut to {} bytes", best.len()));
    zero_spans(o, &mut best, keep, &[64, 8], budget)?;
    // Chunks that don't matter, halving.
    let mut chunk = (best.len() - keep) / 2;
    while chunk > 0 && !spent(o) {
        let mut at = keep;
        while at + chunk <= best.len() && !spent(o) {
            let mut candidate = best[..at].to_vec();
            candidate.extend_from_slice(&best[at + chunk..]);
            if o.makes(&candidate)? {
                best = candidate;
            } else {
                at += chunk;
            }
        }
        chunk /= 2;
    }
    progress(&format!("{} bytes after dropping chunks", best.len()));
    // Bytes that don't matter, zeroed: what's left is what does.
    zero_spans(o, &mut best, keep, &[8, 1], budget)?;
    let nonzero = best[keep..].iter().filter(|&&b| b != 0).count();
    progress(&format!("{} bytes, {nonzero} not zero, {} executions", best.len(), o.executions()));
    Ok(best)
}

/// Zero spans of each width that don't matter.
fn zero_spans(o: &mut impl Oracle, best: &mut Vec<u8>, keep: usize, widths: &[usize], budget: usize) -> Result<()> {
    for &width in widths {
        let mut at = keep;
        while at < best.len() && o.executions() < budget {
            let end = (at + width).min(best.len());
            if best[at..end].iter().any(|&b| b != 0) {
                let mut candidate = best.clone();
                candidate[at..end].fill(0);
                if o.makes(&candidate)? {
                    *best = candidate;
                }
            }
            at = end;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::tests::sample;
    use crate::calls::Call;

    /// A finding when an append with an `A` in its data writes to an area
    /// an earlier call made.
    struct Fake {
        calls: Calls,
        executions: usize,
    }

    impl Oracle for Fake {
        fn makes(&mut self, input: &[u8]) -> Result<bool> {
            self.executions += 1;
            let p = self.calls.decode(input);
            let name = |c: &Call| c.desc.map(|d| self.calls.descs[d].name.as_str());
            Ok(p.iter().enumerate().any(|(i, c)| {
                name(c) == Some("append")
                    && matches!(&c.args[4], Value::Bytes(b) if b.contains(&b'A'))
                    && p[..i].iter().any(|e| name(e) == Some("area") && e.args[1] == c.args[0])
            }))
        }

        fn executions(&self) -> usize {
            self.executions
        }
    }

    #[test]
    fn sequences_lose_calls_then_argument_values() {
        let calls = sample();
        let input = sequence(&[
            (0, vec![1, 2]),
            (3, vec![1, 4, 0, 16, 0, 0]),
            (6, vec![4, 3]),
            (3, vec![1, 5, 7, 0, 0, 0]),
            (4, vec![4, 6, 3, 0x81, 9, 9, 9, 9, 9, 9, 9, 9, b'A', 1, 2, 3, 4, 5, 6, 7]),
            (7, vec![0x84]),
            (99, vec![1, 2, 3]),
        ]);
        let mut o = Fake { calls: sample(), executions: 0 };
        assert!(o.makes(&input).unwrap());
        let best = minimize_calls(&mut o, Some(&calls), input, 1000, &mut |_| {}).unwrap();
        assert_eq!(
            crate::calls::show(Some(&calls), &best),
            ["area(log=log1, slot=area4, size=0)", "append(area=area4, flags=0x0, into=lsn3, undo=0x0, data=41 (1))"]
        );
    }
}
