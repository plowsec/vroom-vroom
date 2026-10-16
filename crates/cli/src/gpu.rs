//! `vroom gpu-helper`: the GPU process the VMM starts for 3D. It loads
//! virglrenderer (and with it GL), starts the render server, locks itself
//! down to the GPU and guest RAM, then serves the VMM.
//!
//! `vroom gpu-server` and `vroom gpu-worker` (macOS): the render server
//! and its workers, one per Venus or Direct3D context of the guest (see
//! vroom_gpu3d::server). A worker loads MoltenVK or DXMT and locks itself
//! down to the GPU (and DXMT's shader cache), then serves its context.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(clap::Args, Debug)]
pub struct HelperArgs {
    /// Socket for calls (an fd we inherit).
    #[arg(long)]
    sock: RawFd,
    /// Socket for fence wakeups (an fd we inherit).
    #[arg(long)]
    events: RawFd,
    /// Guest RAM regions: fd:size (inherited shared memory).
    #[arg(long, value_parser = ram)]
    ram: Vec<(RawFd, u64)>,
    #[arg(long, default_value_t = 0)]
    granule: u64,
    /// OpenGL for the guest: apple or angle.
    #[arg(long)]
    gl: Option<String>,
    #[arg(long)]
    gl_renderer: Option<String>,
    /// DXMT's shader cache: ours to write only without render workers
    /// (they write it themselves).
    #[arg(long)]
    shaders: PathBuf,
}

fn ram(s: &str) -> Result<(RawFd, u64), String> {
    let (fd, size) = s.split_once(':').ok_or("fd:size")?;
    Ok((fd.parse().map_err(|_| "fd")?, size.parse().map_err(|_| "size")?))
}

pub fn helper(a: HelperArgs) -> Result<(), String> {
    // SAFETY: fds our parent (the VMM) set up for us; nothing else here
    // uses them.
    #[allow(unsafe_code)]
    let (sock, events) = unsafe { (UnixStream::from_raw_fd(a.sock), UnixStream::from_raw_fd(a.events)) };
    #[cfg(target_os = "macos")]
    let gl = match a.gl.as_deref() {
        Some("angle") => Some(vroom_gpu3d::GlHost::Angle),
        Some(_) => Some(vroom_gpu3d::GlHost::Apple),
        None => None,
    };
    #[cfg(not(target_os = "macos"))]
    let gl = a.gl.is_some();
    if !vroom_gpu3d::load(gl) {
        return Err("no virglrenderer".into());
    }
    vroom_gpu3d::set_map_granule(a.granule);
    // Venus and Direct3D contexts in processes of their own.
    #[cfg(target_os = "macos")]
    let workers = match std::env::current_exe().and_then(|exe| vroom_gpu3d::server::start(&exe)) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("gpu process: no render server ({e}): Venus runs here, no Direct3D");
            false
        }
    };
    #[cfg(not(target_os = "macos"))]
    let workers = false;
    let r = vroom_gpu3d::Virgl::new(a.gl_renderer).ok_or("virglrenderer: Venus init failed (no Vulkan driver?)")?;
    // Up before anything from the guest arrives. DXMT, and with it its
    // shader cache, is only used here when there are no workers.
    let write_dirs = if workers { Vec::new() } else { vec![a.shaders] };
    let p = vroom_sandbox::Policy { write_dirs, gpu: true, ..Default::default() };
    vroom_sandbox::apply(&p).map_err(|e| e.to_string())?;
    log::info!("gpu process {}: sandbox applied", std::process::id());
    if std::env::var_os("VROOM_SANDBOX_SELFTEST").is_some() {
        crate::session::selftest("gpu process", false);
    }
    vroom_gpu3d::remote::serve(r, sock, events, &a.ram).map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
pub fn server() -> Result<(), String> {
    let sock = vroom_gpu3d::server::take_stdin().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    vroom_gpu3d::server::serve(UnixStream::from(sock), &exe).map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
#[derive(clap::Args, Debug)]
pub struct WorkerArgs {
    /// The guest context this worker serves.
    #[arg(long)]
    ctx: u32,
    /// Its capset: 4 (Venus) or 7 (Direct3D).
    #[arg(long)]
    capset: u32,
    /// The guest's name for it (its process name).
    #[arg(long, default_value = "")]
    name: String,
}

/// One guest context; the socket to the GPU process is our stdin.
#[cfg(target_os = "macos")]
pub fn worker(a: WorkerArgs) -> Result<(), String> {
    let sock = vroom_gpu3d::server::take_stdin().map_err(|e| e.to_string())?;
    vroom_gpu3d::load_worker(a.capset)?;
    // DXMT keeps the Metal versions of guest shaders per VM (the VMM made
    // the directory); Venus writes nothing.
    let cache = std::env::var_os("DXMT_SHADER_CACHE_PATH").map(PathBuf::from).filter(|d| d.is_dir());
    let write_dirs = if a.capset == 7 { cache.into_iter().collect() } else { Vec::new() };
    let p = vroom_sandbox::Policy { write_dirs, gpu: true, ..Default::default() };
    vroom_sandbox::apply(&p).map_err(|e| e.to_string())?;
    log::info!("gpu worker {}: context {} ({}), sandbox applied", std::process::id(), a.ctx, a.name);
    if std::env::var_os("VROOM_SANDBOX_SELFTEST").is_some() {
        crate::session::selftest("gpu worker", false);
    }
    vroom_gpu3d::run_worker(sock, a.ctx, a.capset, &a.name)
}
