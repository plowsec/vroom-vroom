use clap::{Parser, Subcommand};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use vroom_fuzz::{
    BlockCoverage, BugClass, CorpusStore, CoverageBlock, CoverageMap, Engine, Harness, Mutator,
    Result, RunResult,
};

#[derive(Debug, Parser)]
#[command(
    name = "vroom-fuzz",
    about = "snapshot fuzzing and coverage analysis for vroom-lab"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a harness and print the declared analysis surface.
    Check { harness: PathBuf },
    /// Mutate one seed without starting a VM; useful for harness bring-up.
    Mutate {
        seed: PathBuf,
        #[arg(long, default_value_t = 32)]
        max_len: usize,
        #[arg(long, default_value_t = 1)]
        count: usize,
    },
    /// Summarize a coverage map emitted by a run.
    Coverage { map: PathBuf, blocks: PathBuf },
    /// Run a bounded campaign and write coverage/corpus records under output.
    Run {
        harness: PathBuf,
        seed: PathBuf,
        /// Set a fixed IOCTL and treat the seed file as payload bytes.
        #[arg(long, value_parser = parse_u32)]
        ioctl: Option<u32>,
        #[arg(long, default_value_t = 100)]
        iterations: usize,
        /// Skip common payload-size boundary inputs.
        #[arg(long)]
        no_boundary_seeds: bool,
        /// Replay the seed while rotating unresolved-branch breakpoints.
        #[arg(long)]
        repeat_seed: bool,
        #[arg(long)]
        output: Option<PathBuf>,
        /// Keep fuzzing until this many seconds have elapsed (overrides iterations).
        #[arg(long)]
        duration_secs: Option<u64>,
    },
    /// Test each IOCTL in the harness range from the same saved VM state.
    Probe {
        harness: PathBuf,
        /// Raw input bytes, without the four-byte IOCTL prefix.
        payload: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(serde::Serialize)]
struct ProbeRow<'a> {
    ioctl: u32,
    baseline: bool,
    differs_from_baseline: bool,
    covered_blocks: usize,
    block_hits: &'a BTreeMap<u64, u64>,
    finding: Option<BugClass>,
    profile: Option<String>,
}

