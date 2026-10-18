//! `vroom guest exec`: a command run in a running VM's guest by its agent.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use vroom_config::VmConfig;
use vroom_proto::{Decoder, ExecEnd, Message, Shell, MAX_PAYLOAD};

#[derive(Debug, Subcommand)]
pub enum GuestCmd {
    /// Run a command in the guest (Windows: cmd.exe as SYSTEM; Linux: sh
    /// as root). Prints its output, exits with its code (124: timed out).
    Exec(ExecArgs),
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// VM definition (TOML), or the name of one of the app's VMs.
    vm: String,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// PowerShell instead (Linux: pwsh).
    #[arg(long, short)]
    powershell: bool,
    /// Seconds before it's killed.
    #[arg(long, short, default_value_t = 60)]
    timeout: u32,
    /// Most bytes kept of each output.
    #[arg(long, default_value_t = 1 << 20)]
    max_out: u32,
    /// The command line (words joined with spaces).
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

pub fn run(c: GuestCmd, load: fn(&Path) -> Result<VmConfig, String>, state_dir: fn(&Path, &VmConfig) -> PathBuf) -> Result<(), String> {
    let GuestCmd::Exec(a) = c;
    let config = find(&a.vm);
    let cfg = load(&config)?;
    let dir = a.state_dir.unwrap_or_else(|| state_dir(&config, &cfg));
    let not_running = || format!("{} isn't running", cfg.name);
    let sock = std::fs::read_to_string(dir.join(vroom_vmm::control::CONTROL_FILE)).map_err(|_| not_running())?;
    let mut s = UnixStream::connect(sock.trim()).map_err(|_| not_running())?;
    let req = Message::Exec {
        id: 0,
        shell: if a.powershell { Shell::PowerShell } else { Shell::Default },
        timeout_ms: a.timeout.saturating_mul(1000),
        max_out: a.max_out,
        command: a.command.join(" "),
    };
    s.write_all(&req.encode()).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(a.timeout as u64 + 60))).map_err(|e| e.to_string())?;
    let mut d = Decoder::new(MAX_PAYLOAD);
    let mut buf = vec![0u8; 64 << 10];
    let reply = loop {
        if let Some(m) = d.next().map_err(|e| e.to_string())? {
            break m;
        }
        match s.read(&mut buf) {
            Ok(0) => return Err("the VM hung up".into()),
            Ok(n) => d.push(&buf[..n]),
            Err(e) => return Err(e.to_string()),
        }
    };
    let Message::ExecDone { end, truncated, stdout, stderr, .. } = reply else { return Err("a strange answer".into()) };
    if end == ExecEnd::Failed {
        return Err(String::from_utf8_lossy(&stderr).trim().to_string());
    }
    let _ = std::io::stdout().write_all(&stdout);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(&stderr);
    if truncated {
        eprintln!("vroom: output cut at {} bytes", a.max_out);
    }
    match end {
        ExecEnd::Exited(0) => Ok(()),
        // 256, -1073741510...: still a failure in 8 bits.
        ExecEnd::Exited(c) => std::process::exit(if c & 0xff == 0 { 1 } else { c & 0xff }),
        _ => {
            eprintln!("vroom: timed out after {}s: killed", a.timeout);
            std::process::exit(124)
        }
    }
}

/// A VM file, or the app's VM of that name.
fn find(vm: &str) -> PathBuf {
    let p = PathBuf::from(vm);
    if p.exists() || vm.contains('/') {
        return p;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let data = match std::env::var_os("VROOM_HOME") {
        Some(h) => PathBuf::from(h),
        None if cfg!(target_os = "macos") => home.join("Library/Application Support/vroom"),
        None => std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share")).join("vroom"),
    };
    data.join("VMs").join(vm).join(format!("{vm}.toml"))
}
