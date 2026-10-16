//! A process per guest 3D context: virglrenderer's render server, ours.
//!
//! Venus (Vulkan) and Direct3D contexts go through virglrenderer's proxy
//! in the GPU process. The proxy asks a render server for each new
//! context and gets back a socket to the worker serving it. This module
//! is that server (`vroom gpu-server`): it starts one `vroom gpu-worker`
//! per context. A worker loads virglrenderer and its one driver (MoltenVK
//! or DXMT) from our own directory, sandboxes itself (the GPU, nothing
//! else), then serves its context. A guest app that breaks its driver
//! lands in a process with no other app's GPU state; a worker that dies
//! takes its context with it, nothing else.
//!
//! The server has no sandbox: a process can't add a sandbox to the one
//! it inherits on macOS, so whatever starts sandboxed workers must have
//! none. It is small on purpose. It reads four fixed-size requests from
//! the GPU process, never loads virglrenderer or any GPU code, and starts
//! only `vroom gpu-worker` with arguments it builds itself (the context's
//! backend comes from a fixed table, its name is reduced to plain
//! characters).
//!
//! Wire format (virglrenderer's server/render_protocol.h, macOS framing):
//! a 4-byte big-endian length, then the request; fds ride on the length.

use std::ffi::{c_int, c_void};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

use crate::{CAPSET_NEPTUNE, CAPSET_VENUS, NEPTUNE, NO_VIRGL, VENUS};

/// RENDER_SERVER_VERSION: the only one we speak.
const VERSION: u32 = 0;
/// virglrenderer's own limit (RENDER_SERVER_MAX_WORKER_COUNT).
const MAX_WORKERS: usize = 256;
/// The largest request (create context: 44 bytes).
const MAX_REQUEST: usize = 64;

const OP_NOP: u32 = 0;
const OP_INIT: u32 = 1;
const OP_RESET: u32 = 2;
const OP_CREATE_CONTEXT: u32 = 3;
const OP_DESTROY_CONTEXT: u32 = 4;

/// The flags a worker for this capset runs with (virgl_renderer_init's):
/// no GL and its one backend. None for capsets that don't use the server.
pub fn worker_flags(capset: u32) -> Option<u32> {
    match capset {
        CAPSET_VENUS => Some((NO_VIRGL | VENUS) as u32),
        CAPSET_NEPTUNE => Some((NO_VIRGL | NEPTUNE) as u32),
        _ => None,
    }
}

/// The GPU process's end of the server connection, once started.
static CLIENT: OnceLock<OwnedFd> = OnceLock::new();

/// Start the render server, from the GPU process (or the VMM with
/// `VROOM_GPU_IN_PROCESS`) after [`crate::load`] and before the sandbox.
/// Does nothing if the library has no worker in it: then it runs Venus
/// and Direct3D in a thread, as before. `exe` is `vroom`. Whether the
/// server runs.
pub fn start(exe: &Path) -> std::io::Result<bool> {
    if crate::lib().and_then(|l| l.worker_main).is_none() {
        log::warn!("gpu3d: virglrenderer has no render worker: Venus and Direct3D contexts share the GPU process");
        return Ok(false);
    }
    if CLIENT.get().is_some() {
        return Ok(true);
    }
    let (ours, theirs) = UnixStream::pair()?;
    // The socket is its stdin: nothing else crosses over, and no fd
    // numbers on the command line.
    let child = Command::new(exe).arg("gpu-server").stdin(Stdio::from(OwnedFd::from(theirs))).stdout(Stdio::null()).spawn()?;
    log::info!("gpu3d: render server {} up: one process per Venus or Direct3D context", child.id());
    // It exits when we do (its socket closes); nothing to wait for here.
    drop(child);
    let _ = CLIENT.set(ours.into());
    Ok(true)
}

/// The get_server_fd callback for virgl_renderer_init, if the server runs.
pub(crate) fn server_fd_callback() -> Option<extern "C" fn(*mut c_void, u32) -> c_int> {
    CLIENT.get().is_some().then_some(server_fd as extern "C" fn(*mut c_void, u32) -> c_int)
}

/// A new fd for the server connection (the proxy owns and closes it; we
/// keep ours, so a failed init attempt doesn't end the server).
extern "C" fn server_fd(_cookie: *mut c_void, version: u32) -> c_int {
    let Some(fd) = CLIENT.get() else { return -1 };
    if version != VERSION {
        log::warn!("gpu3d: render server version {version}, we speak {VERSION}");
        return -1;
    }
    // SAFETY: dup of an fd we own; the copy is the proxy's.
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) }
}

