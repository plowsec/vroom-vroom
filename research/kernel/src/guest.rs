//! `guest.*`: the guest agent's work (commands, files, guest tools),
//! while the VM runs. Each answers from a thread of its own: the guest
//! runs on. Host files: absolute paths, or under the lab's exports.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vroom_vmm::agent::{Agent, ExecEnd, ExecReq, Shell};
use vroom_vmm::{RunState, Vmm};

use crate::agent::Forward;

pub fn run(vm: &Vmm, exports: &Path, f: Forward) {
    let r = &f.request;
    let op = r["op"].as_str().unwrap_or_default();
    let agent = vm.agent().clone();
    let work: Result<Work, String> = if vm.state() != RunState::Running {
        Err("resume the VM first: the guest does the work".into())
    } else {
        match op {
            "guest_exec" => exec(agent, r),
            "guest_upload" => upload(agent, r, exports),
            "guest_download" => download(agent, r, exports),
            "guest_update_tools" => update_tools(agent, vm.config().guest_os() == vroom_config::GuestOs::Windows),
            _ => Err(format!("unknown op {op}")),
        }
    };
    match work {
        Ok(w) => {
            std::thread::spawn(move || {
                let _ = f.reply.send(w());
            });
        }
        Err(e) => {
            let _ = f.reply.send(Err(e));
        }
    }
}

type Work = Box<dyn FnOnce() -> Result<Value, String> + Send>;

/// Remote x64 clients may retrieve bytes; guest file uploads use the same
/// host-path rules as ARM64. Downloads never create host files.
pub fn run_remote(vm: &Vmm, exports: &Path, f: Forward) {
    if f.request["op"] == "guest_download" && f.request.get("host_path").is_some() {
        let _ = f.reply.send(Err("host_path is not available on x64; omit it to receive hex bytes over RPC".into()));
        return;
    }
    run(vm, exports, f);
}

const CHUNK_LIMIT: u64 = 65536;

fn chunk_range(r: &Value) -> Result<(u64, u64), String> {
    let integer = |key: &str, default| match r.get(key) {
        None => Ok(default),
        Some(v) => v.as_u64().ok_or_else(|| format!("{key} must be an unsigned integer")),
    };
    let offset = integer("offset", 0)?;
    let length = integer("length", CHUNK_LIMIT)?;
    if length > CHUNK_LIMIT { return Err("length must be at most 65536".into()); }
    offset.checked_add(length).ok_or("offset + length overflow")?;
    Ok((offset, length))
}

/// The installed agent sends a whole file. Discard all but the requested range,
/// retaining at most 64 KiB; the transport still verifies the whole-file hash.
struct ChunkSink {
    position: u64,
    offset: u64,
    end: u64,
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for ChunkSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.position.checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("file size overflow"))?;
        let start = self.position.max(self.offset);
        let end = next.min(self.end);
        if start < end {
            self.bytes.lock().map_err(|_| io::Error::other("chunk lock poisoned"))?
                .extend_from_slice(&bytes[(start-self.position) as usize..(end-self.position) as usize]);
        }
        self.position = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn exec(agent: Agent, r: &Value) -> Result<Work, String> {
    let req = ExecReq {
        shell: if r["powershell"].as_bool().unwrap_or(false) { Shell::PowerShell } else { Shell::Default },
        command: r["command"].as_str().ok_or("missing command")?.to_string(),
        timeout: Duration::from_secs(r["timeout"].as_u64().unwrap_or(60)),
        max_out: r["max_output"].as_u64().unwrap_or(1 << 20).min(u32::MAX as u64) as u32,
    };
    Ok(Box::new(move || {
        let o = agent.exec(req)?;
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let code = match o.end {
            ExecEnd::Exited(c) => json!(c),
            ExecEnd::TimedOut => Value::Null,
            ExecEnd::Failed => return Err(text(&o.stderr)),
        };
        Ok(json!({"exit_code": code, "timed_out": o.end == ExecEnd::TimedOut, "truncated": o.truncated, "stdout": text(&o.stdout), "stderr": text(&o.stderr)}))
    }))
}

