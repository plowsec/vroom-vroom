//! Commands the host runs in the guest (`Exec`): as the agent's user
//! (Windows: SYSTEM; Linux: root), no input, killed with what they
//! started at the timeout, output capped. Each on a thread of its own:
//! the agent's other work goes on meanwhile.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vroom_proto::{ExecEnd, Message, Shell};

/// How long output may still come once the command ended (something it
/// left running holds the pipes).
const DRAIN: Duration = Duration::from_secs(2);

/// Run `m` (an `Exec`) on a thread; `done` gets its `ExecDone`.
pub fn spawn(m: Message, done: impl FnOnce(Message) + Send + 'static) {
    let Message::Exec { id, shell, timeout_ms, max_out, command } = m else { return };
    log::info!("exec {id}: {command}");
    std::thread::spawn(move || done(run(id, shell, &command, Duration::from_millis(timeout_ms as u64), max_out as usize)));
}

pub fn run(id: u32, shell: Shell, line: &str, timeout: Duration, max: usize) -> Message {
    let mut c = command(shell, line);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            let why = format!("can't start {}: {e}", c.get_program().to_string_lossy());
            return Message::ExecDone { id, end: ExecEnd::Failed, truncated: false, stdout: Vec::new(), stderr: why.into_bytes() };
        }
    };
    let (tx, rx) = mpsc::channel();
    let out = Arc::new(Mutex::new((Vec::new(), false)));
    let err = Arc::new(Mutex::new((Vec::new(), false)));
    if let Some(p) = child.stdout.take() {
        collect(p, max, out.clone(), tx.clone());
    }
    if let Some(p) = child.stderr.take() {
        collect(p, max, err.clone(), tx);
    }
    let deadline = Instant::now() + timeout;
    let mut nap = Duration::from_millis(2);
    let end = loop {
        match child.try_wait() {
            Ok(Some(s)) => break ExecEnd::Exited(code(s)),
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(&mut child);
                let _ = child.wait();
                break ExecEnd::TimedOut;
            }
            Ok(None) => {
                std::thread::sleep(nap.min(deadline.saturating_duration_since(Instant::now())));
                nap = (nap * 2).min(Duration::from_millis(50));
            }
            Err(e) => {
                kill_tree(&mut child);
                let _ = child.wait();
                err.lock().unwrap_or_else(|e| e.into_inner()).0.extend_from_slice(format!("waiting for it: {e}").as_bytes());
                break ExecEnd::Failed;
            }
        }
    };
    // Both pipes closed, or what came until then.
    let until = Instant::now() + DRAIN;
    for _ in 0..2 {
        if rx.recv_timeout(until.saturating_duration_since(Instant::now())).is_err() {
            break;
        }
    }
    let take = |b: &Arc<Mutex<(Vec<u8>, bool)>>| std::mem::take(&mut *b.lock().unwrap_or_else(|e| e.into_inner()));
    let ((stdout, t1), (stderr, t2)) = (take(&out), take(&err));
    log::info!("exec {id}: {end:?}");
    Message::ExecDone { id, end, truncated: t1 || t2, stdout, stderr }
}

/// Read `r` to its end into `into` (at most `max` bytes kept; the bool
/// says some were dropped), then tell `done`.
fn collect(mut r: impl Read + Send + 'static, max: usize, into: Arc<Mutex<(Vec<u8>, bool)>>, done: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 << 10];
        while let Ok(n @ 1..) = r.read(&mut buf) {
            let mut g = into.lock().unwrap_or_else(|e| e.into_inner());
            let keep = n.min(max - g.0.len());
            g.0.extend_from_slice(&buf[..keep]);
            g.1 |= keep < n;
        }
        let _ = done.send(());
    });
}

