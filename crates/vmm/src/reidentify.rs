//! A clone's guest still has its original's hostname, machine ID and SSH
//! host keys (Windows: computer name and machine GUID): on a network both
//! would look like one machine, and DHCP may hand them one address. The
//! app's Clone leaves the name the clone's guest should take in its state
//! dir; once the guest agent is there, it takes it and the guest restarts
//! once. Tried until the agent has run it; then the note goes, whatever
//! the outcome, so a guest that refuses doesn't restart at every boot.

use std::path::{Path, PathBuf};
use std::time::Duration;

use vroom_config::GuestOs;
use vroom_proto::features;

use crate::agent::{Agent, AgentCmd, ExecEnd, ExecReq, Shell};

/// In the state dir: the hostname, one line.
pub const FILE: &str = "reidentify";

const POLL: Duration = if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(2) };

/// A hostname every guest takes: 1-63 of `[a-z0-9-]`, no `-` at either
/// end (Windows' 15-character limit is the clone's to keep).
pub fn hostname_ok(s: &str) -> bool {
    (1..=63).contains(&s.len()) && !s.starts_with('-') && !s.ends_with('-') && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Start on it if this VM's state dir asks for it.
pub fn start(state_dir: &Path, os: GuestOs, agent: Agent) {
    let note = state_dir.join(FILE);
    // The sandboxed VMM can plant one: no links, no FIFOs, one line.
    let Ok(text) = crate::state::hostile::read_small(&note, 256) else { return };
    let name = String::from_utf8_lossy(&text).trim().to_string();
    if !hostname_ok(&name) {
        log::warn!("{}: {name:?} isn't a hostname; ignored", note.display());
        let _ = std::fs::remove_file(&note);
        return;
    }
    let windows = os == GuestOs::Windows;
    let r = std::thread::Builder::new().name("reidentify".into()).spawn(move || run(&note, &name, windows, &agent));
    if let Err(e) = r {
        log::warn!("guest identity: {e}");
    }
}

fn run(note: &PathBuf, name: &str, windows: bool, agent: &Agent) {
    let req = ExecReq {
        shell: if windows { Shell::PowerShell } else { Shell::Default },
        command: if windows { windows_script(name) } else { unix_script(name) },
        timeout: Duration::from_secs(120),
        max_out: 16 << 10,
    };
    loop {
        std::thread::sleep(POLL);
        let s = agent.status();
        if !s.connected || s.features & features::EXEC == 0 {
            continue;
        }
        // Not run (the agent restarted meanwhile, say): again next time.
        let Ok(out) = agent.exec(req.clone()) else { continue };
        let _ = std::fs::remove_file(note);
        if out.end != ExecEnd::Exited(0) {
            log::warn!("the guest kept its hostname and machine ID ({:?}): {}", out.end, String::from_utf8_lossy(&out.stderr).trim());
            return;
        }
        log::info!("the guest is now {name}, with a new machine ID; restarting it once");
        if s.features & features::SHUTDOWN != 0 {
            agent.send(AgentCmd::Shutdown { reboot: true });
        }
        return;
    }
}

/// `sh` as root. The agent's own service may not write `/etc`, so it goes
/// through a transient systemd unit where there is systemd.
fn unix_script(name: &str) -> String {
    let s = format!(
        r#"set -e
new={name}
old=$(hostname)
if ! hostnamectl set-hostname "$new" 2>/dev/null; then echo "$new" > /etc/hostname; hostname "$new"; fi
if grep -q "^127\.0\.1\.1[[:space:]]" /etc/hosts; then sed -i "s/^127\.0\.1\.1[[:space:]].*/127.0.1.1 $new/" /etc/hosts; else echo "127.0.1.1 $new" >> /etc/hosts; fi
if [ -e /etc/machine-id ]; then
  rm -f /etc/machine-id
  systemd-machine-id-setup >/dev/null 2>&1 || tr -d - < /proc/sys/kernel/random/uuid > /etc/machine-id
  if [ -f /var/lib/dbus/machine-id ] && [ ! -L /var/lib/dbus/machine-id ]; then cp /etc/machine-id /var/lib/dbus/machine-id; fi
fi
if ls /etc/ssh/ssh_host_*_key >/dev/null 2>&1; then rm -f /etc/ssh/ssh_host_*; ssh-keygen -A >/dev/null; fi
echo "$old -> $new""#
    );
    format!("S='{s}'; if [ -d /run/systemd/system ] && command -v systemd-run >/dev/null 2>&1; then exec systemd-run --wait --pipe --quiet --collect /bin/sh -c \"$S\"; else exec /bin/sh -c \"$S\"; fi")
}

/// PowerShell as SYSTEM. The SID stays: only Sysprep changes it.
fn windows_script(name: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         if ($env:COMPUTERNAME -ne '{name}') {{ Rename-Computer -NewName '{name}' -Force -WarningAction SilentlyContinue }}\n\
         Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Microsoft\\Cryptography' -Name MachineGuid -Value ([guid]::NewGuid().ToString())"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use vroom_proto::{Decoder, Message};

    #[test]
    fn hostnames() {
        for ok in ["beta-lab", "a", "web2", &"a".repeat(63)] {
            assert!(hostname_ok(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a.b", "a_b", "a b", "a'b", &"a".repeat(64)] {
            assert!(!hostname_ok(bad), "{bad}");
        }
        // No quote in a name can end the script's quoting.
        assert!(unix_script("beta-lab").contains("new=beta-lab\n"));
        assert!(windows_script("beta-lab").contains("-NewName 'beta-lab'"));
    }

    fn next(guest: &mut UnixStream, d: &mut Decoder) -> Message {
        let mut buf = [0u8; 65536];
        loop {
            if let Ok(Some(m)) = d.next() {
                return m;
            }
            let n = guest.read(&mut buf).unwrap();
            assert!(n > 0, "closed");
            d.push(&buf[..n]);
        }
    }

    #[test]
    fn the_guest_takes_its_name_once_then_restarts() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(FILE), "beta-lab\n").unwrap();
        let (host, mut guest) = UnixStream::pair().unwrap();
        let a = crate::agent::spawn(host, Default::default(), None, Default::default()).unwrap();
        start(t.path(), GuestOs::Ubuntu, a.clone());
        guest.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let f = features::EXEC | features::SHUTDOWN;
        guest.write_all(&Message::Hello { features: f, agent: "t".into() }.encode()).unwrap();
        let mut d = Decoder::new(1 << 20);
        let (id, command) = loop {
            if let Message::Exec { id, command, shell: Shell::Default, .. } = next(&mut guest, &mut d) {
                break (id, command);
            }
        };
        assert!(command.contains("new=beta-lab") && command.contains("systemd-run"), "{command}");
        assert!(t.path().join(FILE).exists(), "kept until it ran");
        let done = Message::ExecDone { id, end: ExecEnd::Exited(0), truncated: false, stdout: b"old-name -> beta-lab".to_vec(), stderr: vec![] };
        guest.write_all(&done.encode()).unwrap();
        loop {
            if next(&mut guest, &mut d) == (Message::Shutdown { reboot: true }) {
                break;
            }
        }
        assert!(!t.path().join(FILE).exists());
        a.send(AgentCmd::Stop);
    }

    #[test]
    fn a_name_that_isnt_one_is_dropped() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(FILE), "rm -rf /").unwrap();
        let (host, _guest) = UnixStream::pair().unwrap();
        let a = crate::agent::spawn(host, Default::default(), None, Default::default()).unwrap();
        start(t.path(), GuestOs::Ubuntu, a.clone());
        assert!(!t.path().join(FILE).exists());
        a.send(AgentCmd::Stop);
    }

    #[test]
    fn a_planted_fifo_does_not_hang_the_start() {
        let t = tempfile::tempdir().unwrap();
        let c = std::ffi::CString::new(t.path().join(FILE).as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (host, _guest) = UnixStream::pair().unwrap();
        let a = crate::agent::spawn(host, Default::default(), None, Default::default()).unwrap();
        start(t.path(), GuestOs::Ubuntu, a.clone());
        a.send(AgentCmd::Stop);
    }
}
