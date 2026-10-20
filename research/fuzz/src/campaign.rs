//! One fuzzing instance: picks inputs (scheduled seeds, then mutations of
//! the corpus weighted by recent novelty), runs them, keeps the novel ones
//! and recovers the VM. Instances share corpus entries through a `Hub`.

use crate::hub::{now, Entry, FindingReport, InstanceStats};
use crate::{cmp_fields, cmplog, signature, BugClass, CorpusStore, CoverageBlock, Engine, Error, Mutator, Result};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct CampaignOptions {
    /// Instance name in the hub and on the dashboard.
    pub name: String,
    /// Fix the IOCTL and treat the seed as payload bytes.
    pub ioctl: Option<u32>,
    pub boundary_seeds: bool,
    pub repeat_seed: bool,
    /// Log each new corpus input's comparisons (Redqueen-style).
    pub cmplog: bool,
    /// Mixed into the mutator seed so parallel instances don't all try the
    /// same mutations. Zero keeps a single run reproducible.
    pub salt: u64,
    /// Each result as a JSON line on stdout, progress on stderr.
    pub verbose: bool,
}

/// Parent weights: energy (novelty) scaled by speed against the corpus
/// median, from 1/16 (16x slower) to 4x (4x faster). A handler that loops
/// for seconds is picked rarely; inputs not timed yet count as typical.
fn speed_weights(energy: &[u32], us: &[u64]) -> Vec<u32> {
    let mut timed: Vec<u64> = us.iter().copied().filter(|&t| t != 0).collect();
    timed.sort_unstable();
    let mean = timed.get(timed.len() / 2).copied().unwrap_or(1).max(1);
    energy
        .iter()
        .zip(us)
        .map(|(&e, &t)| {
            // A parent 100 times slower than most is 100 times less
            // worth mutating: what it finds costs that much.
            let t = if t == 0 { mean } else { t.clamp(mean / 4, mean * 256).max(1) };
            ((u64::from(e) * 16 * mean / t) as u32).max(1)
        })
        .collect()
}

/// Rate window for `execs_per_sec`.
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// Comparison logging: target instructions stepped at most per input, and
/// the time it may take (each step ~0.5 ms; a client's program also runs
/// at full speed between its entries into the target).
const CMP_STEPS: usize = 40_000;
const CMP_BUDGET: Duration = Duration::from_millis(200);
const CMP_CLIENT_BUDGET: Duration = Duration::from_secs(3);
/// A loop compares at one site again and again: its first few pairs.
const CMP_PER_SITE: usize = 4;
/// Inputs one comparison log may suggest; a sequence's (a program that
/// runs ~30 ms) fewer: its direct matches come first.
const CMP_CANDIDATES: usize = 512;
const CMP_SEQUENCE_CANDIDATES: usize = 128;
/// Inputs between learning the call priorities again (a grown corpus).
const PRIORITIES_EVERY: usize = 500;

#[derive(Debug)]
pub struct Campaign {
    engine: Option<Engine>,
    store: CorpusStore,
    root: PathBuf,
    options: CampaignOptions,
    seed: Vec<u8>,
    prefix_len: usize,
    fixed_prefix: Option<Vec<u8>>,
    corpus_inputs: Vec<Vec<u8>>,
    corpus_energy: Vec<u32>,
    /// Each corpus input's last run time (µs; 0: not run here yet).
    corpus_us: Vec<u64>,
    /// A sequence's calls' results when it last ran here (empty: not run
    /// here yet, or no results).
    corpus_calls: Vec<Vec<u32>>,
    corpus_hashes: BTreeSet<String>,
    scheduled: VecDeque<Vec<u8>>,
    /// Corpus inputs whose comparisons haven't been logged yet.
    cmp_queue: VecDeque<Vec<u8>>,
    /// Inputs comparison logging suggested, once each.
    cmp_tried: BTreeSet<String>,
    mutator: Mutator,
    /// Corpus inputs and the iteration when the call priorities were last
    /// learned.
    priorities: (usize, usize),
    finding_novelty: BTreeSet<String>,
    iteration: usize,
    recoveries: usize,
    inputs_since_reopen: usize,
    started: Instant,
    stats: InstanceStats,
    rate: VecDeque<(Instant, u64)>,
    outbox: Vec<Entry>,
    found: Vec<FindingReport>,
}

