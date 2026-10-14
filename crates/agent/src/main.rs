//! bulwark guest agent.
//!
//! Talks to the host over the virtio-console port `org.bulwark.agent.0`.
//! The host is trusted more than the guest, but we still validate
//! everything (sizes, UTF-8) via bulwark-proto.

mod clip;
mod sys;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use bulwark_proto::{features, Decoder, Message, AGENT_PORT};

const POLL: Duration = Duration::from_millis(300);
const MAX_CLIP: usize = 16 << 20;

fn port_path() -> String {
    if cfg!(windows) {
        format!(r"\\.\Global\{AGENT_PORT}")
    } else {
        format!("/dev/virtio-ports/{AGENT_PORT}")
    }
}

struct Args {
    port: String,
    clipboard_file: Option<String>,
    no_clipboard: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { port: port_path(), clipboard_file: None, no_clipboard: false };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--port" => a.port = it.next().ok_or("--port needs a value")?,
            "--clipboard-file" => a.clipboard_file = Some(it.next().ok_or("--clipboard-file needs a path")?),
            "--no-clipboard" => a.no_clipboard = true,
            "-h" | "--help" => {
                println!("bulwark-agent [--port PATH] [--clipboard-file PATH | --no-clipboard]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bulwark-agent: {e}");
            std::process::exit(2);
        }
    };
    loop {
        if let Err(e) = session(&args) {
            log::warn!("session ended: {e}");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn session(args: &Args) -> std::io::Result<()> {
    let port = OpenOptions::new().read(true).write(true).open(&args.port)?;
    let mut reader: File = port.try_clone()?;
    let mut writer = port;
    let mut clip: Box<dyn clip::Clip> = if args.no_clipboard {
        Box::new(clip::NoClip)
    } else if let Some(p) = &args.clipboard_file {
        Box::new(clip::FileClip::new(p))
    } else {
        clip::system()
    };

    let feats = features::CLIPBOARD_TEXT | features::TIME_SYNC | features::SHUTDOWN;
    writer.write_all(&Message::Hello { features: feats, agent: format!("bulwark-agent {}", env!("CARGO_PKG_VERSION")) }.encode())?;
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
                        Ok(Some(m)) => handle(m, &mut writer, clip.as_mut(), &mut last)?,
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

fn handle(m: Message, w: &mut File, clip: &mut dyn clip::Clip, last: &mut Option<String>) -> std::io::Result<()> {
    match m {
        Message::Hello { agent, .. } => log::info!("host: {agent}"),
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
