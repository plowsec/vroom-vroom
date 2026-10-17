//! Windows: installing guest tools the host sent (update.rs staged and
//! checked them). The service starts the new `vroom-agent.exe
//! --apply-update DIR` from the staging folder as its own process, which:
//! 1. stops the VroomAgent service (this ends the old agent);
//! 2. adds the drivers whose version changed (`pnputil`): a driver
//!    package already in the driver store at the same version is left
//!    alone, the display driver only goes into the store (the agent
//!    picks the GPUs' drivers itself, sys::gpu_drivers);
//! 3. swaps the files in Program Files\Vroom: a file in use (the agent
//!    helpers, OpenGL and Vulkan apps holding our drivers) is renamed out of the
//!    way first, which Windows allows, and deleted at the next update;
//! 4. starts the service again. The session helpers see their file
//!    change when the service goes, and start the new one.
//!
//! It runs as SYSTEM, from a folder only SYSTEM and administrators can
//! write (Program Files), on files whose hashes were checked.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, StartServiceW, SC_MANAGER_CONNECT,
    SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED,
};

const SERVICE: &str = "VroomAgent";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Start the staged agent to install the folder it is in.
pub fn launch(dir: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // On its own: not ended with the service, no console.
    const DETACHED_PROCESS: u32 = 0x8;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    let exe = dir.join("vroom-agent.exe");
    if !exe.is_file() {
        return Err("the update has no vroom-agent.exe".into());
    }
    std::process::Command::new(&exe)
        .arg("--apply-update")
        .arg(dir)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", exe.display()))
}

/// `--apply-update DIR`: see the module comment.
pub fn apply(dir: &Path) -> Result<(), String> {
    let dest = crate::update::root();
    // Let the old service tell the host it went well.
    std::thread::sleep(Duration::from_secs(2));
    log::info!("update: installing {} into {}", dir.display(), dest.display());
    service(false)?;
    drivers(dir);
    let r = swap_files(dir, &dest);
    // Whatever happened, the agent runs again (old or new).
    let s = service(true);
    r.and(s)
}

/// Stop the service and wait for it, or start it.
fn service(start: bool) -> Result<(), String> {
    let name = wide(SERVICE);
    // SAFETY: plain call; the handle is closed below.
    let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if scm.is_null() {
        return Err(format!("service manager: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: valid manager handle and name.
    let svc = unsafe { OpenServiceW(scm, name.as_ptr(), SERVICE_STOP | SERVICE_START | SERVICE_QUERY_STATUS) };
    let r = if svc.is_null() {
        Err(format!("{SERVICE}: {}", std::io::Error::last_os_error()))
    } else {
        // SAFETY: plain data.
        let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        let r = if start {
            // SAFETY: valid service handle, no arguments.
            if unsafe { StartServiceW(svc, 0, std::ptr::null()) } == 0 {
                Err(format!("starting {SERVICE}: {}", std::io::Error::last_os_error()))
            } else {
                Ok(())
            }
        } else {
            // SAFETY: valid handle and out pointer. Fails if already stopped.
            unsafe { ControlService(svc, SERVICE_CONTROL_STOP, &mut st) };
            let t0 = Instant::now();
            loop {
                // SAFETY: valid handle and out pointer.
                let ok = unsafe { QueryServiceStatus(svc, &mut st) } != 0;
                if !ok || st.dwCurrentState == SERVICE_STOPPED {
                    break Ok(());
                }
                if t0.elapsed() > Duration::from_secs(30) {
                    break Err(format!("{SERVICE} didn't stop"));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        // SAFETY: handle from OpenServiceW.
        unsafe { CloseServiceHandle(svc) };
        r
    };
    // SAFETY: handle from OpenSCManagerW.
    unsafe { CloseServiceHandle(scm) };
    r
}

/// Every INF in the bundle (driver folders, as on the tools disk).
fn infs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => todo.push(p),
                Ok(t) if t.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("inf")) => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// The versions of `name` (an INF file name) in the driver store.
fn store_versions(name: &str) -> Vec<String> {
    let windir = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let repo = windir.join(r"System32\DriverStore\FileRepository");
    let prefix = format!("{}_", name.to_ascii_lowercase());
    let Ok(rd) = std::fs::read_dir(&repo) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with(&prefix))
        .filter_map(|e| std::fs::read(e.path().join(name)).ok())
        .filter_map(|b| crate::update::driver_ver(&b))
        .collect()
}

fn drivers(dir: &Path) {
    for inf in infs(dir) {
        let Some(name) = inf.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        let Some(ver) = std::fs::read(&inf).ok().and_then(|b| crate::update::driver_ver(&b)) else { continue };
        if store_versions(&name).iter().any(|v| v.eq_ignore_ascii_case(&ver)) {
            log::info!("update: {name} {ver} is installed already");
            continue;
        }
        // The display driver: into the store only, the agent picks which
        // GPU gets which driver.
        let display = name.to_ascii_lowercase().starts_with("viogpu");
        let mut c = std::process::Command::new("pnputil");
        c.arg("/add-driver").arg(&inf);
        if !display {
            c.arg("/install");
        }
        match c.output() {
            Ok(o) => log::info!("update: pnputil {name} {ver}: {}", o.status),
            Err(e) => log::warn!("update: pnputil {name}: {e}"),
        }
    }
}

/// The files that go to Program Files\Vroom: the agent, the OpenGL and
/// Vulkan drivers, virtiofs.exe. Drivers stay where pnputil put them.
fn swap_files(dir: &Path, dest: &Path) -> Result<(), String> {
    let mut wanted: Vec<(PathBuf, String)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return Err(format!("{}: can't read", dir.display())) };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let l = n.to_ascii_lowercase();
        let gl = l.starts_with("vroom_gl") && l.ends_with(".dll");
        let vk = l.starts_with("vroom_vk") && (l.ends_with(".dll") || l.ends_with(".json"));
        if l == "vroom-agent.exe" || gl || vk {
            wanted.push((e.path(), n));
        }
    }
    // Shared folders' program, if this guest has it installed.
    let viofs = dir.join(r"viofs\w11\ARM64\virtiofs.exe");
    if viofs.is_file() && dest.join("virtiofs.exe").is_file() {
        wanted.push((viofs, "virtiofs.exe".into()));
    }
    // Leftovers of the last update, now unused.
    if let Ok(rd) = std::fs::read_dir(dest) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().contains(".old-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (from, name) in &wanted {
        let to = dest.join(name);
        if std::fs::read(from).ok() == std::fs::read(&to).ok() {
            continue;
        }
        if to.exists() {
            let aside = dest.join(format!("{name}.old-{stamp}"));
            if let Err(e) = std::fs::rename(&to, &aside) {
                undo(&moved);
                return Err(format!("{}: {e}", to.display()));
            }
            moved.push((aside, to.clone()));
        }
        if let Err(e) = std::fs::copy(from, &to) {
            undo(&moved);
            return Err(format!("{}: {e}", to.display()));
        }
        log::info!("update: {} replaced", to.display());
    }
    Ok(())
}

/// Put the old files back.
fn undo(moved: &[(PathBuf, PathBuf)]) {
    for (aside, to) in moved.iter().rev() {
        let _ = std::fs::remove_file(to);
        let _ = std::fs::rename(aside, to);
    }
}