#[derive(serde::Serialize)]
struct ProbeSummary {
    requested_candidates: usize,
    tested_candidates: usize,
    complete: bool,
    recoveries: usize,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Check { harness } => {
            let h = Harness::load(&harness)?;
            println!(
                "{}: {} blocks, input GPA {:#x}, {} detectors",
                h.name,
                h.coverage.len(),
                h.input_gpa,
                h.detectors.len()
            );
            if let Some(probe) = &h.ioctl_probe {
                println!(
                    "IOCTL probe: {} candidates, baseline {:#010x}",
                    probe.codes().len(),
                    probe.baseline_ioctl
                );
            }
        }
        Command::Mutate {
            seed,
            max_len,
            count,
        } => {
            let data = std::fs::read(seed)?;
            let mut m = Mutator::new(0x0076_326f_6f6d_u64, max_len);
            for _ in 0..count {
                let out = m.mutate(&data);
                println!("{}", hex::encode(out));
            }
        }
        Command::Coverage { map, blocks } => {
            let map: CoverageMap = serde_json::from_slice(&std::fs::read(map)?)?;
            let blocks: Vec<CoverageBlock> = serde_json::from_slice(&std::fs::read(blocks)?)?;
            println!("{}", serde_json::to_string_pretty(&map.summary(&blocks))?);
        }
        Command::Run {
            harness,
            seed,
            ioctl,
            iterations,
            no_boundary_seeds,
            repeat_seed,
            output,
            duration_secs,
        } => {
            let h = Harness::load(&harness)?;
            let root = output.unwrap_or_else(|| h.state_dir.join("fuzz"));
            let store = CorpusStore::new(&root)?;
            let mut engine = Engine::open(h)?;
            engine.instrument()?;
            let mut seed = std::fs::read(seed)?;
            if let Some(ioctl) = ioctl {
                if !engine.harness.ioctl_prefix {
                    return Err(vroom_fuzz::Error::Invalid(
                        "--ioctl requires ioctl_prefix: true in the harness".into(),
                    ));
                }
                let mut prefixed = ioctl.to_le_bytes().to_vec();
                prefixed.extend_from_slice(&seed);
                seed = prefixed;
            }
            if seed.len() > engine.harness.max_input {
                return Err(vroom_fuzz::Error::Invalid("seed exceeds max_input".into()));
            }
            if engine.harness.ioctl_prefix && seed.len() < 4 {
                return Err(vroom_fuzz::Error::Invalid(
                    "IOCTL-prefixed seed is shorter than four bytes".into(),
                ));
            }
            let mut corpus_inputs = store.load_inputs()?;
            let prefix_len = if engine.harness.ioctl_prefix { 4 } else { 0 };
            let fixed_prefix = ioctl.map(|_| seed[..prefix_len].to_vec());
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
            let mut scheduled_inputs = vec![seed.clone()];
            if !no_boundary_seeds {
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
                        scheduled_inputs.push(input);
                    }
                }
            }
            let mut mutator = Mutator::new(0x0076_326f_6f6d_u64, engine.harness.max_input);
            let campaign_start = Instant::now();
            let mut recoveries = 0;
            let mut inputs_since_reopen = 0;
            let mut finding_novelty = BTreeSet::<String>::new();
            let mut i = 0usize;
            while duration_secs.map_or(i < iterations, |seconds| {
                campaign_start.elapsed() < std::time::Duration::from_secs(seconds)
            }) {
                let mut selected_parent = None;
                let input = if i < scheduled_inputs.len() {
                    scheduled_inputs[i].clone()
                } else if repeat_seed {
                    seed.clone()
                } else {
                    let parent_index = mutator.choose_weighted(&corpus_energy);
                    selected_parent = Some(parent_index);
                    corpus_energy[parent_index] =
                        corpus_energy[parent_index].saturating_sub(1).max(1);
                    let parent = &corpus_inputs[parent_index];
                    mutator.mutate_preserving_prefix(parent, prefix_len)
                };
                let result = engine.run_input(&input)?;
                let new_findings = result
                    .findings
                    .iter()
                    .filter(|finding| {
                        finding_novelty.insert(format!("{:?}:{:?}", finding.class, finding.pc))
                    })
                    .count();
                let novel = result.new_blocks != 0 || result.new_edges != 0 || new_findings != 0;
                if novel {
                    if corpus_hashes.insert(result.input_hash.clone()) {
                        store.save_input(&input, result.new_blocks)?;
                        corpus_inputs.push(input.clone());
                        let score = (1
                            + result.new_blocks as u32 * 16
                            + result.new_edges as u32 * 4
                            + new_findings as u32 * 8)
                            .min(256);
                        corpus_energy.push(score);
                    } else if let Some(index) = corpus_inputs
                        .iter()
                        .position(|candidate| CorpusStore::hash(candidate) == result.input_hash)
                    {
                        let score = (1
                            + result.new_blocks as u32 * 16
                            + result.new_edges as u32 * 4
                            + new_findings as u32 * 8)
                            .min(256);
                        corpus_energy[index] = (corpus_energy[index] + score).min(256);
                    }
                    if let Some(parent) = selected_parent {
                        corpus_energy[parent] =
                            (corpus_energy[parent] + 4 + new_findings as u32 * 4).min(256);
                    }
                }
                for finding in &result.findings {
                    store.save_finding(finding, &input)?;
                }
                let unstable = result
                    .findings
                    .iter()
                    .any(|finding| matches!(finding.class, BugClass::BugCheck | BugClass::Timeout));
                println!("{}", serde_json::to_string(&result)?);
                std::io::stdout().flush()?;
                if i % 25 == 0 {
                    eprintln!(
                        "progress: {} inputs, {} blocks, {} corpus inputs, {} finding signatures",
                        i + 1,
                        engine.coverage.covered(),
                        corpus_inputs.len(),
                        finding_novelty.len()
                    );
                }
                if unstable {
                    eprintln!(
                        "restoring the clean checkpoint after {:?} ({}/{})",
                        result.finding.as_ref().map(|finding| finding.class),
                        i + 1,
                        iterations
                    );
                    engine = engine.reopen()?;
                    recoveries += 1;
                    inputs_since_reopen = 0;
                } else {
                    inputs_since_reopen += 1;
                    if engine.harness.reopen_after != 0
                        && inputs_since_reopen >= engine.harness.reopen_after
                    {
                        eprintln!(
                            "reopening the checkpoint after {} inputs",
                            inputs_since_reopen
                        );
                        engine = engine.reopen()?;
                        recoveries += 1;
                        inputs_since_reopen = 0;
                    }
                }
                i += 1;
            }
            std::fs::write(
                root.join("coverage.json"),
                serde_json::to_vec_pretty(&engine.coverage)?,
            )?;
            std::fs::write(
                root.join("blocks.json"),
                serde_json::to_vec_pretty(&engine.harness.coverage)?,
            )?;
            eprintln!(
                "campaign: {} executions, {} blocks, {} corpus inputs, {} finding signatures, {} checkpoint restores, {:.1} executions/s",
                engine.coverage.executions,
                engine.coverage.covered(),
                corpus_inputs.len(),
                finding_novelty.len(),
                recoveries,
                engine.coverage.executions as f64 / campaign_start.elapsed().as_secs_f64()
            );
        }
        Command::Probe {
            harness,
            payload,
            output,
        } => {
            let h = Harness::load(&harness)?;
            let probe = h.ioctl_probe.clone().ok_or_else(|| {
                vroom_fuzz::Error::Invalid("harness has no ioctl_probe range".into())
            })?;
            if h.coverage.is_empty() {
                return Err(vroom_fuzz::Error::Invalid(
                    "add coverage blocks before probing IOCTLs".into(),
                ));
            }
            let root = output.unwrap_or_else(|| h.state_dir.join("ioctl-probe"));
            if root.exists() {
                return Err(vroom_fuzz::Error::Invalid(format!(
                    "output path already exists: {}; choose a new --output path",
                    root.display()
                )));
            }
            std::fs::create_dir_all(&root)?;
            let payload = std::fs::read(payload)?;
            let mut engine = Engine::open(h)?;
            engine.instrument()?;
            let mut codes = probe.codes();
            if !codes.contains(&probe.baseline_ioctl) {
                codes.push(probe.baseline_ioctl);
            }
            codes.sort_unstable();
            codes.dedup();
            codes.retain(|code| *code != probe.baseline_ioctl);
            let mut report = std::fs::File::create(root.join("results.jsonl"))?;
            let mut inputs_since_reopen;

            let mut baseline_input = probe.baseline_ioctl.to_le_bytes().to_vec();
            baseline_input.extend_from_slice(&payload);
            let probe_watch_cursor = engine.watch_cursor();
            let baseline_result = engine.run_input(&baseline_input)?;
            write_probe_profile(
                &root,
                probe.baseline_ioctl,
                &baseline_result,
                &engine.harness.coverage,
            )?;
            write_probe_row(
                &mut report,
                probe.baseline_ioctl,
                true,
                false,
                &baseline_result,
                Some(format!("ioctl_{:08x}", probe.baseline_ioctl)),
            )?;
            let mut recoveries = 0;
            if baseline_result.finding.as_ref().is_some_and(|finding| {
                matches!(finding.class, BugClass::BugCheck | BugClass::Timeout)
            }) {
                engine = engine.reopen()?;
                recoveries += 1;
                inputs_since_reopen = 0;
            } else {
                inputs_since_reopen = 1;
            }
            if engine.harness.reopen_after != 0
                && inputs_since_reopen >= engine.harness.reopen_after
            {
                eprintln!(
                    "reopening the checkpoint after {} probe inputs",
                    inputs_since_reopen
                );
                engine = engine.reopen()?;
                recoveries += 1;
                inputs_since_reopen = 0;
            }
            let baseline_blocks = baseline_result.block_hits.clone();
            let baseline_edges = baseline_result.edges_hit.clone();
            let baseline_finding = baseline_result.finding.as_ref().map(|f| f.class);

            let started = Instant::now();
            let candidate_count = codes.len();
            let mut tested_candidates = 0;
            for (index, ioctl) in codes.into_iter().enumerate() {
                let mut input = ioctl.to_le_bytes().to_vec();
                input.extend_from_slice(&payload);
                engine.set_watch_cursor(probe_watch_cursor);
                let result = engine.run_input(&input)?;
                let finding_class = result.finding.as_ref().map(|f| f.class);
                let differs = result.block_hits != baseline_blocks
                    || result.edges_hit != baseline_edges
                    || finding_class != baseline_finding;
                let profile = if differs {
                    write_probe_profile(&root, ioctl, &result, &engine.harness.coverage)?;
                    Some(format!("ioctl_{ioctl:08x}"))
                } else {
                    None
                };
                write_probe_row(&mut report, ioctl, false, differs, &result, profile)?;
                tested_candidates += 1;
                let unstable = result.finding.as_ref().is_some_and(|finding| {
                    matches!(finding.class, BugClass::BugCheck | BugClass::Timeout)
                });
                if unstable {
                    eprintln!(
                        "restoring the clean checkpoint after IOCTL {ioctl:#010x}: {:?}",
                        result.finding.as_ref().map(|finding| finding.class)
                    );
                    engine = engine.reopen()?;
                    recoveries += 1;
                    inputs_since_reopen = 0;
                } else {
                    inputs_since_reopen += 1;
                    if engine.harness.reopen_after != 0
                        && inputs_since_reopen >= engine.harness.reopen_after
                    {
                        eprintln!(
                            "reopening the checkpoint after {} probe inputs",
                            inputs_since_reopen
                        );
                        engine = engine.reopen()?;
                        recoveries += 1;
                        inputs_since_reopen = 0;
                    }
                }
                if (index + 1) % 100 == 0 || index + 1 == candidate_count {
                    eprintln!("probed {}/{} IOCTLs", index + 1, candidate_count);
                }
            }
            std::fs::write(
                root.join("coverage.json"),
                serde_json::to_vec_pretty(&engine.coverage)?,
            )?;
            std::fs::write(
                root.join("blocks.json"),
                serde_json::to_vec_pretty(&engine.harness.coverage)?,
            )?;
            std::fs::write(
                root.join("summary.json"),
                serde_json::to_vec_pretty(&ProbeSummary {
                    requested_candidates: candidate_count,
                    tested_candidates,
                    complete: tested_candidates == candidate_count,
                    recoveries,
                })?,
            )?;
            eprintln!(
                "probe tested {}/{} IOCTLs with {} checkpoint restores in {:.1}s; see {}",
                tested_candidates,
                candidate_count,
                recoveries,
                started.elapsed().as_secs_f64(),
                root.join("results.jsonl").display()
            );
        }
    }
    Ok(())
}

