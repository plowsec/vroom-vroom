//! The VM's control socket (`vroom guest ...`): one request per
//! connection, as agent frames (ids unused).
//! - `Exec` -> `ExecDone`; one refused here comes back `Failed`, stderr
//!   says why.
//! - `UpdateBegin`, its `UpdateData`, then the caller's end of writing ->
//!   `UpdateDone`.
//! - `FilePut`, then its `size` bytes as they are (no frames) ->
//!   `FileDone`.
//! - `FileGet` -> `FileData`..., `FileDone`.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use vroom_proto::{Decoder, ExecEnd, Message, FILE_CHUNK, MAGIC};

use crate::agent::{Agent, ExecReq};

/// Where the `vroom` command finds the socket: this file in the state dir.
pub const CONTROL_FILE: &str = "control";

pub(crate) fn serve(l: UnixListener, agent: Agent) -> std::io::Result<()> {
    std::thread::Builder::new().name("control".into()).spawn(move || {
        for s in l.incoming().flatten() {
            let agent = agent.clone();
            let _ = std::thread::Builder::new().name("control-req".into()).spawn(move || {
                if let Err(e) = one(s, &agent) {
                    log::info!("control: {e}");
                }
            });
        }
    })?;
    Ok(())
}

fn file_done(r: Result<crate::agent::FileOut, String>) -> Message {
    match r {
        Ok(o) => Message::FileDone { id: 0, ok: true, size: o.size, sha256: o.sha256, why: String::new() },
        Err(why) => Message::FileDone { id: 0, ok: false, size: 0, sha256: [0; 32], why },
    }
}

/// Bytes written to it go out as `FileData` frames.
struct Framed(UnixStream);

