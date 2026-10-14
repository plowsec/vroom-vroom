//! `bulwark` command line.

mod term;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use bulwark_config::VmConfig;
use bulwark_vmm::agent::AgentCmd;
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
        /// Use a file instead of the system clipboard (headless hosts, tests).
        #[arg(long)]
        host_clipboard_file: Option<PathBuf>,
    },
    /// Check a VM definition and print it back normalized.
    Check { config: PathBuf },
    /// Disk image tools.
    #[command(subcommand)]
    Disk(DiskCmd),
}

#[derive(Subcommand, Debug)]
enum DiskCmd {
    /// Create a qcow2 image, optionally as an overlay on a base image
    /// (the base is never written to).
    Create {
        path: PathBuf,
        /// Size, e.g. 20G, 512M. Defaults to the backing image size.
        #[arg(long)]
        size: Option<String>,
        #[arg(long)]
        backing: Option<PathBuf>,
    },
    /// Show image info.
    Info { path: PathBuf },
}

const HELP: &str = "\r\n[bulwark] Ctrl-A x: quit | s: snapshot | z: suspend | p: pause/resume | r: reset\r\n          c: host clipboard -> guest | v: guest clipboard -> host | h: help\r\n";

/// Host clipboard through arboard.
struct HostClipboard(arboard::Clipboard);

impl bulwark_vmm::agent::ClipboardProvider for HostClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }
    fn set_text(&mut self, text: &str) {
        let _ = self.0.set_text(text.to_string());
    }
}

struct FileClipboard(PathBuf);

impl bulwark_vmm::agent::ClipboardProvider for FileClipboard {
    fn get_text(&mut self) -> Option<String> {
        std::fs::read_to_string(&self.0).ok()
    }
    fn set_text(&mut self, text: &str) {
        let _ = std::fs::write(&self.0, text);
    }
}

fn host_clipboard() -> Option<Box<dyn bulwark_vmm::agent::ClipboardProvider>> {
    match arboard::Clipboard::new() {
        Ok(c) => Some(Box::new(HostClipboard(c))),
        Err(e) => {
            log::info!("no host clipboard: {e}");
            None
        }
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Run { config, restore, snapshot_dir, host_clipboard_file } => {
            let clip = match host_clipboard_file {
                Some(p) => Some(Box::new(FileClipboard(p)) as Box<dyn bulwark_vmm::agent::ClipboardProvider>),
                None => host_clipboard(),
            };
            run(&config, restore, &snapshot_dir, clip)
        }
        Cmd::Disk(d) => disk(d),
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

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mul))
        .ok_or_else(|| format!("bad size {s:?}"))
}

fn disk(cmd: DiskCmd) -> Result<(), String> {
    use bulwark_block::{qcow2::Qcow2Image, Format};
    match cmd {
        DiskCmd::Create { path, size, backing } => {
            let b = match &backing {
                Some(p) => {
                    let fmt = bulwark_block::detect(p).map_err(|e| e.to_string())?;
                    let img = bulwark_block::open(p, Some(fmt), true).map_err(|e| e.to_string())?;
                    Some((p.as_path(), fmt, img.size()))
                }
                None => None,
            };
            let size = match (size, &b) {
                (Some(s), _) => parse_size(&s)?,
                (None, Some((_, _, sz))) => *sz,
                (None, None) => return Err("--size needed without --backing".into()),
            };
            Qcow2Image::create(&path, size, b.map(|(p, f, _)| (p, f))).map_err(|e| e.to_string())?;
            println!("created {} ({} bytes)", path.display(), size);
            Ok(())
        }
        DiskCmd::Info { path } => {
            let fmt = bulwark_block::detect(&path).map_err(|e| e.to_string())?;
            let img = bulwark_block::open(&path, Some(fmt), true).map_err(|e| e.to_string())?;
            println!("format: {}", if fmt == Format::Qcow2 { "qcow2" } else { "raw" });
            println!("virtual size: {} bytes", img.size());
            println!("{img:?}");
            Ok(())
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

fn run(
    config: &Path,
    restore: Option<PathBuf>,
    snap_dir: &Path,
    clipboard: Option<Box<dyn bulwark_vmm::agent::ClipboardProvider>>,
) -> Result<(), String> {
    let cfg = load(config)?;
    let hv = bulwark_hv::default_hypervisor().map_err(|e| e.to_string())?;
    let start = match restore {
        Some(p) => StartMode::Restore(p),
        None => StartMode::Boot,
    };
    let opts = VmmOptions { serial_out: Box::new(std::io::stdout()), start, clipboard };
    let mut vmm = Vmm::start(cfg, hv, opts)
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
                        b'c' => {
                            vmm.agent().send(AgentCmd::PushClipboard);
                            eprint!("\r\n[bulwark] host clipboard sent to guest\r\n");
                        }
                        b'v' => {
                            vmm.agent().send(AgentCmd::PullClipboard);
                            eprint!("\r\n[bulwark] guest clipboard copied to host\r\n");
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
