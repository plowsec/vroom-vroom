//! vroom guest agent.
//!
//! Talks to the host over the virtio-console port `org.vroom.agent.0`
//! (or the only port there is, see find.rs).
//! The host is trusted more than the guest, but we still validate
//! everything (sizes, UTF-8) via vroom-proto.

// Windows: a background app started at logon, no console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod clip;
#[cfg(windows)]
mod coh_win;
#[cfg(target_os = "linux")]
mod coh_x11;
mod coherence;
// X11 arithmetic for coh_x11; built everywhere so it's tested everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod ewmh;
#[cfg(windows)]
mod dnd_win;
// The session side; built everywhere so it's tested everywhere.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod drop;
// Built everywhere so it's tested everywhere; only Windows and X11 find
// drags to take.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod dragout;
#[cfg(target_os = "linux")]
mod dnd_x11;
mod find;
mod layout;
mod port;
// Used by the service mode (Windows, Linux); built everywhere so it's
// tested everywhere.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod relay;
mod scale;
// Windows only; the pure parts are tested everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
mod shares;
mod sys;
#[cfg(windows)]
mod winsvc;
#[cfg(target_os = "linux")]
mod linsvc;

#[cfg(windows)]
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use vroom_proto::{features, Decoder, Message, AGENT_PORT};

const POLL: Duration = Duration::from_millis(300);
pub(crate) const MAX_CLIP: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
enum Role {
    /// Everything in one process (Linux; Windows run elevated).
    Full,
    /// Windows service: host port, clock, shutdown; desktop work relayed.
    Service,
    /// Windows, unprivileged, in the user's session: clipboard, resolution.
    Session,
}

