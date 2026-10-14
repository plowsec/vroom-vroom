//! `bulwark` command line.

mod clip;
mod disk;
mod session;
mod term;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bulwark_config::VmConfig;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "bulwark", version, about = "Secure, modern virtual machines")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run a VM. Serial console on this terminal; use `bulwark-gui` for a window.
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
        /// Display channel fd from bulwark-gui (internal).
        #[arg(long, hide = true)]
        ui_fd: Option<i32>,
    },
    /// Check a VM definition and print it back normalized.
    Check { config: PathBuf },
    /// Disk image tools.
    #[command(subcommand)]
    Disk(disk::DiskCmd),
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let r = match Cli::parse().cmd {
        Cmd::Run { config, restore, fresh, state_dir, host_clipboard_file, no_sandbox, ui_fd } => {
            load(&config).and_then(|cfg| {
                let state_dir = state_dir.unwrap_or_else(|| default_state_dir(&config, &cfg));
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

fn default_state_dir(config: &Path, cfg: &VmConfig) -> PathBuf {
    let dir = config.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir.join(format!("{}.state", cfg.name))
}

fn load(p: &Path) -> Result<VmConfig, String> {
    let s = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut c = VmConfig::from_toml(&s).map_err(|e| e.to_string())?;
    // Relative paths in the config are relative to the config file.
    // Absolute, so nothing derived from them (qcow2 backing names in
    // snapshot overlays) depends on the current directory.
    let base = p.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let base = std::fs::canonicalize(base).map_err(|e| format!("{}: {e}", base.display()))?;
    c.rebase_paths(&base);
    Ok(c)
}
