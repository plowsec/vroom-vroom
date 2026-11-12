//! Linux service mode (`--service`, root, from systemd) and the socket the
//! session helper (`--session`, in the user's desktop) talks to.
//!
//! Socket rules:
//! - `/run/vroom-agent.sock`: /run is root's, nobody can squat the name;
//!   the helper also checks its peer is root.
//! - The service only serves the user logind says is active on seat0, so
//!   another local user can't listen in on the clipboard. Without logind:
//!   the owner of the active VT; neither known: root only. Checked again
//!   per message (relay.rs): a user switched away from loses its helper.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use crate::port::{Reader, Writer};
use crate::relay::{Ev, RealSystem, Relay};

pub const SOCKET: &str = "/run/vroom-agent.sock";

#[allow(unsafe_code)]
fn peer_uid(s: &UnixStream) -> std::io::Result<u32> {
    // SAFETY: zeroed ucred is valid plain data; getsockopt fills it.
    let mut c: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid socket, out pointer and length.
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut c as *mut libc::ucred).cast(), &mut len) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(c.uid)
}

/// Set by SIGTERM (systemd stopping us).
static TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    TERM.store(true, Ordering::Release);
}

/// Whether systemd is taking the whole system down (shutdown, reboot), not
/// just restarting us.
fn system_stopping() -> bool {
    std::process::Command::new("systemctl")
        .arg("is-system-running")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "stopping")
}

/// On SIGTERM: if the system is going down, tell the host first (its
/// window says "Shutting down" rather than showing a dark screen).
#[allow(unsafe_code)]
fn watch_term(tx: Sender<Ev>) {
    // SAFETY: the handler only stores an atomic flag.
    unsafe { libc::signal(libc::SIGTERM, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        if TERM.load(Ordering::Acquire) {
            if system_stopping() {
                let _ = tx.send(Ev::Stopping);
                std::thread::sleep(Duration::from_millis(300));
            }
            std::process::exit(0);
        }
    });
}

/// `port`: from `--port`; None finds it on every try.
pub fn run_service(port: Option<&str>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(SOCKET);
    let l = UnixListener::bind(SOCKET)?;
    // Anyone may connect; who gets served is checked per connection.
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))?;
    let (tx, rx) = channel::<Ev>();
    watch_term(tx.clone());
    {
        let tx = tx.clone();
        std::thread::spawn(move || accept_helpers(l, tx));
    }
    let mut relay = Relay::new(RealSystem::default(), crate::MAX_CLIP, tx.clone());
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

/// A helper's link that hangs up when the relay drops it (replaced, or
/// its user left the screen): the reader's clone alone would keep it
/// open, and the helper would never reconnect.
struct Hangup(UnixStream);

impl Write for Hangup {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Drop for Hangup {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

fn accept_helpers(l: UnixListener, tx: Sender<Ev>) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    for s in l.incoming() {
        let Ok(s) = s else { continue };
        let uid = peer_uid(&s).ok();
        let active = crate::seat::active_uid();
        let Some(uid) = uid.filter(|_| crate::seat::serves(uid, active)) else {
            log::warn!("service: refusing helper of uid {uid:?} (active user {active:?})");
            continue;
        };
        let Ok(r) = s.try_clone() else { continue };
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let w: Writer = Box::new(Hangup(s));
        if tx.send(Ev::HelperUp(id, uid, w)).is_err() {
            return;
        }
        spawn_reader(Box::new(r), tx.clone(), move |b| Ev::Helper(id, b), move || Ev::HelperGone(id));
    }
}

/// Session helper side: connect, and only to a root peer.
pub fn connect() -> std::io::Result<(Reader, Writer)> {
    let s = UnixStream::connect(SOCKET)?;
    if peer_uid(&s)? != 0 {
        return Err(std::io::Error::other("agent socket not served by root"));
    }
    Ok((Box::new(s.try_clone()?), Box::new(s)))
}
