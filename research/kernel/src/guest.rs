//! `guest.*`: the guest agent's work (commands, files, guest tools),
//! while the VM runs. Each answers from a thread of its own: the guest
//! runs on. Host files: absolute paths, or under the lab's exports.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
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
