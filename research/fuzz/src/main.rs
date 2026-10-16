use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;
use vroom_fuzz::{CorpusStore, CoverageBlock, CoverageMap, Engine, Harness, Mutator, Result};

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
        #[arg(long, default_value_t = 100)]
        iterations: usize,
        #[arg(long)]
        output: Option<PathBuf>,
    },
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
        }
        Command::Mutate {
            seed,
            max_len,
            count,
        } => {
            let data = std::fs::read(seed)?;
            let mut m = Mutator::new(0x7632_6f6f_6d_u64, max_len);
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
            iterations,
            output,
        } => {
            let h = Harness::load(&harness)?;
            let root = output.unwrap_or_else(|| h.state_dir.join("fuzz"));
            let store = CorpusStore::new(&root)?;
            let mut engine = Engine::open(h)?;
            engine.instrument()?;
            let seed = std::fs::read(seed)?;
            let mut mutator = Mutator::new(0x7632_6f6f_6d_u64, engine.harness.max_input);
            let campaign_start = Instant::now();
            for i in 0..iterations {
                let input = if i == 0 {
                    seed.clone()
                } else {
                    mutator.mutate(&seed)
                };
                let result = engine.run_input(&input)?;
                if result.new_blocks != 0 {
                    store.save_input(&input, result.new_blocks)?;
                }
                if let Some(finding) = &result.finding {
                    store.save_finding(finding, &input)?;
                }
                println!("{}", serde_json::to_string(&result)?);
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
                "campaign: {} executions, {} blocks, {:.1} executions/s",
                engine.coverage.executions,
                engine.coverage.covered(),
                engine.coverage.executions as f64 / campaign_start.elapsed().as_secs_f64()
            );
        }
    }
    Ok(())
}