/// Move a socket we got as stdin to a fd of its own, and give stdin
/// /dev/null. For the server and the workers.
pub fn take_stdin() -> std::io::Result<OwnedFd> {
    // SAFETY: plain fcntl/open/dup2 on fd 0, which our parent set up for
    // us and nothing else in this process uses.
    unsafe {
        let fd = libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::close(null);
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// One worker: the context it serves.
#[derive(Debug)]
struct Worker {
    ctx: u32,
    child: Child,
}

/// `vroom gpu-server`: serve the GPU process on `sock` until it closes,
/// then stop every worker. `exe` is `vroom`.
pub fn serve(mut sock: UnixStream, exe: &Path) -> std::io::Result<()> {
    let mut workers: Vec<Worker> = Vec::new();
    // Backends the GPU process asked for (INIT): contexts of others are
    // refused.
    let mut backends = 0u32;
    let r = loop {
        // Wake up now and then to notice workers that died.
        let mut p = libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut p, 1, 1000) };
        reap(&mut workers);
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break Err(e);
        }
        if n == 0 {
            continue;
        }
        let req = match read_request(&mut sock) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(e) => break Err(e),
        };
        let word = |i: usize| req.get(i * 4..i * 4 + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
        match (word(0), req.len()) {
            (Some(OP_NOP), 4) => {}
            (Some(OP_INIT), 8) => backends = word(1).unwrap_or(0) & (VENUS | NEPTUNE) as u32,
            (Some(OP_RESET), 4) => {
                for w in workers.drain(..) {
                    stop(w);
                }
            }
            (Some(OP_CREATE_CONTEXT), 44) => {
                let (ctx, capset) = (word(1).unwrap_or(0), word(2).unwrap_or(0));
                let sock_for_proxy = create(&mut workers, exe, backends, ctx, capset, &req[12..44]);
                reply(&sock, sock_for_proxy)?;
            }
            (Some(OP_DESTROY_CONTEXT), 8) => {
                let ctx = word(1).unwrap_or(0);
                if let Some(i) = workers.iter().position(|w| w.ctx == ctx) {
                    stop(workers.swap_remove(i));
                }
            }
            (op, len) => log::warn!("gpu server: bad request (op {op:?}, {len} bytes)"),
        }
    };
    for w in workers.drain(..) {
        stop(w);
    }
    r
}

/// A new worker for a context, and the proxy's end of its socket. None if
/// the request isn't one we serve or the worker can't start.
fn create(workers: &mut Vec<Worker>, exe: &Path, backends: u32, ctx: u32, capset: u32, name: &[u8]) -> Option<UnixStream> {
    let flags = worker_flags(capset)?;
    if ctx == 0 || flags & backends == 0 || workers.iter().any(|w| w.ctx == ctx) {
        log::warn!("gpu server: refused context {ctx} (capset {capset})");
        return None;
    }
    if workers.len() >= MAX_WORKERS {
        log::warn!("gpu server: {MAX_WORKERS} contexts already, refused context {ctx}");
        return None;
    }
    let name = plain_name(name);
    let (ours, theirs) = UnixStream::pair().ok()?;
    let mut cmd = Command::new(exe);
    cmd.arg("gpu-worker").arg("--ctx").arg(ctx.to_string()).arg("--capset").arg(capset.to_string());
    if !name.is_empty() {
        cmd.arg("--name").arg(&name);
    }
    cmd.env_clear().envs(std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(passed_on)));
    let child = match cmd.stdin(Stdio::from(OwnedFd::from(theirs))).stdout(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("gpu server: can't start a worker for context {ctx}: {e}");
            return None;
        }
    };
    log::info!("gpu server: context {ctx} ({name}, capset {capset}) in process {}", child.id());
    workers.push(Worker { ctx, child });
    Some(ours)
}

/// What a worker inherits from our environment: logging, DXMT's shader
/// cache, and the drivers' debug switches. Never a variable that names a
/// library to load.
fn passed_on(k: &str) -> bool {
    matches!(k, "RUST_LOG" | "VROOM_SANDBOX_SELFTEST" | "DXMT_SHADER_CACHE_PATH")
        || (["MVK_", "DXMT_", "VKR_", "NPT_"].iter().any(|p| k.starts_with(p)) && !k.contains("LIBRARY") && !k.contains("PATH"))
}

/// The guest's name for a context (its process name, up to 31 bytes), as
/// letters, digits and `._-` only.
fn plain_name(raw: &[u8]) -> String {
    raw.iter()
        .take_while(|&&b| b != 0)
        .take(31)
        .map(|&b| if b.is_ascii_alphanumeric() || b"._-".contains(&b) { b as char } else { '_' })
        .collect()
}

fn stop(mut w: Worker) {
    let _ = w.child.kill();
    let _ = w.child.wait();
}

/// Forget workers that exited on their own; one that died is worth a line.
fn reap(workers: &mut Vec<Worker>) {
    workers.retain_mut(|w| match w.child.try_wait() {
        Ok(None) => true,
        Ok(Some(st)) => {
            if st.success() {
                log::info!("gpu server: worker for context {} done", w.ctx);
            } else {
                log::warn!("gpu server: worker for context {} (process {}) died: {st}; that context is gone, the others go on", w.ctx, w.child.id());
            }
            false
        }
        Err(_) => false,
    });
}

