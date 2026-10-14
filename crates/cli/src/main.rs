//! `bulwark` command line.

mod term;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use bulwark_config::VmConfig;
use bulwark_vmm::{StartMode, VmEvent, Vmm, VmmOptions};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "bulwark", version, about = "Secure, modern virtual machines")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Boot a VM (serial console on this terminal).
    Run {
        /// VM definition (TOML).
        config: PathBuf,
        /// Resume from a snapshot / suspend image instead of booting.
        #[arg(long)]
        restore: Option<PathBuf>,
        /// Where Ctrl-A s / Ctrl-A z write snapshots.
        #[arg(long, default_value = "snapshots")]
        snapshot_dir: PathBuf,
    },
    /// Check a VM definition and print it back normalized.
    Check { config: PathBuf },
}

const HELP: &str = "\r\n[bulwark] Ctrl-A x: quit | s: snapshot | z: suspend | p: pause/resume | r: reset | h: help\r\n";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Run { config, restore, snapshot_dir } => run(&config, restore, &snapshot_dir),
        Cmd::Check { config } => load(&config).and_then(|c| {
            print!("{}", c.to_toml().map_err(|e| e.to_string())?);
            Ok(())
        }),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bulwark: {e}");
            ExitCode::FAILURE
        }
    }
}

fn load(p: &Path) -> Result<VmConfig, String> {
    let s = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut c = VmConfig::from_toml(&s).map_err(|e| e.to_string())?;
    // Relative paths in the config are relative to the config file.
    let base = p.parent().unwrap_or(Path::new("."));
    c.rebase_paths(base);
    Ok(c)
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn run(config: &Path, restore: Option<PathBuf>, snap_dir: &Path) -> Result<(), String> {
    let cfg = load(config)?;
    let hv = bulwark_hv::default_hypervisor().map_err(|e| e.to_string())?;
    let start = match restore {
        Some(p) => StartMode::Restore(p),
        None => StartMode::Boot,
    };
    let mut vmm = Vmm::start(cfg, hv, VmmOptions { serial_out: Box::new(std::io::stdout()), start })
        .map_err(|e| e.to_string())?;

    let _raw = term::RawTerminal::enable();
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut buf = [0u8; 256];
            let mut stdin = std::io::stdin();
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;

    let mut escape = false;
    loop {
        while let Ok(bytes) = rx.try_recv() {
            let mut fwd = Vec::with_capacity(bytes.len());
            for b in bytes {
                if escape {
                    escape = false;
                    match b {
                        b'x' => {
                            vmm.shutdown();
                            eprint!("\r\n[bulwark] stopped\r\n");
                            return Ok(());
                        }
                        b's' => {
                            let d = snap_dir.join(format!("snap-{}", timestamp()));
                            match vmm.snapshot(&d, "manual") {
                                Ok(()) => eprint!("\r\n[bulwark] snapshot: {}\r\n", d.display()),
                                Err(e) => eprint!("\r\n[bulwark] snapshot failed: {e}\r\n"),
                            }
                        }
                        b'z' => {
                            let d = snap_dir.join(format!("suspend-{}", timestamp()));
                            return match vmm.suspend(&d) {
                                Ok(()) => {
                                    eprint!("\r\n[bulwark] suspended to {}\r\n", d.display());
                                    Ok(())
                                }
                                Err(e) => Err(format!("suspend failed: {e}")),
                            };
                        }
                        b'p' => {
                            let r = if vmm.state() == bulwark_vmm::RunState::Paused {
                                vmm.resume().map(|_| "resumed")
                            } else {
                                vmm.pause().map(|_| "paused")
                            };
                            match r {
                                Ok(s) => eprint!("\r\n[bulwark] {s}\r\n"),
                                Err(e) => eprint!("\r\n[bulwark] {e}\r\n"),
                            }
                        }
                        b'r' => {
                            if let Err(e) = vmm.reset() {
                                eprint!("\r\n[bulwark] reset failed: {e}\r\n");
                            }
                        }
                        0x01 => fwd.push(0x01),
                        _ => eprint!("{HELP}"),
                    }
                } else if b == 0x01 {
                    escape = true;
                } else {
                    fwd.push(b);
                }
            }
            if !fwd.is_empty() {
                vmm.serial_input(&fwd);
            }
        }
        match vmm.next_event(Duration::from_millis(20)) {
            None => {}
            Some(VmEvent::Shutdown) => {
                vmm.shutdown();
                eprint!("\r\n[bulwark] guest powered off\r\n");
                return Ok(());
            }
            Some(VmEvent::Reset) => {
                if let Err(e) = vmm.reset() {
                    return Err(format!("reboot failed: {e}"));
                }
            }
            Some(VmEvent::Crashed(e)) => {
                vmm.shutdown();
                return Err(format!("vm crashed: {e}"));
            }
        }
    }
}