fn host_path(r: &Value, exports: &Path) -> Option<PathBuf> {
    r["host_path"].as_str().map(|p| exports.join(p))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn upload(agent: Agent, r: &Value, exports: &Path) -> Result<Work, String> {
    let guest = r["guest_path"].as_str().ok_or("missing guest_path")?.to_string();
    let host = host_path(r, exports).ok_or("missing host_path")?;
    let f = std::fs::File::open(&host).map_err(|e| format!("{}: {e}", host.display()))?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    Ok(Box::new(move || {
        let o = agent.put(guest.clone(), size, Box::new(f))?;
        Ok(json!({"guest_path": guest, "size": o.size, "sha256": hex(&o.sha256)}))
    }))
}

fn download(agent: Agent, r: &Value, exports: &Path) -> Result<Work, String> {
    let guest = r["guest_path"].as_str().ok_or("missing guest_path")?.to_string();
    if r.get("host_path").is_none() {
        let (offset, length) = chunk_range(r)?;
        return Ok(Box::new(move || {
            let bytes = Arc::new(Mutex::new(Vec::with_capacity(length as usize)));
            let sink = ChunkSink { position: 0, offset, end: offset + length, bytes: bytes.clone() };
            let out = agent.get(guest.clone(), Box::new(sink))?;
            let bytes = bytes.lock().map_err(|e| e.to_string())?;
            Ok(json!({"guest_path": guest, "offset": offset, "length": bytes.len(),
                "file_size": out.size, "eof": offset.saturating_add(bytes.len() as u64) >= out.size,
                "hex": hex(&bytes), "sha256": hex(&out.sha256)}))
        }));
    }
    if r.get("offset").is_some() || r.get("length").is_some() {
        return Err("offset/length cannot be combined with host_path".into());
    }
    let host = match host_path(r, exports) {
        Some(p) => p,
        None => {
            let name = guest.rsplit(['/', '\\']).next().filter(|n| !n.is_empty()).unwrap_or("file");
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            exports.join(format!("guest-{stamp}-{name}"))
        }
    };
    if let Some(d) = host.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&host).map_err(|e| format!("{}: {e}", host.display()))?;
    Ok(Box::new(move || match agent.get(guest, Box::new(f)) {
        Ok(o) => Ok(json!({"path": host, "size": o.size, "sha256": hex(&o.sha256)})),
        Err(e) => {
            let _ = std::fs::remove_file(&host);
            Err(e)
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_chunks_across_writes_and_eof() {
        for (offset, length, expected) in [(2, 4, b"cdef".as_slice()), (6, 9, b"gh"), (8, 2, b""), (99, 2, b""), (0, 0, b"")] {
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let mut sink = ChunkSink { position: 0, offset, end: offset + length, bytes: bytes.clone() };
            for part in [b"abc".as_slice(), b"d", b"efgh"] { sink.write_all(part).unwrap(); }
            assert_eq!(&*bytes.lock().unwrap(), expected);
            assert_eq!(sink.position, 8);
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut sink = ChunkSink { position: 0, offset: 1, end: 65537, bytes: bytes.clone() };
        sink.write_all(&vec![7; 1 << 20]).unwrap();
        assert_eq!(bytes.lock().unwrap().len(), 65536);
    }

    #[test]
    fn validate_chunk_ranges() {
        assert_eq!(chunk_range(&json!({})).unwrap(), (0, 65536));
        assert_eq!(chunk_range(&json!({"offset":10,"length":0})).unwrap(), (10, 0));
        for r in [json!({"length":65537}), json!({"offset":-1}), json!({"length":"2"}), json!({"offset":null}), json!({"offset":u64::MAX,"length":1})] {
            assert!(chunk_range(&r).is_err());
        }
    }

    #[test]
    fn host_paths_are_absolute_or_relative_to_exports() {
        let exports = Path::new("/var/lib/vroom/exports");
        assert_eq!(
            host_path(&json!({"host_path": "/tmp/file.bin"}), exports).unwrap(),
            PathBuf::from("/tmp/file.bin")
        );
        assert_eq!(
            host_path(&json!({"host_path": "file.bin"}), exports).unwrap(),
            exports.join("file.bin")
        );
    }
}

fn update_tools(agent: Agent, windows: bool) -> Result<Work, String> {
    let os = if windows { vroom_tools::Os::Windows } else { vroom_tools::Os::Linux };
    let dir = vroom_tools::os_dir(os).ok_or("no guest tools next to vroom-lab: scripts/make-guest-tools.sh, or set VROOM_GUEST_TOOLS")?;
    let v = vroom_tools::version(os).ok_or("no version in the agent")?.to_string();
    Ok(Box::new(move || {
        let (mut a, b) = std::os::unix::net::UnixStream::pair().map_err(|e| e.to_string())?;
        let v2 = v.clone();
        // Read to its end by the update: closed once written.
        std::thread::spawn(move || vroom_tools::bundle::write(&mut a, 0, &v2, &dir));
        agent.update(b.into())?;
        Ok(json!({"installing": v, "note": "the agent restarts: vm.status shows its version"}))
    }))
}