#[cfg(unix)]
fn code(s: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    // Killed by a signal: as shells say it.
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn code(s: std::process::ExitStatus) -> i32 {
    s.code().unwrap_or(-1)
}

#[cfg(unix)]
fn command(shell: Shell, line: &str) -> Command {
    let mut c = match shell {
        Shell::Default => Command::new("/bin/sh"),
        Shell::PowerShell => Command::new("pwsh"),
    };
    match shell {
        Shell::Default => c.arg("-c").arg(line),
        Shell::PowerShell => c.args(["-NoProfile", "-NonInteractive", "-Command", line]),
    };
    // A group of its own: the timeout kills all of it.
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    c
}

#[cfg(windows)]
fn command(shell: Shell, line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let sys = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into())).join("System32");
    let mut c = match shell {
        Shell::Default => {
            let mut c = Command::new(sys.join("cmd.exe"));
            // /s: cmd takes the outer quotes off and the line as it is.
            c.raw_arg(format!("/d /s /c \"{line}\""));
            c
        }
        Shell::PowerShell => {
            let mut c = Command::new(sys.join("WindowsPowerShell\\v1.0\\powershell.exe"));
            // No progress records ("Preparing modules for first use"),
            // and text, not CLIXML, on stderr.
            let script = format!("$ProgressPreference = 'SilentlyContinue'\ntry {{ [Console]::OutputEncoding = [Text.Encoding]::UTF8 }} catch {{}}\n{line}");
            c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-OutputFormat", "Text", "-EncodedCommand", &utf16_base64(&script)]);
            c
        }
    };
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_tree(child: &mut Child) {
    // SAFETY: plain call; the group is the child's own (process_group(0)).
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    let _ = child.kill();
}

#[cfg(windows)]
fn kill_tree(child: &mut Child) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).creation_flags(0x0800_0000).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = child.kill();
}

/// PowerShell's `-EncodedCommand`: UTF-16LE, base64.
#[cfg_attr(not(windows), allow(dead_code))]
fn utf16_base64(s: &str) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for ch in b.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= ch.len() { ABC[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_gets_utf16_base64() {
        assert_eq!(utf16_base64("ab"), "YQBiAA==");
        assert_eq!(utf16_base64("dir"), "ZABpAHIA");
        assert_eq!(utf16_base64("é"), "6QA=");
    }

    #[cfg(unix)]
    fn sh(line: &str, timeout_ms: u64, max: usize) -> (ExecEnd, bool, String, String) {
        let Message::ExecDone { id: 7, end, truncated, stdout, stderr } = run(7, Shell::Default, line, Duration::from_millis(timeout_ms), max) else { panic!() };
        (end, truncated, String::from_utf8(stdout).unwrap(), String::from_utf8(stderr).unwrap())
    }

    #[cfg(unix)]
    #[test]
    fn output_and_exit_code_come_back() {
        assert_eq!(sh("echo out; echo err >&2; exit 3", 10_000, 1024), (ExecEnd::Exited(3), false, "out\n".into(), "err\n".into()));
        assert_eq!(sh("kill -9 $$", 10_000, 1024).0, ExecEnd::Exited(137));
    }

    #[cfg(unix)]
    #[test]
    fn output_is_capped() {
        assert_eq!(sh("printf 1234567890; printf ab >&2", 10_000, 4), (ExecEnd::Exited(0), true, "1234".into(), "ab".into()));
        let (_, truncated, out, _) = sh("head -c 1000000 /dev/zero", 10_000, 0);
        assert!(truncated && out.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_timeout_kills_what_it_started() {
        let t = Instant::now();
        let (end, _, out, _) = sh("echo started; sleep 30 & sleep 30", 300, 1024);
        assert_eq!((end, out.as_str()), (ExecEnd::TimedOut, "started\n"));
        assert!(t.elapsed() < DRAIN, "{:?}: the background sleep held the pipe", t.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn runs_on_its_own_thread() {
        let (tx, rx) = mpsc::channel();
        let m = Message::Exec { id: 3, shell: Shell::Default, timeout_ms: 10_000, max_out: 64, command: "echo hi".into() };
        spawn(m, move |r| tx.send(r).unwrap());
        let r = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(r, Message::ExecDone { id: 3, end: ExecEnd::Exited(0), truncated: false, stdout: b"hi\n".to_vec(), stderr: Vec::new() });
    }
}