fn read_request(s: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut n = [0u8; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n) as usize;
    if !(4..=MAX_REQUEST).contains(&n) {
        return Err(std::io::Error::other(format!("request of {n} bytes")));
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

/// render_client_op_create_context_reply: `bool ok`, the socket with it.
fn reply(s: &UnixStream, sock: Option<UnixStream>) -> std::io::Result<()> {
    let ok = [sock.is_some() as u8];
    send(s, &1u32.to_be_bytes(), sock.as_ref().map(|s| s.as_raw_fd()))?;
    (&*s).write_all(&ok)
}

/// `bytes`, with `fd` attached if any (on the first byte, as the proxy
/// expects it with the length).
fn send(s: &UnixStream, bytes: &[u8], fd: Option<RawFd>) -> std::io::Result<()> {
    let Some(fd) = fd else { return (&*s).write_all(bytes) };
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut c_void, iov_len: bytes.len() };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid; the pointers below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut c_void;
    msg.msg_controllen = space as _;
    // SAFETY: msg has room for one cmsg of one fd (CMSG_SPACE above).
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
    }
    // SAFETY: valid socket and message.
    let n = unsafe { libc::sendmsg(s.as_raw_fd(), &msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The fd went with the first byte; the rest, if any, plainly.
    (&*s).write_all(&bytes[n as usize..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: u32, rest: &[u8]) -> Vec<u8> {
        let mut r = op.to_ne_bytes().to_vec();
        r.extend_from_slice(rest);
        let mut v = (r.len() as u32).to_be_bytes().to_vec();
        v.extend(r);
        v
    }

    #[test]
    fn names_and_flags() {
        assert_eq!(plain_name(b"vkcube\0junk"), "vkcube");
        assert_eq!(plain_name(b"a b/../c$(x)"), "a_b_.._c__x_");
        assert_eq!(plain_name(&[b'x'; 40]).len(), 31);
        assert_eq!(worker_flags(CAPSET_VENUS), Some((NO_VIRGL | VENUS) as u32));
        assert_eq!(worker_flags(1), None);
        assert!(passed_on("RUST_LOG") && passed_on("MVK_CONFIG_LOG_LEVEL"));
        assert!(!passed_on("NPT_D3D11_LIBRARY_PATH") && !passed_on("DYLD_INSERT_LIBRARIES") && !passed_on("HOME"));
    }

    /// The server's side of the protocol, with `true` standing in for
    /// the worker: refused contexts get `ok = 0` and no fd, good ones a
    /// socket; bad framing ends the server.
    #[test]
    fn create_and_refuse() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || serve(server, Path::new("/usr/bin/true")));
        let create = |ctx: u32, capset: u32| {
            let mut b = ctx.to_ne_bytes().to_vec();
            b.extend(capset.to_ne_bytes());
            b.extend([b'a'; 32]);
            request(OP_CREATE_CONTEXT, &b)
        };
        let answer = |c: &mut UnixStream| {
            let mut len = [0u8; 4];
            let mut byte = [0u8; 1];
            // recvmsg for a possible fd with the length.
            let mut cbuf = vec![0u8; 64];
            let mut iov = libc::iovec { iov_base: len.as_mut_ptr() as *mut c_void, iov_len: 4 };
            // SAFETY: zeroed msghdr; buffers outlive the call.
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr() as *mut c_void;
            msg.msg_controllen = cbuf.len() as _;
            // SAFETY: valid socket and buffers.
            assert_eq!(unsafe { libc::recvmsg(c.as_raw_fd(), &mut msg, 0) }, 4);
            // SAFETY: the kernel filled msg.
            let got_fd = unsafe { !libc::CMSG_FIRSTHDR(&msg).is_null() };
            c.read_exact(&mut byte).unwrap();
            (u32::from_be_bytes(len), byte[0], got_fd)
        };
        // Before INIT, no backend: refused.
        client.write_all(&create(1, CAPSET_VENUS)).unwrap();
        assert_eq!(answer(&mut client), (1, 0, false));
        client.write_all(&request(OP_INIT, &((VENUS | NO_VIRGL) as u32).to_ne_bytes())).unwrap();
        client.write_all(&create(1, CAPSET_VENUS)).unwrap();
        assert_eq!(answer(&mut client), (1, 1, true));
        // Same id again, a backend not asked for, GL: refused.
        for (ctx, cap) in [(1, CAPSET_VENUS), (2, CAPSET_NEPTUNE), (3, 2)] {
            client.write_all(&create(ctx, cap)).unwrap();
            assert_eq!(answer(&mut client), (1, 0, false));
        }
        client.write_all(&request(OP_DESTROY_CONTEXT, &1u32.to_ne_bytes())).unwrap();
        // A length the protocol never uses: the server stops.
        client.write_all(&1000u32.to_be_bytes()).unwrap();
        assert!(t.join().unwrap().is_err());
    }
}
