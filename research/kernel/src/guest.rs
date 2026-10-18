//! `guest.exec`: a command the guest agent runs, while the VM runs.

use serde_json::{json, Value};
use std::time::Duration;
use vroom_vmm::agent::{Agent, ExecEnd, ExecReq, Shell};

use crate::agent::Forward;

/// Answer `f` from a thread of its own once the command ends.
pub fn exec(agent: Agent, running: bool, f: Forward) {
    let r = &f.request;
    let req = if !running {
        Err("resume the VM first: the guest runs the command".to_string())
    } else {
        r["command"].as_str().ok_or_else(|| "missing command".to_string()).map(|command| ExecReq {
            shell: if r["powershell"].as_bool().unwrap_or(false) { Shell::PowerShell } else { Shell::Default },
            command: command.to_string(),
            timeout: Duration::from_secs(r["timeout"].as_u64().unwrap_or(60)),
            max_out: r["max_output"].as_u64().unwrap_or(1 << 20).min(u32::MAX as u64) as u32,
        })
    };
    let req = match req {
        Ok(q) => q,
        Err(e) => {
            let _ = f.reply.send(Err(e));
            return;
        }
    };
    std::thread::spawn(move || {
        let r = agent.exec(req).and_then(|o| {
            let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            let code = match o.end {
                ExecEnd::Exited(c) => json!(c),
                ExecEnd::TimedOut => Value::Null,
                ExecEnd::Failed => return Err(text(&o.stderr)),
            };
            Ok(json!({"exit_code": code, "timed_out": o.end == ExecEnd::TimedOut, "truncated": o.truncated, "stdout": text(&o.stdout), "stderr": text(&o.stderr)}))
        });
        let _ = f.reply.send(r);
    });
}
