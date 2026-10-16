//! `vroom` command line.

mod camera;
mod clip;
mod disk;
#[cfg(unix)]
mod gpu;
mod import;
mod log_limit;
mod new;
mod session;
mod snap;
mod term;
mod usb;
mod video;
#[cfg(target_os = "macos")]
mod vt;

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
    AudioOut {
        /// The VM may record from the microphone (`[audio] input`).
        #[arg(long)]
        input: bool,
    },
    /// Camera helper started by the VMM (internal).
    #[command(hide = true)]
    CameraHelper,
    /// GPU process started by the VMM for 3D (internal).
    #[cfg(unix)]
    #[command(hide = true)]
    GpuHelper(gpu::HelperArgs),
    /// Video decoding helper started by the VMM (internal).
    #[command(hide = true)]
    VideoHelper {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// USB passthrough helper started by the VMM (internal).
    #[command(hide = true)]
    UsbHost(usb::HostArgs),
    /// Host USB devices a VM can be given.
    #[command(subcommand)]
    Usb(usb::UsbCmd),
    /// Create a VM: write <name>.toml and its disk.
    New(new::NewArgs),
    /// Bring a VM over from VMware (copies its disks, VMware's copy stays).
    Import(import::ImportArgs),
    /// Snapshot housekeeping (list, delete, prune disk layers).
    #[command(subcommand)]
    Snapshot(snap::SnapCmd),
}

/// Apps started from the Dock or Finder get 256 open files. A Windows
/// guest with 3D keeps one or more per Direct3D event and fence (DXMT,
/// the render server, in the GPU process, which inherits this limit), and
/// runs out of them in minutes: fences then never finish and the guest
/// hangs. Take what the system allows.
fn raise_open_files() {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: valid out pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 {
        return;
    }
    // macOS refuses more than kern.maxfilesperproc (and RLIM_INFINITY).
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut cap: libc::rlim_t = 1 << 16;
    #[cfg(target_os = "macos")]
    {
        let mut n: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        // SAFETY: a valid NUL-terminated name and out buffer of `len` bytes.
        if unsafe { libc::sysctlbyname(c"kern.maxfilesperproc".as_ptr(), &mut n as *mut _ as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0) } == 0 && n > 0 {
            cap = cap.min(n as libc::rlim_t);
        }
    }
    let want = cap.min(r.rlim_max);
    if r.rlim_cur < want {
        r.rlim_cur = want;
        // SAFETY: valid rlimit; failing leaves the old limit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &r) } != 0 {
            log::warn!("can't raise the open file limit: {}", std::io::Error::last_os_error());
        }
    }
}

fn main() -> ExitCode {
    log_limit::Limited::install(env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).build());
    let r = match Cli::parse().cmd {
        Cmd::Run { config, restore, fresh, state_dir, host_clipboard_file, no_sandbox, ui_fd } => {
            raise_open_files();
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
        Cmd::AudioOut { input } => audio_out(input),
        Cmd::CameraHelper => camera::helper(),
        #[cfg(unix)]
        Cmd::GpuHelper(a) => gpu::helper(a),
        Cmd::VideoHelper { args } => video::helper(&args),
        Cmd::UsbHost(a) => usb::host(a),
        Cmd::Usb(c) => usb::run(c),
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
fn audio_out(input: bool) -> Result<(), String> {
    use std::os::fd::FromRawFd;
    // SAFETY: fd 0 is the socket our parent set up for us; nobody else
    // in this process uses stdin.
    let sock = unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(0) };
    let path = |v: &str| std::env::var_os(v).map(PathBuf::from);
    let opts = vroom_audio::HelperOpts { wav_out: path("VROOM_AUDIO_WAV"), wav_in: path("VROOM_AUDIO_WAV_IN"), input };
    vroom_audio::helper_main(sock, opts).map_err(|e| e.to_string())
}
