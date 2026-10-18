//! Linux service mode (`--service`, root, from systemd) and the socket the
//! session helper (`--session`, in the user's desktop) talks to.
//!
//! Socket rules:
//! - `/run/vroom-agent.sock`: /run is root's, nobody can squat the name;
//!   the helper also checks its peer is root.
//! - The service only serves the user logind says is active on seat0, so
//!   another local user can't listen in on the clipboard.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// The user on seat0's screen right now (logind's view), if any.
fn active_uid() -> Option<u32> {
    let s = std::fs::read_to_string("/run/systemd/seats/seat0").ok()?;
    s.lines().find_map(|l| l.strip_prefix("ACTIVE_UID=")).and_then(|v| v.trim().parse().ok())
}

/// `port`: from `--port`; None finds it on every try.
pub fn run_service(port: Option<&str>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(SOCKET);
    let l = UnixListener::bind(SOCKET)?;
    // Anyone may connect; who gets served is checked per connection.
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666))?;
    let (tx, rx) = channel::<Ev>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || accept_helpers(l, tx));
    }
    let mut relay = Relay::new(RealSystem::default(), crate::MAX_CLIP);
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

fn accept_helpers(l: UnixListener, tx: Sender<Ev>) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    for s in l.incoming() {
        let Ok(s) = s else { continue };
        let uid = peer_uid(&s).ok();
        let active = active_uid();
        // No logind (minimal systems): any local user's helper.
        if active.is_some() && uid != active && uid != Some(0) {
            log::warn!("service: refusing helper of uid {uid:?} (active user {active:?})");
            continue;
        }
        let Ok(r) = s.try_clone() else { continue };
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let w: Writer = Box::new(s);
        if tx.send(Ev::HelperUp(id, w)).is_err() {
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
