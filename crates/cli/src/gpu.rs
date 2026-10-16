//! `vroom gpu-helper`: the GPU process the VMM starts for 3D. It loads
//! virglrenderer (and with it GL, MoltenVK, DXMT), locks itself down to
//! the GPU, guest RAM and its shader cache, then serves the VMM.

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
    /// DXMT's shader cache (the one place we may write).
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
    let r = vroom_gpu3d::Virgl::new(a.gl_renderer).ok_or("virglrenderer: Venus init failed (no Vulkan driver?)")?;
    // Up before anything from the guest arrives.
    let p = vroom_sandbox::Policy { write_dirs: vec![a.shaders], gpu: true, ..Default::default() };
    vroom_sandbox::apply(&p).map_err(|e| e.to_string())?;
    log::info!("gpu process {}: sandbox applied", std::process::id());
    if std::env::var_os("VROOM_SANDBOX_SELFTEST").is_some() {
        crate::session::selftest("gpu process", false);
    }
    vroom_gpu3d::remote::serve(r, sock, events, &a.ram).map_err(|e| e.to_string())
}
