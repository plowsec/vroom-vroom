//! `vroom` command line.

mod clip;
mod disk;
mod import;
mod log_limit;
mod new;
mod session;
mod snap;
mod term;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vroom_config::VmConfig;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "vroom", version, about = "Secure, modern virtual machines")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run a VM. Serial console on this terminal; use `vroom-gui` for a window.
    Run {
        /// VM definition (TOML).
        config: PathBuf,
        /// Resume from this snapshot instead of booting.
        #[arg(long)]
        restore: Option<PathBuf>,
        /// Ignore a pending suspend image and cold boot.
        #[arg(long)]
        fresh: bool,
        /// State directory (snapshots, suspend). Default: <config>.state next to the config.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Use a file instead of the system clipboard (headless hosts, tests).
        #[arg(long)]
        host_clipboard_file: Option<PathBuf>,
        /// Don't sandbox the VMM process (debugging only).
        #[arg(long)]
        no_sandbox: bool,
        /// Display channel fd from vroom-gui (internal).
        #[arg(long, hide = true)]
        ui_fd: Option<i32>,
    },
    /// Check a VM definition and print it back normalized.
    Check { config: PathBuf },
    /// Disk image tools.
    #[command(subcommand)]
    Disk(disk::DiskCmd),
    /// Audio helper started by the VMM (internal).
    #[command(hide = true)]
    AudioOut,
    /// Create a VM: write <name>.toml and its disk.
    New(new::NewArgs),
    /// Bring a VM over from VMware (copies its disks, VMware's copy stays).
    Import(import::ImportArgs),
    /// Snapshot housekeeping (list, delete, prune disk layers).
    #[command(subcommand)]
    Snapshot(snap::SnapCmd),
}

fn main() -> ExitCode {
    log_limit::Limited::install(env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).build());
    let r = match Cli::parse().cmd {
        Cmd::Run { config, restore, fresh, state_dir, host_clipboard_file, no_sandbox, ui_fd } => {
            load(&config).and_then(|cfg| {
                for w in cfg.warnings() {
                    eprintln!("vroom: warning: {w}");
                }
                let state_dir = state_dir.unwrap_or_else(|| default_state_dir(&config, &cfg));
                // Absolute before the sandbox: no cwd lookups after that.
                let state_dir = std::path::absolute(&state_dir).unwrap_or(state_dir);
                session::run(session::RunOpts {
                    clipboard: if ui_fd.is_some() { None } else { clip::provider(host_clipboard_file) },
                    config: cfg,
                    restore,
                    state_dir,
                    sandbox: !no_sandbox,
                    ui_fd,
                    fresh,
                })
            })
        }
        Cmd::Disk(d) => disk::run(d),
        Cmd::Snapshot(s) => snap::run(s, load, default_state_dir),
        Cmd::New(a) => new::run(a, load),
        Cmd::Import(a) => import::run(a),
        Cmd::AudioOut => audio_out(),
        Cmd::Check { config } => load(&config).and_then(|c| {
            print!("{}", c.to_toml().map_err(|e| e.to_string())?);
            for w in c.warnings() {
                eprintln!("warning: {w}");
            }
            Ok(())
        }),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vroom: {e}");
            ExitCode::FAILURE
        }
    }
}

fn default_state_dir(config: &Path, cfg: &VmConfig) -> PathBuf {
    cfg.default_state_dir(config)
}

fn load(p: &Path) -> Result<VmConfig, String> {
    VmConfig::load(p).map_err(|e| e.to_string())
}

/// The VMM hands us a datagram socket as stdin.
fn audio_out() -> Result<(), String> {
    use std::os::fd::FromRawFd;
    // SAFETY: fd 0 is the socket our parent set up for us; nobody else
    // in this process uses stdin.
    let sock = unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(0) };
    let wav = std::env::var_os("VROOM_AUDIO_WAV").map(std::path::PathBuf::from);
    vroom_audio::helper_main(sock, wav).map_err(|e| e.to_string())
}
