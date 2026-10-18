use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vroom_fuzz::campaign::{Campaign, CampaignOptions};
use vroom_fuzz::hub::Hub;
use vroom_fuzz::sync::{Client, Hello, PROTOCOL};
use vroom_fuzz::{
    tui, BlockCoverage, BugClass, CorpusStore, CoverageBlock, CoverageMap, Engine, Harness,
    Mutator, Result, RunResult,
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
        #[command(flatten)]
        fuzz: FuzzArgs,
        /// Stop after this many inputs (default 100 unless --duration-secs
        /// or --master is given).
        #[arg(long)]
        iterations: Option<usize>,
        #[arg(long)]
        output: Option<PathBuf>,
        /// Use this VM state directory instead of the harness's (one per
        /// parallel instance).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Live dashboard instead of JSON lines.
        #[arg(long)]
        tui: bool,
        /// Share the corpus through this master (`serve`); fuzz until it
        /// says stop.
        #[arg(long)]
        master: Option<String>,
        /// Instance name on the master (default host-pid).
        #[arg(long)]
        name: Option<String>,
        /// Mutator salt (default: 0 alone, random under a master).
        #[arg(long)]
        salt: Option<u64>,
        /// Stop when the process that started this one exits (`fleet`).
        #[arg(long, hide = true)]
        exit_with_parent: bool,
    },
    /// Hold the shared corpus for workers on this or other machines.
    Serve {
        harness: PathBuf,
        /// Shared corpus, findings, coverage.json and stats.json. Inputs
        /// already there are handed out again (resume).
        #[arg(long)]
        output: PathBuf,
        /// Address to accept workers on. Other machines need a reachable
        /// interface, e.g. 0.0.0.0:7979, and should set a token.
        #[arg(long, default_value = vroom_fuzz::sync::DEFAULT_ADDRESS)]
        listen: String,
        /// Tell workers to stop after this many seconds.
        #[arg(long)]
        duration_secs: Option<u64>,
        #[arg(long)]
        tui: bool,
    },
    /// Start several workers on this machine (one VM each), with a master
    /// and dashboard here unless --master points at one elsewhere.
    Fleet {
        harness: PathBuf,
        seed: PathBuf,
        #[command(flatten)]
        fuzz: FuzzArgs,
        #[arg(long, default_value_t = 2)]
        workers: usize,
        /// Shared output (local master) or per-worker output root (remote).
        #[arg(long)]
        output: PathBuf,
        /// Join this master instead of starting one.
        #[arg(long)]
        master: Option<String>,
        /// Local master address (default: a free localhost port).
        #[arg(long)]
        listen: Option<String>,
        /// Plain progress lines instead of the dashboard.
        #[arg(long)]
        no_tui: bool,
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

/// Input selection shared by `run` and `fleet`.
#[derive(Debug, Clone, clap::Args)]
struct FuzzArgs {
    /// Set a fixed IOCTL and treat the seed file as payload bytes.
    #[arg(long, value_parser = parse_u32)]
    ioctl: Option<u32>,
    /// Skip common payload-size boundary inputs.
    #[arg(long)]
    no_boundary_seeds: bool,
    /// Replay the seed while rotating unresolved-branch breakpoints.
    #[arg(long)]
    repeat_seed: bool,
    /// Keep fuzzing until this many seconds have elapsed (overrides iterations).
    #[arg(long)]
    duration_secs: Option<u64>,
}

/// Workers and master share this secret (`VROOM_FUZZ_TOKEN`) when set.
fn token() -> Option<String> {
    std::env::var("VROOM_FUZZ_TOKEN").ok().filter(|t| !t.is_empty())
}

fn host_name() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "host".into())
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
            fuzz,
            iterations,
            output,
            state_dir,
            tui,
            master,
            name,
            salt,
            exit_with_parent,
        } => run(RunArgs {
            harness,
            seed,
            fuzz,
            iterations,
            output,
            state_dir,
            tui,
            master,
            name,
            salt,
            exit_with_parent,
        })?,
        Command::Serve {
            harness,
            output,
            listen,
            duration_secs,
            tui,
        } => {
            let h = Harness::load(&harness)?;
            let listener = std::net::TcpListener::bind(&listen)?;
            eprintln!("master for {} on {}", h.name, listener.local_addr()?);
            let hub = master_hub(&h, &output, listener)?;
            supervise(&hub, duration_secs, tui, None)?;
        }
        Command::Fleet {
            harness,
            seed,
            fuzz,
            workers,
            output,
            master,
            listen,
            no_tui,
        } => fleet(FleetArgs {
            harness,
            seed,
            fuzz,
            workers,
            output,
            master,
            listen,
            tui: !no_tui,
        })?,
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
            if h.coverage_mode == vroom_fuzz::CoverageMode::Blocks {
                return Err(vroom_fuzz::Error::Invalid(
                    "probe compares each IOCTL's blocks with a baseline: use coverage_mode edges".into(),
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
            if baseline_result.reopen {
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
                if result.reopen {
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

#[derive(Debug)]
struct RunArgs {
    harness: PathBuf,
    seed: PathBuf,
    fuzz: FuzzArgs,
    iterations: Option<usize>,
    output: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    tui: bool,
    master: Option<String>,
    name: Option<String>,
    salt: Option<u64>,
    exit_with_parent: bool,
}

/// How often a running instance reports to its hub (and master).
const REPORT_EVERY: Duration = Duration::from_millis(250);

fn lock(hub: &Mutex<Hub>) -> std::sync::MutexGuard<'_, Hub> {
    hub.lock().unwrap_or_else(|e| e.into_inner())
}

fn random_salt() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    nanos ^ (u64::from(std::process::id()) << 32)
}

/// One instance: alone (JSON lines or `--tui`) or as a worker of a master.
fn run(args: RunArgs) -> Result<()> {
    let mut h = Harness::load(&args.harness)?;
    if let Some(dir) = args.state_dir {
        h.state_dir = dir;
    }
    let root = args.output.unwrap_or_else(|| h.state_dir.join("fuzz"));
    std::fs::create_dir_all(&root)?;
    let name = args
        .name
        .unwrap_or_else(|| format!("{}-{}", host_name(), std::process::id()));
    let worker = args.master.is_some();
    // Workers must not all try the same mutations.
    let salt = args
        .salt
        .unwrap_or_else(|| if worker { random_salt() } else { 0 });
    let title = h.name.clone();
    let mut engine = Engine::open(h)?;
    engine.instrument()?;
    let seed = std::fs::read(&args.seed)?;
    let verbose = !args.tui && !worker;
    let mut campaign = Campaign::new(
        engine,
        root.clone(),
        seed,
        CampaignOptions {
            name: name.clone(),
            ioctl: args.fuzz.ioctl,
            boundary_seeds: !args.fuzz.no_boundary_seeds,
            repeat_seed: args.fuzz.repeat_seed,
            salt,
            verbose,
        },
    )?;
    let hub = Arc::new(Mutex::new(Hub::new(
        title.clone(),
        campaign.blocks().to_vec(),
        None,
        Some(root.clone()),
    )));
    let client = args.master.map(|address| {
        Client::start(
            address,
            Hello {
                protocol: PROTOCOL,
                token: token(),
                instance: name.clone(),
                harness: title,
                blocks: campaign.blocks().len(),
            },
        )
    });
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let dashboard = args
        .tui
        .then(|| tui::spawn(hub.clone(), stop.clone(), done.clone()));
    let iterations = args
        .iterations
        .or((args.fuzz.duration_secs.is_none() && !worker).then_some(100));
    let duration = args.fuzz.duration_secs.map(Duration::from_secs);
    let parent = std::os::unix::process::parent_id();
    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut last_sample = Instant::now();
    let mut last_persist = Instant::now();
    let mut last_log = Instant::now();
    let outcome = loop {
        if stop.load(Ordering::Relaxed)
            || iterations.is_some_and(|n| campaign.iterations() >= n)
            || duration.is_some_and(|d| started.elapsed() >= d)
            || client.as_ref().is_some_and(Client::stop_requested)
            || (args.exit_with_parent && std::os::unix::process::parent_id() != parent)
        {
            break Ok(());
        }
        if let Err(error) = campaign.step() {
            break Err(error);
        }
        if last_report.elapsed() >= REPORT_EVERY {
            last_report = Instant::now();
            let (entries, findings) = campaign.take_reports();
            let stats = campaign.stats();
            if let Some(client) = &client {
                client.send(stats.clone(), entries.clone(), findings.clone());
            }
            let mut hub = lock(&hub);
            if let Err(error) = hub.submit(stats.clone(), entries, findings) {
                break Err(error);
            }
            if last_sample.elapsed() >= Duration::from_secs(1) {
                last_sample = Instant::now();
                hub.sample();
            }
            if last_persist.elapsed() >= Duration::from_secs(5) {
                last_persist = Instant::now();
                hub.persist()?;
            }
            drop(hub);
            if let Some(client) = &client {
                for entry in client.received() {
                    campaign.import(entry)?;
                }
            }
            if worker && !args.tui && last_log.elapsed() >= Duration::from_secs(30) {
                last_log = Instant::now();
                eprintln!(
                    "{name}: {} inputs, {:.0}/s, {} blocks, {} corpus inputs, {} crashing inputs",
                    stats.execs, stats.execs_per_sec, stats.blocks, stats.corpus, stats.crashes
                );
            }
        }
    };
    let (entries, findings) = campaign.take_reports();
    let mut stats = campaign.stats();
    stats.done = true;
    {
        let mut hub = lock(&hub);
        hub.submit(stats.clone(), entries.clone(), findings.clone())?;
        hub.persist()?;
    }
    let refusal = client.as_ref().and_then(Client::refusal);
    if let Some(client) = client {
        client.finish(stats, entries, findings);
    }
    done.store(true, Ordering::Relaxed);
    if let Some(dashboard) = dashboard {
        let _ = dashboard.join();
    }
    outcome?;
    campaign.finish()?;
    if let Some(reason) = refusal {
        return Err(vroom_fuzz::Error::Invalid(format!("master refused this worker: {reason}")));
    }
    Ok(())
}

/// A master's hub: shared corpus in `output`, serving workers on `listener`.
fn master_hub(h: &Harness, output: &Path, listener: std::net::TcpListener) -> Result<Arc<Mutex<Hub>>> {
    let store = CorpusStore::new(output)?;
    let resumed = store.load_inputs()?;
    let mut hub = Hub::new(
        h.name.clone(),
        h.coverage.clone(),
        Some(store),
        Some(output.to_path_buf()),
    );
    hub.master = true;
    if !resumed.is_empty() {
        eprintln!("resuming with {} inputs from {}", resumed.len(), output.display());
    }
    hub.add_unverified(resumed);
    let hub = Arc::new(Mutex::new(hub));
    vroom_fuzz::sync::serve(listener, hub.clone(), token(), h.name.clone(), h.coverage.len());
    Ok(hub)
}

/// Run a master until its duration, `q`, or (for a fleet) the end of every
/// local worker; then stop the workers and save.
fn supervise(
    hub: &Arc<Mutex<Hub>>,
    duration_secs: Option<u64>,
    dashboard: bool,
    mut children: Option<&mut Vec<std::process::Child>>,
) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let dashboard = dashboard.then(|| tui::spawn(hub.clone(), stop.clone(), done.clone()));
    let started = Instant::now();
    let mut ticks = 0u64;
    let all_exited = |children: &mut Option<&mut Vec<std::process::Child>>| {
        children.as_mut().is_some_and(|children| {
            children
                .iter_mut()
                .all(|child| child.try_wait().ok().flatten().is_some())
        })
    };
    loop {
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(100));
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
        ticks += 1;
        {
            let mut hub = lock(hub);
            hub.sample();
            if ticks % 5 == 0 {
                hub.persist()?;
            }
            if dashboard.is_none() && ticks % 10 == 0 {
                eprintln!(
                    "{}: {} execs, {:.0}/s, {} corpus, {} blocks, {} edges, {} unique findings, {} workers",
                    tui::duration(started.elapsed().as_secs_f64()),
                    hub.execs(),
                    hub.execs_per_sec(),
                    hub.corpus_len(),
                    hub.covered_blocks(),
                    hub.edge_count(),
                    hub.findings().count(),
                    hub.live().count()
                );
            }
        }
        if stop.load(Ordering::Relaxed)
            || duration_secs.is_some_and(|d| started.elapsed() >= Duration::from_secs(d))
            || all_exited(&mut children)
        {
            break;
        }
    }
    lock(hub).stop = true;
    // Workers hear it on their next sync and send their last report.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let finished = lock(hub).all_done();
        let exited = children.is_none() || all_exited(&mut children);
        if finished && exited {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        lock(hub).sample();
    }
    lock(hub).persist()?;
    done.store(true, Ordering::Relaxed);
    if let Some(dashboard) = dashboard {
        let _ = dashboard.join();
    }
    let hub = lock(hub);
    eprintln!(
        "campaign: {} executions, {} blocks, {} edges, {} corpus inputs, {} unique findings, {} instances",
        hub.execs(),
        hub.covered_blocks(),
        hub.edge_count(),
        hub.corpus_len(),
        hub.findings().count(),
        hub.instances().count()
    );
    Ok(())
}

#[derive(Debug)]
struct FleetArgs {
    harness: PathBuf,
    seed: PathBuf,
    fuzz: FuzzArgs,
    workers: usize,
    output: PathBuf,
    master: Option<String>,
    listen: Option<String>,
    tui: bool,
}

/// N worker processes (HVF runs one VM per process), each with its own VM
/// state, salt and output under `output/workers/<i>`.
fn fleet(args: FleetArgs) -> Result<()> {
    if args.workers == 0 {
        return Err(vroom_fuzz::Error::Invalid("--workers must be at least 1".into()));
    }
    std::fs::create_dir_all(&args.output)?;
    let h = Harness::load(&args.harness)?;
    let (address, hub) = match &args.master {
        Some(address) => (address.clone(), None),
        None => {
            let listener =
                std::net::TcpListener::bind(args.listen.as_deref().unwrap_or("127.0.0.1:0"))?;
            let address = listener.local_addr()?.to_string();
            (address, Some(master_hub(&h, &args.output, listener)?))
        }
    };
    let program = std::env::current_exe()?;
    let host = host_name();
    let mut children = Vec::new();
    for i in 0..args.workers {
        let dir = args.output.join("workers").join(i.to_string());
        std::fs::create_dir_all(&dir)?;
        let log = std::fs::File::create(dir.join("worker.log"))?;
        let mut command = std::process::Command::new(&program);
        command
            .arg("run")
            .arg(&args.harness)
            .arg(&args.seed)
            .arg("--master")
            .arg(&address)
            .arg("--name")
            .arg(format!("{host}-{i}"))
            .arg("--state-dir")
            .arg(dir.join("state"))
            .arg("--output")
            .arg(&dir)
            .arg("--exit-with-parent")
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if let Some(ioctl) = args.fuzz.ioctl {
            command.arg("--ioctl").arg(format!("{ioctl:#x}"));
        }
        if args.fuzz.no_boundary_seeds {
            command.arg("--no-boundary-seeds");
        }
        if args.fuzz.repeat_seed {
            command.arg("--repeat-seed");
        }
        if hub.is_none() {
            if let Some(seconds) = args.fuzz.duration_secs {
                command.arg("--duration-secs").arg(seconds.to_string());
            }
        }
        children.push(command.spawn()?);
    }
    eprintln!(
        "{} workers on {address}; logs in {}",
        args.workers,
        args.output.join("workers").display()
    );
    if let Some(hub) = &hub {
        supervise(hub, args.fuzz.duration_secs, args.tui, Some(&mut children))?;
    }
    let deadline = Instant::now() + Duration::from_secs(if hub.is_some() { 30 } else { u64::MAX / 4 });
    for (i, child) in children.iter_mut().enumerate() {
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                break child.wait().ok();
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if !status.is_some_and(|status| status.success()) {
            eprintln!(
                "worker {i} ended with {status:?}; see {}",
                args.output.join("workers").join(i.to_string()).join("worker.log").display()
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

#[cfg(test)]
mod tests {
    #[test]
    fn command_line_is_consistent() {
        use clap::CommandFactory;
        super::Cli::command().debug_assert();
    }
}
