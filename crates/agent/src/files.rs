//! Files the host puts in the guest and gets from it (`FilePut`,
//! `FileGet`), as the agent's user. The disk work runs on threads of
//! their own; a file going out waits for each piece to be written to the
//! host (the link sets the pace, nothing piles up).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use vroom_proto::{Message, FILE_CHUNK};

/// Writes a frame to the host, once it's out.
pub type Send = Arc<dyn Fn(Message) -> io::Result<()> + std::marker::Send + Sync>;

/// A file coming in that stops this long is given up.
const IDLE: Duration = if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(60) };

#[derive(Default)]
pub struct Files {
    /// Files coming in: their writer, and the bytes still to come.
    puts: HashMap<u32, (SyncSender<Vec<u8>>, u64)>,
}

impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Files").field("puts", &self.puts.len()).finish()
    }
}

fn failed(id: u32, why: String) -> Message {
    Message::FileDone { id, ok: false, size: 0, sha256: [0; 32], why }
}

impl Files {
    /// A `FilePut`, `FileGet` or `FileData` from the host.
    pub fn handle(&mut self, m: Message, send: &Send) {
        match m {
            Message::FilePut { id, size, path } => {
                log::info!("file {id}: {path} from the host, {size} bytes");
                let (tx, rx) = sync_channel::<Vec<u8>>(16);
                let send = send.clone();
                std::thread::spawn(move || {
                    let r = match receive(&path, size, |t| rx.recv_timeout(t)) {
                        Ok(sha256) => Message::FileDone { id, ok: true, size, sha256, why: String::new() },
                        Err(e) => failed(id, format!("{path}: {e}")),
                    };
                    let _ = send(r);
                });
                // Nothing to wait for: the writer finishes alone.
                if size > 0 {
                    self.puts.insert(id, (tx, size));
                }
            }
            Message::FileData { id, data } => {
                let Some((tx, left)) = self.puts.get_mut(&id) else { return };
                let n = data.len() as u64;
                // More than it said: the writer, cut off, says so.
                if n > *left || tx.send(data).is_err() {
                    self.puts.remove(&id);
                    return;
                }
                *left -= n;
                if *left == 0 {
                    self.puts.remove(&id);
                }
            }
            Message::FileGet { id, path } => {
                log::info!("file {id}: {path} to the host");
                let send = send.clone();
                std::thread::spawn(move || {
                    if let Err(e) = give(id, &path, &send) {
                        let _ = send(failed(id, format!("{path}: {e}")));
                    }
                });
            }
            _ => {}
        }
    }
}

/// Write `size` bytes from `next` to `path` (through a file beside it,
/// so a file cut short never replaces one). Their SHA-256.
fn receive(path: &str, size: u64, next: impl Fn(Duration) -> Result<Vec<u8>, RecvTimeoutError>) -> io::Result<[u8; 32]> {
    let part = format!("{path}.vroom-part");
    let r = (|| {
        let mut f = std::fs::File::create(&part)?;
        let mut h = Sha256::new();
        let mut got = 0u64;
        while got < size {
            let b = next(IDLE).map_err(|e| match e {
                RecvTimeoutError::Timeout => io::Error::other("the host stopped sending it"),
                RecvTimeoutError::Disconnected => io::Error::other("the host sent more or less than it said"),
            })?;
            f.write_all(&b)?;
            h.update(&b);
            got += b.len() as u64;
        }
        f.flush()?;
        drop(f);
        std::fs::rename(&part, path)?;
        Ok(h.finalize().into())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    r
}

/// Send the file at `path`, then its size and SHA-256.
fn give(id: u32, path: &str, send: &Send) -> io::Result<()> {
    let mut f = std::fs::File::open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::other("not a file"));
    }
    let mut h = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; FILE_CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
        // The host gone: stop, nobody to tell.
        if send(Message::FileData { id, data: buf[..n].to_vec() }).is_err() {
            return Ok(());
        }
    }
    let _ = send(Message::FileDone { id, ok: true, size, sha256: h.finalize().into(), why: String::new() });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn sink() -> (Send, Arc<Mutex<Vec<Message>>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        (Arc::new(move |m| {
            g.lock().unwrap().push(m);
            Ok(())
        }), got)
    }

    fn wait_done(got: &Arc<Mutex<Vec<Message>>>) -> Message {
        for _ in 0..400 {
            if let Some(m) = got.lock().unwrap().iter().find(|m| matches!(m, Message::FileDone { .. })) {
                return m.clone();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no answer");
    }

    #[test]
    fn a_file_comes_in_whole_or_not_at_all() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("a.bin").to_string_lossy().into_owned();
        let data: Vec<u8> = (0..FILE_CHUNK * 2 + 5).map(|i| i as u8).collect();
        let (send, got) = sink();
        let mut f = Files::default();
        f.handle(Message::FilePut { id: 1, size: data.len() as u64, path: path.clone() }, &send);
        for c in data.chunks(FILE_CHUNK) {
            f.handle(Message::FileData { id: 1, data: c.to_vec() }, &send);
        }
        let sha256: [u8; 32] = Sha256::digest(&data).into();
        assert_eq!(wait_done(&got), Message::FileDone { id: 1, ok: true, size: data.len() as u64, sha256, why: String::new() });
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(f.puts.is_empty());
        // Too much: refused, the old file stays.
        let (send, got) = sink();
        f.handle(Message::FilePut { id: 2, size: 2, path: path.clone() }, &send);
        f.handle(Message::FileData { id: 2, data: vec![1, 2, 3] }, &send);
        assert!(matches!(wait_done(&got), Message::FileDone { id: 2, ok: false, .. }));
        // Too little: given up after a while.
        let (send, got) = sink();
        f.handle(Message::FilePut { id: 3, size: 2, path: path.clone() }, &send);
        f.handle(Message::FileData { id: 3, data: vec![1] }, &send);
        assert!(matches!(wait_done(&got), Message::FileDone { id: 3, ok: false, .. }));
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 1, "no parts left");
        // Empty.
        let (send, got) = sink();
        f.handle(Message::FilePut { id: 4, size: 0, path: path.clone() }, &send);
        assert!(matches!(wait_done(&got), Message::FileDone { id: 4, ok: true, size: 0, .. }));
        assert!(std::fs::read(&path).unwrap().is_empty());
    }

    #[test]
    fn a_file_goes_out_in_pieces_with_its_hash() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("b.bin");
        let data: Vec<u8> = (0..FILE_CHUNK + 7).map(|i| (i * 3) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let (send, got) = sink();
        Files::default().handle(Message::FileGet { id: 9, path: path.to_string_lossy().into_owned() }, &send);
        let done = wait_done(&got);
        let sha256: [u8; 32] = Sha256::digest(&data).into();
        assert_eq!(done, Message::FileDone { id: 9, ok: true, size: data.len() as u64, sha256, why: String::new() });
        let pieces: Vec<u8> = got.lock().unwrap().iter().filter_map(|m| if let Message::FileData { id: 9, data } = m { Some(data.clone()) } else { None }).flatten().collect();
        assert_eq!(pieces, data);
        for bad in [t.path().to_path_buf(), t.path().join("nope")] {
            let (send, got) = sink();
            Files::default().handle(Message::FileGet { id: 1, path: bad.to_string_lossy().into_owned() }, &send);
            assert!(matches!(wait_done(&got), Message::FileDone { id: 1, ok: false, .. }), "{bad:?}");
        }
    }
}
