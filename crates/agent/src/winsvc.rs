//! Windows service mode (`--service`, runs as LocalSystem) and the pipe
//! the unprivileged session helper (`--session`) talks to.
//!
//! Pipe rules:
//! - ACL: SYSTEM and Administrators full, interactive users read/write;
//!   remote clients rejected.
//! - Server side: only a client in the active console session is served.
//! - Client side: only a server process in session 0 (a service) is
//!   trusted, so a user process squatting the pipe name gets nothing.

use std::io::Read;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_CALL_NOT_IMPLEMENTED, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientSessionId, GetNamedPipeServerSessionId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::port::{self, Reader};
use crate::relay::{Ev, RealSystem, Relay};

pub const PIPE: &str = r"\\.\pipe\vroom-agent";
const SERVICE_NAME: &str = "VroomAgent";
/// SYSTEM, Administrators: all. Interactive users: read/write. Protected
/// DACL (no inheritance).
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

static STATUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
/// `--port`, for service_main. None: find it on every try.
static PORT: OnceLock<Option<String>> = OnceLock::new();

fn set_state(state: SERVICE_STATUS_CURRENT_STATE) {
    let h = STATUS.load(Ordering::Acquire);
    if h.is_null() {
        return;
    }
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWin32ExitCode: NO_ERROR,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    // SAFETY: handle from RegisterServiceCtrlHandlerExW; valid struct.
    unsafe { SetServiceStatus(h, &st) };
}

unsafe extern "system" fn control(ctrl: u32, _ty: u32, _data: *mut core::ffi::c_void, _ctx: *mut core::ffi::c_void) -> u32 {
    match ctrl {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_state(SERVICE_STOPPED);
            std::process::exit(0);
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name = wide(SERVICE_NAME);
    // SAFETY: valid name; handler is a plain extern fn.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), std::ptr::null()) };
    STATUS.store(h, Ordering::Release);
    set_state(SERVICE_RUNNING);
    serve(PORT.get().and_then(|p| p.as_deref()));
    set_state(SERVICE_STOPPED);
}

/// Hand the process to the service control manager.
pub fn run_service(port: Option<String>) -> std::io::Result<()> {
    let _ = PORT.set(port);
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    // SAFETY: NUL-terminated table; blocks until the service stops.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The service's work: keep the host port open, accept helpers, relay.
fn serve(port: Option<&str>) {
    // Shared folders added since the last boot get their drive letter.
    std::thread::spawn(|| {
        if let Err(e) = crate::shares::mount_all() {
            log::info!("shares: {e}");
        }
    });
    let (tx, rx) = channel::<Ev>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || accept_helpers(tx));
    }
    let mut relay = Relay::new(RealSystem, crate::MAX_CLIP);
    loop {
        match crate::open_port(port) {
            Ok((path, r, mut w)) => {
                log::info!("service: connected to {path}");
                spawn_reader(r, tx.clone(), Ev::Host, || Ev::HostGone);
                if let Err(e) = relay.run(&mut w, &rx) {
                    log::warn!("service: {e}");
                }
            }
            Err(e) => log::debug!("service: {e}"),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn spawn_reader(mut r: Reader, tx: Sender<Ev>, data: impl Fn(Vec<u8>) -> Ev + Send + 'static, gone: impl Fn() -> Ev + Send + 'static) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match r.read(&mut buf) {
                Ok(n) if n > 0 => {
                    if tx.send(data(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = tx.send(gone());
                    return;
                }
            }
        }
    });
}

struct SecDesc(PSECURITY_DESCRIPTOR);
impl Drop for SecDesc {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptor...W.
        unsafe { LocalFree(self.0) };
    }
}

fn pipe_security() -> std::io::Result<SecDesc> {
    let sddl = wide(PIPE_SDDL);
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid SDDL string and out pointer.
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(SecDesc(sd))
}

fn create_pipe(first: bool) -> std::io::Result<HANDLE> {
    let sd = pipe_security()?;
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: 0 };
    let name = wide(PIPE);
    let flags = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    // SAFETY: valid name and attributes; sd outlives the call.
    let h = unsafe {
        CreateNamedPipeW(name.as_ptr(), flags, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, 4, 64 << 10, 64 << 10, 0, &sa)
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(h)
}

/// Wait for a client on an overlapped pipe handle.
fn connect(h: HANDLE) -> std::io::Result<()> {
    port::overlapped(h, |h, ov: *mut OVERLAPPED| {
        // SAFETY: valid pipe handle and OVERLAPPED (kept alive by caller).
        let ok = unsafe { ConnectNamedPipe(h, ov) };
        // SAFETY: plain call.
        if ok == 0 && unsafe { GetLastError() } == ERROR_PIPE_CONNECTED {
            return 1;
        }
        ok
    })
    .map(|_| ())
}

fn accept_helpers(tx: Sender<Ev>) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut first = true;
    loop {
        let h = match create_pipe(first) {
            Ok(h) => h,
            Err(e) => {
                // First instance taken: someone squats our name.
                log::warn!("service: pipe: {e}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        first = false;
        let (r, w) = match port::from_handle(h) {
            Ok(rw) => rw,
            Err(e) => {
                log::warn!("service: pipe: {e}");
                continue;
            }
        };
        if let Err(e) = connect(h) {
            log::warn!("service: pipe connect: {e}");
            continue;
        }
        let mut sid = 0u32;
        // SAFETY: connected pipe; valid out pointer.
        let ok = unsafe { GetNamedPipeClientSessionId(h, &mut sid) } != 0;
        // SAFETY: plain call.
        let console = unsafe { WTSGetActiveConsoleSessionId() };
        if !ok || sid != console {
            log::warn!("service: refusing pipe client from session {sid} (console is {console})");
            continue; // dropping r/w closes the instance
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        if tx.send(Ev::HelperUp(id, w)).is_err() {
            return;
        }
        spawn_reader(r, tx.clone(), move |b| Ev::Helper(id, b), move || Ev::HelperGone(id));
    }
}

/// Session helper side: is the pipe served by a service (session 0)?
/// Asks the pipe, not the process: an unprivileged helper may not query
/// a SYSTEM process.
pub fn check_server(h: HANDLE) -> std::io::Result<()> {
    let mut sess = u32::MAX;
    // SAFETY: valid pipe handle and out pointer.
    if unsafe { GetNamedPipeServerSessionId(h, &mut sess) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if sess != 0 {
        return Err(std::io::Error::other(format!("pipe server runs in session {sess}, not as a service")));
    }
    Ok(())
}
