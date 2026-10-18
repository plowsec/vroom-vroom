//! The VM's control socket (`vroom guest exec`): one request per
//! connection, as agent frames. `Exec` in (its id unused), `ExecDone`
//! out; one refused here comes back `Failed`, stderr says why.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use vroom_proto::{Decoder, ExecEnd, Message, EXEC_CMD_MAX};

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

fn one(mut s: UnixStream, agent: &Agent) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut d = Decoder::new(EXEC_CMD_MAX + 64);
    let mut buf = vec![0u8; 16 << 10];
    let m = loop {
        if let Some(m) = d.next().map_err(std::io::Error::other)? {
            break m;
        }
        let n = s.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        d.push(&buf[..n]);
    };
    let Message::Exec { shell, timeout_ms, max_out, command, .. } = m else {
        return Err(std::io::Error::other("not a request"));
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
    use vroom_proto::{features, Shell};

    #[test]
    fn a_request_reaches_the_agent_and_its_answer_comes_back() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let agent = spawn(host, Default::default(), None, Screen::default()).unwrap();
        guest.write_all(&Message::Hello { features: features::EXEC, agent: "t".into() }.encode()).unwrap();
        // The guest agent: answers what it is asked.
        std::thread::spawn(move || {
            let mut d = Decoder::new(1 << 20);
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = guest.read(&mut buf) {
                d.push(&buf[..n]);
                while let Ok(Some(m)) = d.next() {
                    if let Message::Exec { id, command, .. } = m {
                        let done = Message::ExecDone { id, end: ExecEnd::Exited(2), truncated: true, stdout: command.into_bytes(), stderr: Vec::new() };
                        guest.write_all(&done.encode()).unwrap();
                    }
                }
            }
        });
        let ask = |m: Message| {
            let (mut cli, vmm) = UnixStream::pair().unwrap();
            let a = agent.clone();
            let t = std::thread::spawn(move || one(vmm, &a));
            cli.write_all(&m.encode()).unwrap();
            t.join().unwrap().unwrap();
            let mut b = Vec::new();
            cli.read_to_end(&mut b).unwrap();
            let mut d = Decoder::new(1 << 20);
            d.push(&b);
            d.next().unwrap().unwrap()
        };
        let exec = |command: &str| Message::Exec { id: 9, shell: Shell::Default, timeout_ms: 5000, max_out: 64, command: command.into() };
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(ask(exec("netsh")), Message::ExecDone { id: 0, end: ExecEnd::Exited(2), truncated: true, stdout: b"netsh".to_vec(), stderr: Vec::new() });
        agent.send(crate::agent::AgentCmd::Stop);
    }
}
