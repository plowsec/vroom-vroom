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
    /// Copy a file into the guest (made or replaced once all came).
    Push {
        #[command(flatten)]
        vm: Vm,
        host: PathBuf,
        /// The guest's path (Windows: C:\...).
        guest: String,
    },
    /// Copy a file out of the guest (to a folder: same name).
    Pull {
        #[command(flatten)]
        vm: Vm,
        guest: String,
        host: PathBuf,
    },
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
        GuestCmd::Push { vm, host, guest } => push(vm, &host, guest, load, state_dir),
        GuestCmd::Pull { vm, guest, host } => pull(vm, guest, &host, load, state_dir),
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

/// The VMM's answers, frame by frame.
struct Frames<'a> {
    s: &'a mut UnixStream,
    d: Decoder,
    buf: Vec<u8>,
}

impl Frames<'_> {
    fn new(s: &mut UnixStream, wait: Duration) -> Result<Frames<'_>, String> {
        s.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
        Ok(Frames { s, d: Decoder::new(MAX_PAYLOAD), buf: vec![0u8; 64 << 10] })
    }

    fn next(&mut self) -> Result<Message, String> {
        loop {
            if let Some(m) = self.d.next().map_err(|e| e.to_string())? {
                return Ok(m);
            }
            match self.s.read(&mut self.buf) {
                Ok(0) => return Err("the VM hung up".into()),
                Ok(n) => self.d.push(&self.buf[..n]),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// The VMM's answer.
fn reply(s: &mut UnixStream, wait: Duration) -> Result<Message, String> {
    Frames::new(s, wait)?.next()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A file's size and SHA-256 as the guest's agent and we both saw them.
fn agreed(m: Message, size: u64, sha256: [u8; 32]) -> Result<String, String> {
    match m {
        Message::FileDone { ok: true, size: s2, sha256: h2, .. } if (s2, h2) == (size, sha256) => Ok(format!("{size} bytes, sha256 {}", hex(&sha256))),
        Message::FileDone { ok: true, .. } => Err("the guest's file differs from ours (size or SHA-256)".into()),
        Message::FileDone { why, .. } => Err(why),
        _ => Err("a strange answer".into()),
    }
}

fn push(vm: Vm, host: &Path, guest: String, load: Load, state_dir: StateDir) -> Result<(), String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(host).map_err(|e| format!("{}: {e}", host.display()))?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    let (mut s, _) = connect(&vm, load, state_dir)?;
    s.write_all(&Message::FilePut { id: 0, size, path: guest }.encode()).map_err(|e| e.to_string())?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 256 << 10];
    let mut left = size;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want]).map_err(|e| format!("{}: {e}", host.display()))?;
        if n == 0 {
            return Err(format!("{}: changed while being sent", host.display()));
        }
        h.update(&buf[..n]);
        s.write_all(&buf[..n]).map_err(|e| format!("to the VM: {e}"))?;
        left -= n as u64;
    }
    println!("{}", agreed(reply(&mut s, Duration::from_secs(120))?, size, h.finalize().into())?);
    Ok(())
}

fn pull(vm: Vm, guest: String, host: &Path, load: Load, state_dir: StateDir) -> Result<(), String> {
    use sha2::Digest;
    let to = if host.is_dir() { host.join(guest.rsplit(['/', '\\']).next().filter(|n| !n.is_empty()).unwrap_or("file")) } else { host.to_path_buf() };
    let (mut s, _) = connect(&vm, load, state_dir)?;
    s.write_all(&Message::FileGet { id: 0, path: guest }.encode()).map_err(|e| e.to_string())?;
    let part = PathBuf::from(format!("{}.vroom-part", to.display()));
    let mut f = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let r = (|| {
        let mut frames = Frames::new(&mut s, Duration::from_secs(120))?;
        let mut h = sha2::Sha256::new();
        let mut size = 0u64;
        loop {
            match frames.next()? {
                Message::FileData { data, .. } => {
                    f.write_all(&data).map_err(|e| format!("{}: {e}", part.display()))?;
                    h.update(&data);
                    size += data.len() as u64;
                }
                m => return agreed(m, size, h.finalize().into()),
            }
        }
    })()
    .and_then(|ok| std::fs::rename(&part, &to).map(|_| ok).map_err(|e| format!("{}: {e}", to.display())));
    match r {
        Ok(ok) => {
            println!("{}: {ok}", to.display());
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
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