impl Campaign {
    pub fn new(
        engine: Engine,
        root: PathBuf,
        seed: Vec<u8>,
        options: CampaignOptions,
    ) -> Result<Self> {
        let store = CorpusStore::new(&root)?;
        let mut seed = seed;
        if let Some(ioctl) = options.ioctl {
            if !engine.harness.ioctl_prefix {
                return Err(Error::Invalid(
                    "--ioctl requires ioctl_prefix: true in the harness".into(),
                ));
            }
            let mut prefixed = ioctl.to_le_bytes().to_vec();
            prefixed.extend_from_slice(&seed);
            seed = prefixed;
        }
        if seed.len() > engine.harness.max_input {
            return Err(Error::Invalid("seed exceeds max_input".into()));
        }
        if engine.harness.ioctl_prefix && seed.len() < 4 {
            return Err(Error::Invalid(
                "IOCTL-prefixed seed is shorter than four bytes".into(),
            ));
        }
        let mut corpus_inputs = store.load_inputs()?;
        let prefix_len = if engine.harness.ioctl_prefix { 4 } else { 0 };
        let fixed_prefix = options.ioctl.map(|_| seed[..prefix_len].to_vec());
        corpus_inputs.retain(|input| {
            input.len() <= engine.harness.max_input
                && input.len() >= prefix_len
                && fixed_prefix
                    .as_ref()
                    .map_or(true, |prefix| input.starts_with(prefix))
        });
        let mut corpus_energy = vec![1u32; corpus_inputs.len()];
        let mut corpus_hashes = corpus_inputs
            .iter()
            .map(|input| CorpusStore::hash(input))
            .collect::<BTreeSet<_>>();
        if corpus_hashes.insert(CorpusStore::hash(&seed)) {
            store.save_input(&seed, 0)?;
            corpus_inputs.push(seed.clone());
            corpus_energy.push(1);
        }
        // The seed, then the corpus as it was: a resumed campaign knows its
        // coverage again (and its master learns it) before mutating.
        let mut scheduled = VecDeque::from([seed.clone()]);
        scheduled.extend(corpus_inputs.iter().filter(|input| **input != seed).cloned());
        if options.boundary_seeds {
            let max_payload_len =
                (engine.harness.max_input - prefix_len).min(engine.harness.input_capacity);
            let boundaries = [
                0, 1, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2047, 2048, 2049, 3072,
            ];
            for payload_len in boundaries
                .into_iter()
                .chain(std::iter::once(max_payload_len))
            {
                if payload_len > max_payload_len {
                    continue;
                }
                let mut input = seed[..prefix_len].to_vec();
                input.resize(prefix_len + payload_len, b'A');
                if corpus_hashes.insert(CorpusStore::hash(&input)) {
                    store.save_input(&input, 0)?;
                    corpus_inputs.push(input.clone());
                    corpus_energy.push(1);
                    scheduled.push_back(input);
                }
            }
        }
        let mut mutator = Mutator::new(0x0076_326f_6f6d_u64 ^ options.salt, engine.harness.max_input);
        if !engine.harness.calls.is_empty() {
            mutator.describe(crate::calls::Calls::new(engine.harness.calls.clone())?, &corpus_inputs);
        }
        let priorities_corpus = corpus_inputs.len();
        let cmp_queue = VecDeque::from([seed.clone()]);
        let stats = InstanceStats {
            name: options.name.clone(),
            started: now(),
            ..Default::default()
        };
        Ok(Self {
            engine: Some(engine),
            store,
            root,
            options,
            seed,
            prefix_len,
            fixed_prefix,
            corpus_inputs,
            corpus_us: vec![0; corpus_energy.len()],
            corpus_calls: vec![Vec::new(); corpus_energy.len()],
            corpus_energy,
            corpus_hashes,
            scheduled,
            cmp_queue,
            cmp_tried: BTreeSet::new(),
            mutator,
            priorities: (priorities_corpus, 0),
            finding_novelty: BTreeSet::new(),
            iteration: 0,
            recoveries: 0,
            inputs_since_reopen: 0,
            started: Instant::now(),
            stats,
            rate: VecDeque::new(),
            outbox: Vec::new(),
            found: Vec::new(),
        })
    }

