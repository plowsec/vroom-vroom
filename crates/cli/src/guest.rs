//! `vroom guest ...`: a running VM's guest, through its agent (the VMM's
//! control socket, see vroom_vmm::control).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use vroom_config::{GuestOs, VmConfig};
use vroom_proto::{Decoder, ExecEnd, Message, Shell, MAX_PAYLOAD};

#[derive(Debug, Subcommand)]
pub enum GuestCmd {
    /// Run a command in the guest (Windows: cmd.exe as SYSTEM; Linux: sh
    /// as root). Prints its output, exits with its code (124: timed out).
    Exec(ExecArgs),
    /// Install this vroom's guest tools in the guest (no window needed).
    UpdateTools(Vm),
}

#[derive(Debug, Args)]
pub struct Vm {
    /// VM definition (TOML), or the name of one of the app's VMs.
    vm: String,
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[command(flatten)]
    vm: Vm,
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

type Load = fn(&Path) -> Result<VmConfig, String>;
type StateDir = fn(&Path, &VmConfig) -> PathBuf;

pub fn run(c: GuestCmd, load: Load, state_dir: StateDir) -> Result<(), String> {
    match c {
        GuestCmd::Exec(a) => exec(a, load, state_dir),
        GuestCmd::UpdateTools(vm) => update_tools(vm, load, state_dir),
    }
}

/// The VM's control socket, and its definition.
fn connect(vm: &Vm, load: Load, state_dir: StateDir) -> Result<(UnixStream, VmConfig), String> {
    let config = find(&vm.vm);
    let cfg = load(&config)?;
    let dir = vm.state_dir.clone().unwrap_or_else(|| state_dir(&config, &cfg));
    let not_running = || format!("{} isn't running", cfg.name);
    let sock = std::fs::read_to_string(dir.join(vroom_vmm::control::CONTROL_FILE)).map_err(|_| not_running())?;
    let s = UnixStream::connect(sock.trim()).map_err(|_| not_running())?;
    Ok((s, cfg))
}

/// The VMM's answer.
fn reply(s: &mut UnixStream, wait: Duration) -> Result<Message, String> {
    s.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
    let mut d = Decoder::new(MAX_PAYLOAD);
    let mut buf = vec![0u8; 64 << 10];
    loop {
        if let Some(m) = d.next().map_err(|e| e.to_string())? {
            return Ok(m);
        }
        match s.read(&mut buf) {
            Ok(0) => return Err("the VM hung up".into()),
            Ok(n) => d.push(&buf[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn exec(a: ExecArgs, load: Load, state_dir: StateDir) -> Result<(), String> {
    let (mut s, _) = connect(&a.vm, load, state_dir)?;
    let req = Message::Exec {
        id: 0,
        shell: if a.powershell { Shell::PowerShell } else { Shell::Default },
        timeout_ms: a.timeout.saturating_mul(1000),
        max_out: a.max_out,
        command: a.command.join(" "),
    };
    s.write_all(&req.encode()).map_err(|e| e.to_string())?;
    let Message::ExecDone { end, truncated, stdout, stderr, .. } = reply(&mut s, Duration::from_secs(a.timeout as u64 + 60))? else {
        return Err("a strange answer".into());
    };
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

fn update_tools(vm: Vm, load: Load, state_dir: StateDir) -> Result<(), String> {
    let (mut s, cfg) = connect(&vm, load, state_dir)?;
    let os = if cfg.guest_os() == GuestOs::Windows { vroom_tools::Os::Windows } else { vroom_tools::Os::Linux };
    let dir = vroom_tools::os_dir(os).ok_or("no guest tools here: build them with scripts/make-guest-tools.sh")?;
    let v = vroom_tools::version(os).ok_or("no version in the agent")?;
    vroom_tools::bundle::write(&mut s, 0, &v.to_string(), &dir).map_err(|e| e.to_string())?;
    s.shutdown(std::net::Shutdown::Write).map_err(|e| e.to_string())?;
    match reply(&mut s, Duration::from_secs(660))? {
        Message::UpdateDone { ok: true, .. } => {
            println!("{}: installing {v}; the agent restarts", cfg.name);
            Ok(())
        }
        Message::UpdateDone { why, .. } => Err(why),
        _ => Err("a strange answer".into()),
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
