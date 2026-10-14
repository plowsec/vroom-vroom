//! `bulwark snapshot ...`: housekeeping while the VM is off.

use std::path::{Path, PathBuf};

use bulwark_config::VmConfig;
use bulwark_vmm::state::{self, PruneReport, StateLock};
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SnapCmd {
    /// List a VM's snapshots, oldest first.
    List {
        config: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Delete a snapshot and the disk layers only it used. Layers that
    /// become pass-through are merged into the next one.
    Delete {
        config: PathBuf,
        /// Snapshot name (see `list`).
        name: String,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Merge and remove disk layers nothing needs (e.g. after reverts).
    Prune {
        config: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / (1 << 20) as f64)
}

fn report(r: &PruneReport) {
    for p in &r.folded {
        println!("merged  {}", p.display());
    }
    for p in &r.deleted {
        println!("removed {}", p.display());
    }
    if r.folded.is_empty() && r.deleted.is_empty() {
        println!("disk layers: nothing to clean up");
    }
}

/// Directories the configured disks live in: layer chains end there.
fn base_dirs(cfg: &VmConfig) -> Vec<PathBuf> {
    cfg.disks.iter().filter_map(|d| d.path.parent().map(Path::to_path_buf)).collect()
}

pub fn run(cmd: SnapCmd, load: impl Fn(&Path) -> Result<VmConfig, String>, state_of: impl Fn(&Path, &VmConfig) -> PathBuf) -> Result<(), String> {
    let (config, state_dir) = match &cmd {
        SnapCmd::List { config, state_dir } | SnapCmd::Delete { config, state_dir, .. } | SnapCmd::Prune { config, state_dir } => {
            (config, state_dir)
        }
    };
    let cfg = load(config)?;
    let state = state_dir.clone().unwrap_or_else(|| state_of(config, &cfg));
    let state = std::path::absolute(&state).map_err(|e| e.to_string())?;
    match cmd {
        SnapCmd::List { .. } => {
            let snaps = state::list(&state).map_err(|e| e.to_string())?;
            if snaps.is_empty() {
                println!("no snapshots in {}", state.display());
            }
            for s in snaps {
                println!("{:<24} {:>12}  {}  {}", s.name, mib(s.bytes), s.created_unix, s.description);
            }
            Ok(())
        }
        SnapCmd::Delete { name, .. } => {
            let _lock = StateLock::acquire(&state).map_err(|e| format!("{e} (stop the VM first)"))?;
            let r = state::delete(&state, &name, &base_dirs(&cfg)).map_err(|e| e.to_string())?;
            println!("deleted {name}");
            report(&r);
            Ok(())
        }
        SnapCmd::Prune { .. } => {
            let _lock = StateLock::acquire(&state).map_err(|e| format!("{e} (stop the VM first)"))?;
            report(&state::prune(&state, &base_dirs(&cfg)).map_err(|e| e.to_string())?);
            Ok(())
        }
    }
}