    pub fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("engine present between steps")
    }

    fn engine_mut(&mut self) -> &mut Engine {
        self.engine.as_mut().expect("engine present between steps")
    }

    pub fn blocks(&self) -> &[CoverageBlock] {
        &self.engine().harness.coverage
    }

    pub fn iterations(&self) -> usize {
        self.iteration
    }

    /// Log `parent`'s comparisons and queue the inputs that would turn them
    /// the other way; keep the compared values for mutation.
    fn compare(&mut self, parent: &[u8]) {
        let fields = cmp_fields(&self.engine().harness, parent);
        let budget = if self.engine().harness.client() { CMP_CLIENT_BUDGET } else { CMP_BUDGET };
        let cmps = match self.engine_mut().compare_log(parent, CMP_STEPS, budget) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("comparison logging: {e}");
                return;
            }
        };
        // Blocks only the logged run reached (it runs the whole input,
        // other threads included): the parent covers them; share them like
        // a new input's, or they'd be covered here and nowhere else.
        let reached = std::mem::take(&mut self.engine_mut().cmplog_blocks);
        if !reached.is_empty() {
            self.stats.last_new_cov = Some(now());
            self.outbox.push(Entry {
                hash: CorpusStore::hash(parent),
                data: parent.to_vec(),
                blocks: Some(reached),
                edges: Vec::new(),
                findings: Vec::new(),
            });
        }
        let mut per_site = HashMap::<u64, usize>::new();
        let cmps: Vec<_> = cmps
            .into_iter()
            .filter(|c| {
                let n = per_site.entry(c.pc).or_default();
                *n += 1;
                *n <= CMP_PER_SITE
            })
            .collect();
        for c in &cmps {
            if let Ok(value) = u32::try_from(c.b) {
                self.mutator.learn(value);
            }
        }
        let sequence = self.engine().harness.sequence;
        let cap = if sequence { CMP_SEQUENCE_CANDIDATES } else { CMP_CANDIDATES };
        for input in cmplog::candidates(parent, &cmps, &fields, sequence, cap) {
            if self.fixed_prefix.as_ref().is_some_and(|p| !input.starts_with(p)) {
                continue;
            }
            let hash = CorpusStore::hash(&input);
            if !self.corpus_hashes.contains(&hash) && self.cmp_tried.insert(hash) {
                self.scheduled.push_back(input);
            }
        }
        self.stats.cmplog += 1;
    }

    /// Run one input.
    pub fn step(&mut self) -> Result<()> {
        if self.scheduled.is_empty() && self.options.cmplog && !self.options.repeat_seed {
            if let Some(parent) = self.cmp_queue.pop_front() {
                self.compare(&parent);
            }
        }
        let mut selected_parent = None;
        let input = if let Some(input) = self.scheduled.pop_front() {
            input
        } else if self.options.repeat_seed || self.corpus_inputs.is_empty() {
            // An empty corpus: its inputs timed out when run again (a
            // worker's seed too), the master's not imported yet.
            self.seed.clone()
        } else {
            let weights = speed_weights(&self.corpus_energy, &self.corpus_us);
            let parent_index = self.mutator.choose_weighted(&weights);
            selected_parent = Some(parent_index);
            self.corpus_energy[parent_index] =
                self.corpus_energy[parent_index].saturating_sub(1).max(1);
            if self.iteration >= self.priorities.1 + PRIORITIES_EVERY && self.corpus_inputs.len() != self.priorities.0 {
                self.mutator.learn_priorities(&self.corpus_inputs);
                self.priorities = (self.corpus_inputs.len(), self.iteration);
            }
            let parent = &self.corpus_inputs[parent_index];
            if self.engine().harness.sequence {
                let d = self.mutator.choose(self.corpus_inputs.len());
                let parent = (parent.as_slice(), self.corpus_calls[parent_index].as_slice());
                let donor = (self.corpus_inputs[d].as_slice(), self.corpus_calls[d].as_slice());
                self.mutator.mutate_sequence(parent, donor)
            } else if self.prefix_len == 4 && self.fixed_prefix.is_none() {
                self.mutator.mutate_ioctl(parent)
            } else {
                self.mutator.mutate_preserving_prefix(parent, self.prefix_len)
            }
        };
        let result = self.engine_mut().run_input(&input)?;
        if selected_parent.is_none() && !result.calls.is_empty() {
            // A corpus input run again (a resumed or imported one).
            if let Some(i) = self.corpus_inputs.iter().position(|c| *c == input) {
                self.corpus_calls[i] = result.calls.clone();
            }
        }
        let mut new_signatures = Vec::new();
        for finding in &result.findings {
            let signature = signature(finding);
            if self.finding_novelty.insert(signature.clone()) {
                new_signatures.push(signature.clone());
                self.found.push(FindingReport {
                    signature,
                    finding: finding.clone(),
                    data: input.clone(),
                });
            }
        }
        let new_findings = new_signatures.len();
        let novel = result.new_blocks != 0 || result.new_edges != 0 || new_findings != 0;
        if novel {
            let score = (1
                + result.new_blocks as u32 * 16
                + result.new_edges as u32 * 4
                + new_findings as u32 * 8)
                .min(256);
            if self.corpus_hashes.insert(result.input_hash.clone()) {
                self.store.save_input(&input, result.new_blocks)?;
                self.corpus_inputs.push(input.clone());
                self.corpus_energy.push(score);
                self.corpus_us.push(result.elapsed_us);
                self.corpus_calls.push(result.calls.clone());
            } else if let Some(index) = self
                .corpus_inputs
                .iter()
                .position(|candidate| CorpusStore::hash(candidate) == result.input_hash)
            {
                self.corpus_energy[index] = (self.corpus_energy[index] + score).min(256);
            }
            if let Some(parent) = selected_parent {
                self.corpus_energy[parent] =
                    (self.corpus_energy[parent] + 4 + new_findings as u32 * 4).min(256);
            }
            if result.new_blocks != 0 || result.new_edges != 0 {
                self.stats.last_new_cov = Some(now());
                self.cmp_queue.push_back(input.clone());
            }
            self.outbox.push(Entry {
                hash: result.input_hash.clone(),
                data: input.clone(),
                blocks: Some(result.block_hits.keys().copied().collect()),
                edges: result.edges_hit.iter().copied().collect(),
                findings: result.findings.iter().map(signature).collect(),
            });
        }
        // One input per signature: the first that made it.
        for finding in result.findings.iter().filter(|f| new_signatures.contains(&signature(f))) {
            self.store.save_finding(finding, &input)?;
        }
        self.stats.execs += 1;
        if !result.findings.is_empty() {
            self.stats.crashes += 1;
        }
        if result
            .findings
            .iter()
            .any(|finding| finding.class == BugClass::Timeout)
        {
            self.stats.timeouts += 1;
        }
        self.stats.foreign_threads += result.foreign_threads;
        // Recent inputs weigh most: a mean over the last ~50.
        let average = |old: u64, new: u64| {
            if old == 0 {
                new
            } else {
                (old * 49 + new) / 50
            }
        };
        self.stats.exec_us = average(self.stats.exec_us, result.elapsed_us);
        self.stats.reset_us = average(self.stats.reset_us, result.reset_us);
        if self.options.verbose {
            println!("{}", serde_json::to_string(&result)?);
            std::io::stdout().flush()?;
            if self.iteration % 25 == 0 {
                eprintln!(
                    "progress: {} inputs, {} blocks, {} corpus inputs, {} finding signatures",
                    self.iteration + 1,
                    self.engine().coverage.covered(),
                    self.corpus_inputs.len(),
                    self.finding_novelty.len()
                );
            }
        }
        // A bugcheck stopped at `bugcheck_pc` resets in place like a clean
        // input; a timeout or a guest reset restores from disk. A client's
        // timeout (its calls blocked; the whole guest runs, every reset
        // restores RAM, CPUs, devices and disks) resets in place too: a
        // restore brings back nothing more, costs ~15 s and reads the
        // snapshot's RAM again (overnight, runs of timeouts made restores
        // back to back, 3 GB/s of reads, and stalled every worker).
        let timeout = result.findings.iter().any(|f| f.class == BugClass::Timeout);
        if let (true, Some(parent)) = (timeout, selected_parent) {
            // Its children run out of time: a parent near the limit.
            self.corpus_energy[parent] = 1;
        }
        if timeout && selected_parent.is_none() {
            // A corpus input run again (a resumed campaign) that no longer
            // finishes in time: every resume would wait it out again.
            if let Some(i) = self.corpus_inputs.iter().position(|c| *c == input) {
                self.corpus_inputs.remove(i);
                self.corpus_energy.remove(i);
                self.corpus_us.remove(i);
                self.corpus_calls.remove(i);
                self.store.remove_input(&input)?;
            }
        }
        let in_place = timeout && self.engine().harness.client();
        if result.reopen && !in_place {
            if self.options.verbose {
                eprintln!(
                    "restoring the clean snapshot after {:?} ({})",
                    result.finding.as_ref().map(|finding| finding.class),
                    self.iteration + 1
                );
            }
            self.reopen()?;
        } else {
            self.inputs_since_reopen += 1;
            let reopen_after = self.engine().harness.reopen_after;
            if reopen_after != 0 && self.inputs_since_reopen >= reopen_after {
                if self.options.verbose {
                    eprintln!(
                        "reopening the snapshot after {} inputs",
                        self.inputs_since_reopen
                    );
                }
                self.reopen()?;
            }
        }
        self.iteration += 1;
        Ok(())
    }

    fn reopen(&mut self) -> Result<()> {
        let engine = self.engine.take().expect("engine present between steps");
        self.engine = Some(engine.reopen()?);
        self.recoveries += 1;
        self.stats.restores += 1;
        self.inputs_since_reopen = 0;
        Ok(())
    }

    /// Take an entry another instance found. Known coverage joins the corpus
    /// when it is new here, as if this instance had found it; an entry
    /// without coverage is run like a seed.
    pub fn import(&mut self, entry: Entry) -> Result<()> {
        if entry.data.len() > self.engine().harness.max_input
            || entry.data.len() < self.prefix_len
            || self
                .fixed_prefix
                .as_ref()
                .is_some_and(|prefix| !entry.data.starts_with(prefix))
            || self.corpus_hashes.contains(&entry.hash)
        {
            return Ok(());
        }
        let Some(blocks) = &entry.blocks else {
            self.scheduled.push_back(entry.data);
            return Ok(());
        };
        let new_findings = entry
            .findings
            .iter()
            .filter(|signature| self.finding_novelty.insert((*signature).clone()))
            .count();
        let (new_blocks, new_edges) =
            self.engine_mut()
                .coverage
                .merge(blocks, &entry.edges, &entry.hash);
        // Blocks mode: no breakpoint needed where the others have been.
        self.engine_mut().take_coverage(blocks)?;
        if new_blocks == 0 && new_edges == 0 && new_findings == 0 {
            return Ok(());
        }
        let score =
            (1 + new_blocks as u32 * 16 + new_edges as u32 * 4 + new_findings as u32 * 8).min(256);
        self.store.save_input(&entry.data, new_blocks)?;
        self.corpus_hashes.insert(entry.hash);
        if !self.engine().harness.calls.is_empty() {
            // Run once for its calls' results (typed mutation's slots).
            self.scheduled.push_back(entry.data.clone());
        }
        self.corpus_inputs.push(entry.data);
        self.corpus_energy.push(score);
        self.corpus_us.push(0);
        self.corpus_calls.push(Vec::new());
        Ok(())
    }

    /// Novel inputs and new findings since the last call.
    pub fn take_reports(&mut self) -> (Vec<Entry>, Vec<FindingReport>) {
        (
            std::mem::take(&mut self.outbox),
            std::mem::take(&mut self.found),
        )
    }

    pub fn stats(&mut self) -> InstanceStats {
        let at = Instant::now();
        self.rate.push_back((at, self.stats.execs));
        while self
            .rate
            .front()
            .is_some_and(|(then, _)| at.duration_since(*then) > RATE_WINDOW)
        {
            self.rate.pop_front();
        }
        if let Some((then, execs)) = self.rate.front() {
            let span = at.duration_since(*then).as_secs_f64();
            self.stats.execs_per_sec = if span > 0.2 {
                (self.stats.execs - execs) as f64 / span
            } else {
                self.stats.execs as f64 / self.started.elapsed().as_secs_f64().max(1e-3)
            };
        }
        let (blocks, edges) = {
            let coverage = &self.engine().coverage;
            (coverage.covered(), coverage.edges.len())
        };
        self.stats.blocks = blocks;
        self.stats.edges = edges;
        self.stats.corpus = self.corpus_inputs.len();
        self.stats.clone()
    }

    /// Write the coverage map and block list; summarize on stderr.
    pub fn finish(&self) -> Result<()> {
        let engine = self.engine();
        std::fs::write(
            self.root.join("coverage.json"),
            serde_json::to_vec_pretty(&engine.coverage)?,
        )?;
        std::fs::write(
            self.root.join("blocks.json"),
            serde_json::to_vec_pretty(&engine.harness.coverage)?,
        )?;
        eprintln!(
            "campaign: {} executions, {} blocks, {} corpus inputs, {} finding signatures, {} snapshot restores, {:.1} executions/s",
            engine.coverage.executions,
            engine.coverage.covered(),
            self.corpus_inputs.len(),
            self.finding_novelty.len(),
            self.recoveries,
            engine.coverage.executions as f64 / self.started.elapsed().as_secs_f64()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::speed_weights;

    #[test]
    fn slow_parents_are_picked_less() {
        // Same energy; 1 ms, 2 ms, 4 s, and one not timed yet.
        let w = speed_weights(&[4, 4, 4, 4], &[1000, 2000, 4_000_000, 0]);
        assert!(w[0] > w[1] && w[1] > w[2], "{w:?}");
        assert_eq!(w[3], 4 * 16, "untimed counts as typical");
        assert!(w[2] >= 1);
    }
}
