//! `vroom import`: bring a VM over from VMware.

use std::io::Write;
use std::path::PathBuf;

use clap::Args;

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// The VMware VM: its .vmwarevm folder or its .vmx file.
    pub vm: PathBuf,
    /// Where to put the new VM (a folder named after it is made here).
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Name for the new VM (default: VMware's name).
    #[arg(long)]
    pub name: Option<String>,
    /// Keep VMware's BIOS UUID and SMBIOS names so Windows sees the same
    /// machine (activation). The guest can then tell it came from VMware.
    #[arg(long)]
    pub keep_hardware_ids: bool,
    /// Import even if the VM is suspended in VMware (its memory is lost).
    #[arg(long)]
    pub yes: bool,
    /// Only show what would happen.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(a: ImportArgs) -> Result<(), String> {
    let p = vroom_import::plan(&a.vm).map_err(|e| e.to_string())?;
    let name = a.name.clone().unwrap_or_else(|| p.name.clone());
    println!("{} -> {}", p.vmx.display(), a.dir.join(&name).display());
    println!("  {:?}, {} CPUs, {} MiB, TPM: {}", p.os, p.cpus, p.memory_mib, if p.tpm { "yes" } else { "no" });
    for d in &p.disks {
        let what = if d.cdrom { "attach ISO" } else { "copy" };
        println!("  {}: {what} {}", d.slot, d.source.display());
    }
    for n in &p.notes {
        println!("  note: {n}");
    }
    if a.dry_run {
        return Ok(());
    }
    if p.suspended && !a.yes {
        return Err("the VM is suspended in VMware: shut it down there first, or pass --yes to import anyway".into());
    }
    let opts = vroom_import::Options { name: a.name, keep_hardware_ids: a.keep_hardware_ids, accept_suspended: a.yes };
    let mut last = u64::MAX;
    let cfg = vroom_import::run(&p, &a.dir, &opts, |pr| {
        let pct = pr.done * 100 / pr.total.max(1);
        if pct != last {
            last = pct;
            print!("\r  disk {}/{}: {pct}% of {} GiB", pr.disk + 1, pr.disks, pr.total >> 30);
            let _ = std::io::stdout().flush();
        }
    })
    .map_err(|e| e.to_string())?;
    println!("\ndone: {}", cfg.display());
    println!("start it: vroom-gui {}", cfg.display());
    Ok(())
}