impl Write for Framed {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = b.len().min(FILE_CHUNK);
        self.0.write_all(&Message::FileData { id: 0, data: b[..n].to_vec() }.encode())?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The first frame, read to its end and no further (what follows may be
/// for someone else), as bytes and decoded.
fn read_frame(s: &mut UnixStream) -> std::io::Result<(Vec<u8>, Message)> {
    let mut f = vec![0u8; 8];
    s.read_exact(&mut f)?;
    let len = u32::from_le_bytes([f[4], f[5], f[6], f[7]]) as usize;
    if f[..2] != MAGIC || len > 1 << 20 {
        return Err(std::io::Error::other("not a request"));
    }
    f.resize(8 + len, 0);
    s.read_exact(&mut f[8..])?;
    let mut d = Decoder::new(1 << 20);
    d.push(&f);
    let m = d.next().map_err(std::io::Error::other)?.ok_or_else(|| std::io::Error::other("not a request"))?;
    Ok((f, m))
}

fn one(mut s: UnixStream, agent: &Agent) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let (raw, m) = read_frame(&mut s)?;
    let (shell, timeout_ms, max_out, command) = match m {
        Message::Exec { shell, timeout_ms, max_out, command, .. } => (shell, timeout_ms, max_out, command),
        Message::UpdateBegin { version, .. } => {
            log::info!("control: guest tools {version}");
            s.set_read_timeout(Some(Duration::from_secs(60)))?;
            // The update reads frames from a pipe to its end: this one,
            // then the rest of what the caller sends.
            let (mut a, b) = UnixStream::pair()?;
            let mut rest = s.try_clone()?;
            std::thread::Builder::new().name("control-update".into()).spawn(move || {
                if a.write_all(&raw).is_ok() {
                    let _ = std::io::copy(&mut rest, &mut a);
                }
            })?;
            let (ok, why) = match agent.update(b.into()) {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e),
            };
            return s.write_all(&Message::UpdateDone { id: 0, ok, why }.encode());
        }
        Message::FilePut { size, path, .. } => {
            log::info!("control: file to the guest's {path}");
            s.set_read_timeout(Some(Duration::from_secs(60)))?;
            let r = agent.put(path, size, Box::new(s.try_clone()?.take(size)));
            return s.write_all(&file_done(r).encode());
        }
        Message::FileGet { path, .. } => {
            log::info!("control: the guest's {path}");
            s.set_write_timeout(Some(Duration::from_secs(60)))?;
            let r = agent.get(path, Box::new(Framed(s.try_clone()?)));
            return s.write_all(&file_done(r).encode());
        }
        _ => return Err(std::io::Error::other("not a request")),
    };
    log::info!("control: exec {command}");
    let r = agent.exec(ExecReq { shell, command, timeout: Duration::from_millis(timeout_ms as u64), max_out });
    let reply = match r {
        Ok(o) => Message::ExecDone { id: 0, end: o.end, truncated: o.truncated, stdout: o.stdout, stderr: o.stderr },
        Err(why) => Message::ExecDone { id: 0, end: ExecEnd::Failed, truncated: false, stdout: Vec::new(), stderr: why.into_bytes() },
    };
    s.write_all(&reply.encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{spawn, Screen};
    use sha2::Digest;
    use vroom_proto::{features, Shell};

    #[test]
    fn a_request_reaches_the_agent_and_its_answer_comes_back() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let agent = spawn(host, Default::default(), None, Screen::default()).unwrap();
        guest.write_all(&Message::Hello { features: features::EXEC | features::UPDATE | features::FILES, agent: "t".into() }.encode()).unwrap();
        // The guest agent: answers what it is asked.
        std::thread::spawn(move || {
            let mut d = Decoder::new(1 << 20);
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = guest.read(&mut buf) {
                d.push(&buf[..n]);
                while let Ok(Some(m)) = d.next() {
                    match m {
                        Message::Exec { id, command, .. } => {
                            let done = Message::ExecDone { id, end: ExecEnd::Exited(2), truncated: true, stdout: command.into_bytes(), stderr: Vec::new() };
                            guest.write_all(&done.encode()).unwrap();
                        }
                        Message::FileGet { id, .. } => {
                            guest.write_all(&Message::FileData { id, data: b"abc".to_vec() }.encode()).unwrap();
                            let sha256 = sha2::Sha256::digest(b"abc").into();
                            guest.write_all(&Message::FileDone { id, ok: true, size: 3, sha256, why: String::new() }.encode()).unwrap();
                        }
                        Message::FileData { id, data } => {
                            let sha256 = sha2::Sha256::digest(&data).into();
                            guest.write_all(&Message::FileDone { id, ok: true, size: data.len() as u64, sha256, why: String::new() }.encode()).unwrap();
                        }
                        Message::UpdateData { id, data } => {
                            guest.write_all(&Message::UpdateDone { id, ok: data == b"agent", why: "bad".into() }.encode()).unwrap();
                        }
                        _ => {}
                    }
                }
            }
        });
        let talk = |ms: &[Message], raw: &[u8]| {
            let (mut cli, vmm) = UnixStream::pair().unwrap();
            let a = agent.clone();
            let t = std::thread::spawn(move || one(vmm, &a));
            for m in ms {
                cli.write_all(&m.encode()).unwrap();
            }
            cli.write_all(raw).unwrap();
            cli.shutdown(std::net::Shutdown::Write).unwrap();
            t.join().unwrap().unwrap();
            let mut b = Vec::new();
            cli.read_to_end(&mut b).unwrap();
            let mut d = Decoder::new(1 << 20);
            d.push(&b);
            std::iter::from_fn(|| d.next().unwrap()).collect::<Vec<_>>()
        };
        let ask = |ms: &[Message]| talk(ms, &[]).pop().unwrap();
        let exec = |command: &str| Message::Exec { id: 9, shell: Shell::Default, timeout_ms: 5000, max_out: 64, command: command.into() };
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(ask(&[exec("netsh")]), Message::ExecDone { id: 0, end: ExecEnd::Exited(2), truncated: true, stdout: b"netsh".to_vec(), stderr: Vec::new() });
        // Guest tools: the frames after the first reach the guest too.
        let files = vec![vroom_proto::UpdateFile { path: "vroom-agent".into(), size: 5, sha256: [0; 32] }];
        let update = [Message::UpdateBegin { id: 1, version: "0.1.0".into(), files }, Message::UpdateData { id: 1, data: b"agent".to_vec() }];
        assert_eq!(ask(&update), Message::UpdateDone { id: 0, ok: true, why: String::new() });
        // Files: raw bytes in, frames out.
        let sha = |b: &[u8]| -> [u8; 32] { sha2::Sha256::digest(b).into() };
        let put = talk(&[Message::FilePut { id: 0, size: 4, path: "/tmp/x".into() }], b"wxyz");
        assert_eq!(put, [Message::FileDone { id: 0, ok: true, size: 4, sha256: sha(b"wxyz"), why: String::new() }]);
        let get = talk(&[Message::FileGet { id: 0, path: "/tmp/x".into() }], &[]);
        assert_eq!(get, [Message::FileData { id: 0, data: b"abc".to_vec() }, Message::FileDone { id: 0, ok: true, size: 3, sha256: sha(b"abc"), why: String::new() }]);
        agent.send(crate::agent::AgentCmd::Stop);
    }
}