fn parse_u32(value: &str) -> std::result::Result<u32, String> {
    let parsed = value
        .strip_prefix("0x")
        .map(|hex| u32::from_str_radix(hex, 16))
        .unwrap_or_else(|| value.parse::<u32>());
    parsed.map_err(|error| format!("invalid 32-bit number {value:?}: {error}"))
}

fn write_probe_profile(
    root: &Path,
    ioctl: u32,
    result: &RunResult,
    blocks: &[CoverageBlock],
) -> Result<()> {
    let path = root.join(format!("ioctl_{ioctl:08x}"));
    std::fs::create_dir_all(&path)?;
    let coverage = CoverageMap {
        executions: 1,
        new_edges: result.edges_hit.len() as u64,
        blocks: result
            .block_hits
            .iter()
            .map(|(id, hits)| {
                (
                    *id,
                    BlockCoverage {
                        hits: *hits,
                        first_input: Some(result.input_hash.clone()),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        edges: result.edges_hit.clone(),
    };
    std::fs::write(
        path.join("coverage.json"),
        serde_json::to_vec_pretty(&coverage)?,
    )?;
    std::fs::write(path.join("blocks.json"), serde_json::to_vec_pretty(blocks)?)?;
    Ok(())
}

fn write_probe_row(
    report: &mut std::fs::File,
    ioctl: u32,
    baseline: bool,
    differs_from_baseline: bool,
    result: &RunResult,
    profile: Option<String>,
) -> Result<()> {
    let row = ProbeRow {
        ioctl,
        baseline,
        differs_from_baseline,
        covered_blocks: result.block_hits.len(),
        block_hits: &result.block_hits,
        finding: result.finding.as_ref().map(|f| f.class),
        profile,
    };
    serde_json::to_writer(&mut *report, &row)?;
    report.write_all(b"\n")?;
    Ok(())
}