struct Args {
    /// None: find it (find.rs).
    port: Option<String>,
    clipboard_file: Option<String>,
    no_clipboard: bool,
    role: Role,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { port: None, clipboard_file: None, no_clipboard: false, role: Role::Full };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--port" => a.port = Some(it.next().ok_or("--port needs a value")?),
            "--clipboard-file" => a.clipboard_file = Some(it.next().ok_or("--clipboard-file needs a path")?),
            "--no-clipboard" => a.no_clipboard = true,
            #[cfg(any(windows, target_os = "linux"))]
            "--service" => a.role = Role::Service,
            #[cfg(windows)]
            "--mount-shares" => {
                // Needs admin rights (it makes services): setup runs it.
                match shares::mount_all() {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        log::error!("shares: {e}");
                        std::process::exit(1);
                    }
                }
            }
            #[cfg(windows)]
            "--session" => {
                a.role = Role::Session;
                a.port = Some(winsvc::PIPE.into());
            }
            #[cfg(target_os = "linux")]
            "--session" => {
                a.role = Role::Session;
                a.port = Some(linsvc::SOCKET.into());
            }
            "-h" | "--help" => {
                println!("vroom-agent [--port PATH] [--clipboard-file PATH | --no-clipboard] [--service | --session | --mount-shares]");
                println!();
                println!("--port PATH  host channel. Default: virtio-serial port {AGENT_PORT}.");
                println!("             If it's missing ([identity].agent_port renamed it), the");
                println!("             only port there is. Several ports: pass --port.");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn main() {
    let mut logger = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    // No console on Windows: log to %LOCALAPPDATA%\Vroom\agent.log.
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Vroom")) {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(f) = OpenOptions::new().create(true).append(true).open(dir.join("agent.log")) {
            logger.target(env_logger::Target::Pipe(Box::new(f)));
        }
    }
    logger.init();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vroom-agent: {e}");
            std::process::exit(2);
        }
    };
    #[cfg(windows)]
    if args.role == Role::Service {
        if let Err(e) = winsvc::run_service(args.port) {
            log::error!("not started by the service manager: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(target_os = "linux")]
    if args.role == Role::Service {
        if let Err(e) = linsvc::run_service(args.port.as_deref()) {
            log::error!("service: {e}");
            std::process::exit(1);
        }
        return;
    }
    loop {
        if let Err(e) = session(&args) {
            log::warn!("session ended: {e}");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Find and open the host port: `--port`, else find.rs. Ok has the
/// path, for logs.
pub(crate) fn open_port(explicit: Option<&str>) -> std::io::Result<(String, port::Reader, port::Writer)> {
    let path = match explicit {
        Some(p) => p.to_string(),
        None => find::port_path()?,
    };
    let (r, w) = port::open(&path).map_err(|e| std::io::Error::new(e.kind(), format!("{path}: {e}")))?;
    Ok((path, r, w))
}

fn open_link(args: &Args) -> std::io::Result<(String, port::Reader, port::Writer)> {
    #[cfg(windows)]
    if args.role == Role::Session {
        let path = args.port.as_deref().unwrap_or(winsvc::PIPE);
        let (r, w) = port::open_checked(path, winsvc::check_server)?;
        return Ok((path.to_string(), r, w));
    }
    #[cfg(target_os = "linux")]
    if args.role == Role::Session {
        let (r, w) = linsvc::connect()?;
        return Ok((linsvc::SOCKET.to_string(), r, w));
    }
    open_port(args.port.as_deref())
}

fn session(args: &Args) -> std::io::Result<()> {
    let (path, mut reader, writer) = open_link(args)?;
    let mut writer = dragout::Shared::new(writer);
    let system = args.role == Role::Full;
    sys::single_display();
    let mut clip: Box<dyn clip::Clip> = if args.no_clipboard {
        Box::new(clip::NoClip)
    } else if let Some(p) = &args.clipboard_file {
        Box::new(clip::FileClip::new(p))
    } else {
        clip::system()
    };

    if system {
        writer.write_all(&hello().encode())?;
    }
    log::info!("connected to {path}");

    // Blocking reads on their own thread; main loop polls the clipboard.
    // Coherence's window events wake it too.
    let (tx, rx) = mpsc::channel::<In>();
    let wake = std::sync::Mutex::new(tx.clone());
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 << 10];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(In::Host(buf[..n].to_vec())).is_err() {
                break;
            }
        }
        let _ = tx.send(In::Closed);
    });

    let mut dec = Decoder::new(MAX_CLIP + 64);
    let mut last = clip.get();
    let mut drops = drop::Drops::new();
    // Dropped (taskbar back) when the session ends.
    let mut coh = coherence::Coherence::new(std::sync::Arc::new(move || {
        let _ = wake.lock().unwrap_or_else(|e| e.into_inner()).send(In::Wake);
    }));
    let mut clip_at = std::time::Instant::now();
    let mut dragout = dragout::DragOut::new();
    loop {
        match rx.recv_timeout(coh.wait().unwrap_or(POLL).min(POLL)) {
            // Coherence: something changed on screen.
            Ok(In::Wake) => {}
            Ok(In::Closed) => return Err(std::io::Error::other("port closed")),
            Ok(In::Host(bytes)) => {
                dec.push(&bytes);
                loop {
                    match dec.next() {
                        Ok(Some(m)) => {
                            if !drops.handle(&m, &mut writer)? && !coh.handle(&m, &mut writer)? && !dragout.handle(&m, &writer)? {
                                handle(m, system, &mut writer, clip.as_mut(), &mut last)?
                            }
                        }
                        Ok(None) => break,
                        Err(e) => log::warn!("bad message from host: {e}"),
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::other("port closed"));
            }
        }
        coh.tick(&mut writer)?;
        if clip_at.elapsed() < POLL {
            continue;
        }
        clip_at = std::time::Instant::now();
        // Local copy happened? Tell the host.
        let now = clip.get();
        if now.is_some() && now != last {
            last = now.clone();
            if let Some(t) = now {
                if t.len() <= MAX_CLIP {
                    writer.write_all(&Message::ClipboardText(t).encode())?;
                }
            }
        }
    }
}

/// What wakes the session loop.
enum In {
    Host(Vec<u8>),
    /// The host link is gone.
    Closed,
    /// Coherence: the windows changed.
    Wake,
}

fn hello() -> Message {
    let mut features = features::CLIPBOARD_TEXT | features::TIME_SYNC | features::SHUTDOWN | features::SCALE | features::FILE_DROP;
    if cfg!(any(windows, target_os = "linux")) {
        features |= features::DRAG_OUT | features::MONITORS;
    }
    if cfg!(windows) {
        features |= features::GPU3D;
    }
    if coherence::Coherence::supported() {
        features |= features::COHERENCE;
    }
    if coherence::Coherence::starts_apps() {
        features |= features::START_APP;
    }
    Message::Hello { features, agent: format!("vroom-agent {}", env!("CARGO_PKG_VERSION")) }
}

/// `system`: we own the host link and may act on privileged requests.
/// The session helper only ever gets clipboard, resolution and scale messages.
fn handle(m: Message, system: bool, w: &mut dyn Write, clip: &mut dyn clip::Clip, last: &mut Option<String>) -> std::io::Result<()> {
    match m {
        Message::Hello { .. } | Message::TimeSync { .. } | Message::Shutdown { .. } | Message::Ping(_) if !system => {
            log::debug!("ignoring a privileged message on the session link");
        }
        // The host says hello when it (re)starts, e.g. resuming a
        // suspended VM in a new process: introduce ourselves again.
        Message::Hello { agent, .. } => {
            log::info!("host: {agent}");
            w.write_all(&hello().encode())?;
        }
        Message::ClipboardText(t) => {
            clip.set(&t);
            // Don't bounce it straight back.
            *last = Some(t);
        }
        Message::TimeSync { unix_secs, nanos } => {
            if let Err(e) = sys::set_time(unix_secs, nanos) {
                log::warn!("time sync failed (need privileges?): {e}");
            }
        }
        Message::Shutdown { reboot } => sys::shutdown(reboot),
        Message::SetResolution { width, height } => {
            scale::note_resolution(width, height);
            if let Err(e) = sys::set_resolution(width, height) {
                log::debug!("resolution {width}x{height}: {e}");
            }
        }
        Message::SetLayout { monitors } => {
            if let Some(Some(m)) = monitors.first() {
                scale::note_resolution(m.width, m.height);
            }
            if let Err(e) = layout::apply(&monitors) {
                log::info!("monitors: {e}");
            }
        }
        Message::SetScale { percent } => scale::set(percent),
        Message::Mouse { x, y, buttons, wheel } => layout::mouse(x, y, buttons, wheel),
        Message::Ping(v) => w.write_all(&Message::Pong(v).encode())?,
        // Windows' service handles it (relay); nothing to turn off here.
        Message::Gpu3d { .. } | Message::Pong(_) => {}
        // Drops are drop.rs's, Coherence coherence.rs's; the rest is
        // guest -> host.
        Message::DragEnter { .. }
        | Message::DragCancel { .. }
        | Message::DropEntry { .. }
        | Message::DropData { .. }
        | Message::DropEnd { .. }
        | Message::DragReady { .. }
        | Message::DropDone { .. }
        | Message::Coherence { .. }
        | Message::WindowDo { .. }
        | Message::Windows(_)
        | Message::DragOutQuery { .. }
        | Message::DragOutItems { .. }
        | Message::DragOutGet { .. }
        | Message::DragOutEnd { .. }
        | Message::DragOutFail { .. }
        | Message::StartApp(_)
        | Message::WindowIcon { .. }
        | Message::CoherenceNo(_)
        => {}
    }
    Ok(())
}
