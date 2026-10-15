//! vTPM backend: swtpm (libtpms) in its own process, sandboxed on macOS.
//!
//! We talk to it over two socketpairs, set up before our own sandbox: the
//! control channel (swtpm's ioctl-style protocol, passed as fd 3) and a data
//! channel for TPM commands (handed over with CMD_SET_DATAFD). swtpm exits
//! when the control channel closes, so it can't outlive the VM.
//!
//! State lives in `<state dir>/tpm`. Suspend keeps volatile state
//! (CMD_STORE_VOLATILE + init without deleting it); snapshots carry the
//! state files so reverting brings the TPM back too.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use vroom_devices::tpm::{TpmBackend, MAX_BUFFER};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const CMD_INIT: u32 = 2;
const CMD_STORE_VOLATILE: u32 = 10;
const CMD_STOP: u32 = 14;
const CMD_SET_DATAFD: u32 = 16;
const INIT_FLAG_DELETE_VOLATILE: u32 = 1;
/// Key generation on a busy host can take a while.
const TIMEOUT: Duration = Duration::from_secs(60);
const STATE_FILES: [&str; 2] = ["tpm2-00.permall", "tpm2-00.volatilestate"];

pub struct Swtpm {
    ctrl: UnixStream,
    data: UnixStream,
    child: Child,
    dir: PathBuf,
}

impl std::fmt::Debug for Swtpm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Swtpm").field("dir", &self.dir).finish()
    }
}

fn find_swtpm() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VROOM_SWTPM") {
        return Some(p.into());
    }
    ["/opt/homebrew/bin/swtpm", "/usr/local/bin/swtpm", "/usr/bin/swtpm"].iter().map(PathBuf::from).find(|p| p.is_file())
}

fn err(what: &str) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Setup(format!("vtpm: {what}: {e}"))
}

#[derive(Serialize, Deserialize)]
struct StateFiles(Vec<(String, Vec<u8>)>);

