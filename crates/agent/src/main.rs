//! vroom guest agent.
//!
//! Talks to the host over the virtio-console port `org.vroom.agent.0`.
//! The host is trusted more than the guest, but we still validate
//! everything (sizes, UTF-8) via vroom-proto.

// Windows: a background app started at logon, no console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod clip;
mod port;
// Used by the service mode (Windows, Linux); built everywhere so it's
// tested everywhere.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
mod relay;
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

fn port_path() -> String {
    if cfg!(windows) {
        format!(r"\\.\Global\{AGENT_PORT}")
    } else {
        format!("/dev/virtio-ports/{AGENT_PORT}")
    }
}

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
    port: String,
    clipboard_file: Option<String>,
    no_clipboard: bool,
    role: Role,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { port: port_path(), clipboard_file: None, no_clipboard: false, role: Role::Full };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--port" => a.port = it.next().ok_or("--port needs a value")?,
            "--clipboard-file" => a.clipboard_file = Some(it.next().ok_or("--clipboard-file needs a path")?),
            "--no-clipboard" => a.no_clipboard = true,
            #[cfg(any(windows, target_os = "linux"))]
            "--service" => a.role = Role::Service,
            #[cfg(windows)]
            "--session" => {
                a.role = Role::Session;
                a.port = winsvc::PIPE.into();
            }
            #[cfg(target_os = "linux")]
            "--session" => {
                a.role = Role::Session;
                a.port = linsvc::SOCKET.into();
            }
            "-h" | "--help" => {
                println!("vroom-agent [--port PATH] [--clipboard-file PATH | --no-clipboard] [--service | --session]");
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
        if let Err(e) = winsvc::run_service() {
            log::error!("not started by the service manager: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(target_os = "linux")]
    if args.role == Role::Service {
        if let Err(e) = linsvc::run_service(&args.port) {
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

fn open_link(args: &Args) -> std::io::Result<(port::Reader, port::Writer)> {
    #[cfg(windows)]
    if args.role == Role::Session {
        return port::open_checked(&args.port, winsvc::check_server);
    }
    #[cfg(target_os = "linux")]
    if args.role == Role::Session {
        return linsvc::connect();
    }
    port::open(&args.port)
}

fn session(args: &Args) -> std::io::Result<()> {
    let (mut reader, mut writer) = open_link(args)?;
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
    log::info!("connected to {}", args.port);

    // Blocking reads on their own thread; main loop polls the clipboard.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 << 10];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut dec = Decoder::new(MAX_CLIP + 64);
    let mut last = clip.get();
    loop {
        match rx.recv_timeout(POLL) {
            Ok(bytes) => {
                dec.push(&bytes);
                loop {
                    match dec.next() {
                        Ok(Some(m)) => handle(m, system, &mut writer, clip.as_mut(), &mut last)?,
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

fn hello() -> Message {
    let features = features::CLIPBOARD_TEXT | features::TIME_SYNC | features::SHUTDOWN;
    Message::Hello { features, agent: format!("vroom-agent {}", env!("CARGO_PKG_VERSION")) }
}

/// `system`: we own the host link and may act on privileged requests.
/// The session helper only ever gets clipboard and resolution messages.
fn handle(m: Message, system: bool, w: &mut port::Writer, clip: &mut dyn clip::Clip, last: &mut Option<String>) -> std::io::Result<()> {
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
            if let Err(e) = sys::set_resolution(width, height) {
                log::debug!("resolution {width}x{height}: {e}");
            }
        }
        Message::Ping(v) => w.write_all(&Message::Pong(v).encode())?,
        Message::Pong(_) => {}
    }
    Ok(())
}