impl Swtpm {
    /// Start swtpm on `dir`. `fresh`: cold boot (drop volatile state) vs
    /// resuming a suspended VM.
    pub fn start(dir: &Path, fresh: bool) -> Result<Swtpm> {
        let exe = find_swtpm().ok_or_else(|| Error::Setup("tpm = true needs swtpm (brew install swtpm)".into()))?;
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        // swtpm < 0.8 has no "terminate" (exit when the control channel
        // closes); then only our kill on drop stops it.
        let terminate = Command::new(&exe)
            .args(["socket", "--print-capabilities"])
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("ctrl-opt-terminate"));
        let (ctrl, ctrl_child) = UnixStream::pair()?;
        let (data, data_child) = UnixStream::pair()?;
        let args = [
            "socket".to_string(),
            "--tpm2".into(),
            "--tpmstate".into(),
            format!("dir={},mode=0600", dir.display()),
            "--ctrl".into(),
            if terminate { "type=unixio,clientfd=3,terminate" } else { "type=unixio,clientfd=3" }.into(),
            "--log".into(),
            format!("file={},level=1,truncate", dir.join("swtpm.log").display()),
        ];
        let mut cmd = Self::command(&exe, dir, &args)?;
        let fd: RawFd = ctrl_child.as_raw_fd();
        // SAFETY: between fork and exec we only call dup2/fcntl, which are
        // async-signal-safe. fd 3 becomes the control socket: dup2 clears
        // close-on-exec on the copy; if it already is fd 3, clear it by hand.
        unsafe {
            cmd.pre_exec(move || {
                let r = if fd == 3 { libc::fcntl(3, libc::F_SETFD, 0) } else { libc::dup2(fd, 3) };
                if r < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().map_err(err("spawn swtpm"))?;
        drop(ctrl_child);
        for s in [&ctrl, &data] {
            s.set_read_timeout(Some(TIMEOUT))?;
            s.set_write_timeout(Some(TIMEOUT))?;
        }
        let mut t = Swtpm { ctrl, data, child, dir: dir.to_path_buf() };
        vroom_ipc::send_fd_with(&t.ctrl, &CMD_SET_DATAFD.to_be_bytes(), data_child.as_raw_fd())
            .map_err(|e| Error::Setup(format!("vtpm: set data fd: {e}")))?;
        t.result("set data fd")?;
        drop(data_child);
        // A previous instance (suspend, reset of the host process) may
        // still be letting go of the state lock: retry briefly.
        let flags = if fresh { INIT_FLAG_DELETE_VOLATILE } else { 0 };
        let mut tries = 0;
        while let Err(e) = t.init(flags) {
            tries += 1;
            if tries > 25 {
                return Err(e);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        log::info!("vtpm: swtpm {} on {}", t.child.id(), dir.display());
        crate::note_helper(t.child.id());
        Ok(t)
    }

    /// macOS: run under its own Seatbelt profile. Linux: swtpm's seccomp.
    fn command(exe: &Path, dir: &Path, args: &[String]) -> Result<Command> {
        #[cfg(target_os = "macos")]
        {
            let prof = vroom_sandbox::helper_profile(exe, dir).map_err(|e| Error::Setup(format!("vtpm sandbox: {e}")))?;
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(prof).arg(exe).args(args);
            Ok(c)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = dir;
            let mut c = Command::new(exe);
            c.args(args).args(["--seccomp", "action=kill"]);
            Ok(c)
        }
    }

    fn ctrl_cmd(&mut self, cmd: u32, payload: &[u8], what: &str) -> Result<()> {
        let mut m = cmd.to_be_bytes().to_vec();
        m.extend_from_slice(payload);
        self.ctrl.write_all(&m).map_err(err(what))?;
        self.result(what)
    }

    fn result(&mut self, what: &str) -> Result<()> {
        let mut r = [0u8; 4];
        self.ctrl.read_exact(&mut r).map_err(err(what))?;
        match u32::from_be_bytes(r) {
            0 => Ok(()),
            rc => Err(Error::Setup(format!("vtpm: {what}: tpm result {rc:#x}"))),
        }
    }

    fn init(&mut self, flags: u32) -> Result<()> {
        self.ctrl_cmd(CMD_INIT, &flags.to_be_bytes(), "init")
    }

    fn run(&mut self, cmd: &[u8]) -> std::io::Result<Vec<u8>> {
        self.data.write_all(cmd)?;
        let mut hdr = [0u8; 10];
        self.data.read_exact(&mut hdr)?;
        let len = u32::from_be_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;
        if !(10..=MAX_BUFFER).contains(&len) {
            return Err(std::io::Error::other(format!("bad response length {len}")));
        }
        let mut rsp = hdr.to_vec();
        rsp.resize(len, 0);
        self.data.read_exact(&mut rsp[10..])?;
        Ok(rsp)
    }

    fn stop_and_init(&mut self, flags: u32) {
        if let Err(e) = self.ctrl_cmd(CMD_STOP, &[], "stop").and_then(|_| self.init(flags)) {
            log::warn!("{e}");
        }
    }
}

impl TpmBackend for Swtpm {
    fn execute(&mut self, cmd: &[u8]) -> Vec<u8> {
        match self.run(cmd) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("vtpm: command failed: {e}");
                // TPM_RC_FAILURE
                vec![0x80, 0x01, 0, 0, 0, 10, 0, 0, 1, 1]
            }
        }
    }

    fn reset(&mut self) {
        self.stop_and_init(0);
    }

    fn save(&mut self) -> Vec<u8> {
        if let Err(e) = self.ctrl_cmd(CMD_STORE_VOLATILE, &[], "store volatile") {
            log::warn!("{e}");
        }
        let files: Vec<(String, Vec<u8>)> = STATE_FILES
            .iter()
            .filter_map(|n| std::fs::read(self.dir.join(n)).ok().map(|b| (n.to_string(), b)))
            .collect();
        serde_json::to_vec(&StateFiles(files)).unwrap_or_default()
    }

    fn load(&mut self, blob: &[u8]) -> bool {
        let Ok(StateFiles(files)) = serde_json::from_slice::<StateFiles>(blob) else { return false };
        if files.iter().any(|(n, b)| !STATE_FILES.contains(&n.as_str()) || b.len() > 1 << 20) {
            return false;
        }
        if let Err(e) = self.ctrl_cmd(CMD_STOP, &[], "stop") {
            log::warn!("{e}");
            return false;
        }
        for n in STATE_FILES {
            let _ = std::fs::remove_file(self.dir.join(n));
        }
        for (n, b) in &files {
            if std::fs::write(self.dir.join(n), b).is_err() {
                return false;
            }
        }
        self.init(0).is_ok()
    }
}

impl Drop for Swtpm {
    fn drop(&mut self) {
        // swtpm exits when its control channel closes ("terminate"); we
        // may not be allowed to signal it from inside the sandbox.
        let _ = self.ctrl.shutdown(std::net::Shutdown::Both);
        let _ = self.data.shutdown(std::net::Shutdown::Both);
        let _ = self.child.kill();
        for _ in 0..50 {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        log::warn!("vtpm: swtpm {} did not exit", self.child.id());
    }
}
