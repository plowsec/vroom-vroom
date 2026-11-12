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
use std::os::unix::fs::OpenOptionsExt;
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
const MAX_STATE_FILE: usize = 1 << 20;

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

impl StateFiles {
    fn validate(&self) -> vroom_snapshot::Result<()> {
        // This backend always runs TPM 2. A successful init has permanent
        // state; STORE_VOLATILE must then produce volatile state. TPM 1.2's
        // optional savestate blob is not part of this backend's contract.
        if self.0.len() != STATE_FILES.len()
            || STATE_FILES.iter().any(|required| self.0.iter().filter(|(n, _)| n == required).count() != 1)
        {
            return Err(vroom_snapshot::Error::Invalid("vtpm: missing, duplicate or unexpected state file".into()));
        }
        for (name, bytes) in &self.0 {
            if bytes.is_empty() || bytes.len() > MAX_STATE_FILE {
                return Err(vroom_snapshot::Error::Invalid(format!("vtpm: {name}: empty or oversized state")));
            }
        }
        Ok(())
    }

    fn decode(blob: &[u8]) -> vroom_snapshot::Result<Self> {
        // Do not include serde's error text: malformed input may contain state
        // bytes in a position where a field/type diagnostic would print them.
        let state: Self = serde_json::from_slice(blob)
            .map_err(|_| vroom_snapshot::Error::Invalid("vtpm: malformed state archive".into()))?;
        state.validate()?;
        Ok(state)
    }
}

fn read_state_file(dir: &Path, name: &str) -> std::io::Result<Vec<u8>> {
    // Bound the read and reject links/special files without potentially blocking
    // on a FIFO. The TPM is serialized behind the device lock during capture.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(name))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_STATE_FILE as u64 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "expected bounded nonempty regular state file"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_STATE_FILE as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "state file length changed while reading"));
    }
    Ok(bytes)
}

/// Export first, then capture both required files. No STOP, INIT or state-file
/// replacement occurs here: an error leaves the live backend available to the
/// caller's existing snapshot/suspend recovery path.
fn capture_state(
    export: impl FnOnce() -> Result<()>,
    mut read: impl FnMut(&str) -> std::io::Result<Vec<u8>>,
) -> vroom_snapshot::Result<Vec<u8>> {
    export().map_err(|e| vroom_snapshot::Error::Invalid(format!("vtpm state export: {e}")))?;
    let mut files = Vec::with_capacity(STATE_FILES.len());
    for name in STATE_FILES {
        let bytes = read(name).map_err(|e| vroom_snapshot::Error::Io(std::io::Error::new(
            e.kind(), format!("vtpm: read required {name}: {}", e.kind()),
        )))?;
        files.push((name.to_string(), bytes));
    }
    let state = StateFiles(files);
    state.validate()?;
    vroom_snapshot::encode(&state)
}

/// Files swtpm opens in its state dir (following links; on Linux it has
/// no sandbox of its own).
const SWTPM_FILES: [&str; 4] = [STATE_FILES[0], STATE_FILES[1], "swtpm.log", ".lock"];

/// `<state>/tpm` as swtpm needs it: a real directory (not a link the
/// sandboxed VMM left to a folder elsewhere), mode 0700, and nothing but
/// plain files where swtpm opens its files.
fn clean_dir(dir: &Path) -> std::io::Result<()> {
    crate::state::hostile::ensure_dir(dir, 0o700)?;
    crate::state::hostile::set_dir_mode(dir, 0o700)?;
    for n in SWTPM_FILES {
        let p = dir.join(n);
        if std::fs::symlink_metadata(&p).is_ok() && !crate::state::hostile::plain_file(&p) {
            log::warn!("vtpm: {} is not a plain file; removed", p.display());
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

impl Swtpm {
    /// Start swtpm on `dir`. `fresh`: cold boot (drop volatile state) vs
    /// resuming a suspended VM.
    pub fn start(dir: &Path, fresh: bool) -> Result<Swtpm> {
        let exe = find_swtpm().ok_or_else(|| Error::Setup("tpm = true needs swtpm (brew install swtpm)".into()))?;
        clean_dir(dir)?;
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

    /// macOS: run under its own Seatbelt profile. Linux: no_new_privs,
    /// Landlock (its state dir read-write, system libraries read-only),
    /// and swtpm's own seccomp.
    fn command(exe: &Path, dir: &Path, args: &[String]) -> Result<Command> {
        #[cfg(target_os = "macos")]
        {
            let prof = vroom_sandbox::helper_profile(exe, dir).map_err(|e| Error::Setup(format!("vtpm sandbox: {e}")))?;
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(prof).arg(exe).args(args);
            Ok(c)
        }
        #[cfg(target_os = "linux")]
        {
            // Built here: we are multi-threaded, the child may only make
            // async-signal-safe calls.
            let sb = vroom_sandbox::ExecSandbox::new(exe, dir).map_err(|e| Error::Setup(format!("vtpm sandbox: {e}")))?;
            let mut c = Command::new(exe);
            c.args(args).args(["--seccomp", "action=kill"]);
            // SAFETY: `enter` is two raw syscalls (prctl, landlock_restrict_self).
            unsafe {
                c.pre_exec(move || sb.enter());
            }
            Ok(c)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        let dir = self.dir.clone();
        capture_state(
            || self.ctrl_cmd(CMD_STORE_VOLATILE, &[], "store volatile"),
            |name| read_state_file(&dir, name),
        )
    }

    fn load(&mut self, blob: &[u8]) -> bool {
        // Reject incomplete older captures before stopping the current TPM or
        // replacing any of its files. Complete existing archives are unchanged.
        let Ok(StateFiles(files)) = StateFiles::decode(blob) else { return false };
        if let Err(e) = self.ctrl_cmd(CMD_STOP, &[], "stop") {
            log::warn!("{e}");
            return false;
        }
        for n in STATE_FILES {
            let _ = std::fs::remove_file(self.dir.join(n));
        }
        for (n, b) in &files {
            // New files only (the old ones are gone): never through a link.
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
            if o.open(self.dir.join(n)).and_then(|mut f| f.write_all(b)).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::ErrorKind;

    fn write_state(dir: &Path) {
        std::fs::write(dir.join(STATE_FILES[0]), b"test permanent state").unwrap();
        std::fs::write(dir.join(STATE_FILES[1]), b"test volatile state").unwrap();
    }

    #[test]
    fn planted_links_in_the_tpm_dir_go() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim");
        std::fs::write(&victim, b"user file").unwrap();
        let dir = temp.path().join("tpm");
        std::os::unix::fs::symlink(temp.path(), &dir).unwrap();
        assert!(clean_dir(&dir).is_err(), "a link to a folder elsewhere");
        std::fs::remove_file(&dir).unwrap();
        clean_dir(&dir).unwrap();
        write_state(&dir);
        std::os::unix::fs::symlink(&victim, dir.join("swtpm.log")).unwrap();
        std::fs::remove_file(dir.join(STATE_FILES[1])).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join(STATE_FILES[1])).unwrap();
        clean_dir(&dir).unwrap();
        assert!(std::fs::symlink_metadata(dir.join("swtpm.log")).is_err());
        assert!(std::fs::symlink_metadata(dir.join(STATE_FILES[1])).is_err());
        assert_eq!(std::fs::read(dir.join(STATE_FILES[0])).unwrap(), b"test permanent state", "plain files stay");
        assert_eq!(std::fs::read(&victim).unwrap(), b"user file");
    }

    #[test]
    fn successful_export_creates_required_volatile_state_and_captures_both_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(STATE_FILES[0]), b"test permanent state").unwrap();
        // Volatile state may be absent after INIT, but not after its successful
        // export. No TPM 1.2 savestate file is needed or read.
        let exported = Cell::new(false);
        let blob = capture_state(
            || {
                std::fs::write(temp.path().join(STATE_FILES[1]), b"test volatile state")?;
                exported.set(true);
                Ok(())
            },
            |name| {
                assert!(exported.get());
                read_state_file(temp.path(), name)
            },
        ).unwrap();
        let state = StateFiles::decode(&blob).unwrap();
        assert_eq!(state.0, vec![
            (STATE_FILES[0].into(), b"test permanent state".to_vec()),
            (STATE_FILES[1].into(), b"test volatile state".to_vec()),
        ]);
        assert_eq!(std::fs::read(temp.path().join(STATE_FILES[0])).unwrap(), b"test permanent state");
    }

    #[test]
    fn failed_export_does_not_read_or_return_stale_files() {
        let reads = Cell::new(0);
        let result = capture_state(
            || Err(Error::Setup("store volatile: injected failure".into())),
            |_| { reads.set(reads.get() + 1); Ok(b"stale state".to_vec()) },
        );
        assert!(result.unwrap_err().to_string().contains("store volatile: injected failure"));
        assert_eq!(reads.get(), 0);
    }

    #[test]
    fn unreadable_required_state_is_not_a_partial_success_or_secret_diagnostic() {
        for failed in STATE_FILES {
            let error = capture_state(|| Ok(()), |name| {
                if name == failed {
                    Err(std::io::Error::new(ErrorKind::PermissionDenied, "secret state bytes must not appear"))
                } else {
                    Ok(vec![1, 2, 3])
                }
            }).unwrap_err();
            let text = error.to_string();
            assert!(text.contains(failed));
            assert!(!text.contains("secret state bytes"));
            assert!(matches!(error, vroom_snapshot::Error::Io(e) if e.kind() == ErrorKind::PermissionDenied));
        }
    }

    #[test]
    fn disappearing_state_aborts_capture_and_a_later_capture_can_retry() {
        let temp = tempfile::tempdir().unwrap();
        write_state(temp.path());
        let result = capture_state(|| Ok(()), |name| {
            let bytes = read_state_file(temp.path(), name)?;
            if name == STATE_FILES[0] {
                std::fs::remove_file(temp.path().join(STATE_FILES[1]))?;
            }
            Ok(bytes)
        });
        assert!(matches!(result, Err(vroom_snapshot::Error::Io(e)) if e.kind() == ErrorKind::NotFound));
        assert_eq!(std::fs::read(temp.path().join(STATE_FILES[0])).unwrap(), b"test permanent state");
        let retried = capture_state(
            || { std::fs::write(temp.path().join(STATE_FILES[1]), b"new volatile state")?; Ok(()) },
            |name| read_state_file(temp.path(), name),
        ).unwrap();
        assert_eq!(StateFiles::decode(&retried).unwrap().0[1].1, b"new volatile state");
    }

    #[test]
    fn missing_required_state_after_successful_export_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        for missing in STATE_FILES {
            write_state(temp.path());
            std::fs::remove_file(temp.path().join(missing)).unwrap();
            let error = capture_state(|| Ok(()), |name| read_state_file(temp.path(), name)).unwrap_err();
            assert!(error.to_string().contains(missing));
        }
    }

    #[test]
    fn capture_rejects_empty_and_oversized_required_state() {
        for size in [0, MAX_STATE_FILE + 1] {
            let result = capture_state(|| Ok(()), |_| Ok(vec![0; size]));
            assert!(result.is_err());
        }
    }

    #[test]
    fn state_reader_rejects_empty_large_nonregular_and_symlink_files() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(STATE_FILES[0]);
        let f = std::fs::File::create(&path).unwrap();
        assert!(read_state_file(temp.path(), STATE_FILES[0]).is_err());
        f.set_len(MAX_STATE_FILE as u64 + 1).unwrap();
        assert!(read_state_file(temp.path(), STATE_FILES[0]).is_err());
        drop(f);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(read_state_file(temp.path(), STATE_FILES[0]).is_err());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(temp.path().join("other"), b"test state").unwrap();
        symlink("other", &path).unwrap();
        assert!(read_state_file(temp.path(), STATE_FILES[0]).is_err());
    }

    #[test]
    fn restore_preflight_requires_exact_nonempty_state_without_echoing_input() {
        let valid = || vec![(STATE_FILES[0].to_string(), vec![1]), (STATE_FILES[1].to_string(), vec![2])];
        assert!(StateFiles::decode(&serde_json::to_vec(&StateFiles(valid())).unwrap()).is_ok());
        let mut reversed = valid();
        reversed.reverse();
        assert!(StateFiles::decode(&serde_json::to_vec(&StateFiles(reversed)).unwrap()).is_ok());
        for files in [
            vec![], vec![valid()[0].clone()],
            vec![valid()[0].clone(), valid()[0].clone()],
            vec![("secret unknown name".into(), vec![1]), valid()[1].clone()],
            vec![(STATE_FILES[0].into(), vec![]), valid()[1].clone()],
            vec![(STATE_FILES[0].into(), vec![0; MAX_STATE_FILE + 1]), valid()[1].clone()],
        ] {
            let error = StateFiles::decode(&serde_json::to_vec(&StateFiles(files)).unwrap()).err().unwrap();
            assert!(!error.to_string().contains("secret unknown name"));
        }
        for blob in [b"".as_slice(), b"[\"secret malformed state\"]".as_slice()] {
            assert_eq!(StateFiles::decode(blob).err().unwrap().to_string(), "snapshot is invalid: vtpm: malformed state archive");
        }
    }

    #[test]
    #[ignore = "requires an explicit disposable helper directory and external 30-second supervisor"]
    fn disposable_swtpm_captures_manufactured_state_before_guest_commands() {
        let root = std::env::var_os("VROOM_TPM_TEST_ROOT").expect("set private disposable VROOM_TPM_TEST_ROOT");
        let temp = tempfile::tempdir_in(&root).unwrap();
        let mut tpm = Swtpm::start(temp.path(), true).unwrap();
        std::fs::write(Path::new(&root).join("helper.pid"), tpm.child.id().to_string()).unwrap();
        // No TPM guest command has been sent: permanent state must already
        // exist after successful INIT/manufacture.
        assert!(std::fs::metadata(temp.path().join(STATE_FILES[0])).unwrap().len() > 0);
        let first = tpm.save().unwrap();
        assert!(StateFiles::decode(&first).is_ok());
        let permanent = std::fs::read(temp.path().join(STATE_FILES[0])).unwrap();
        // Invalid restore input must not STOP the live TPM or replace its NV.
        for invalid in [b"".as_slice(), b"[]".as_slice()] {
            assert!(!tpm.load(invalid));
            assert!(std::fs::read(temp.path().join(STATE_FILES[0])).unwrap() == permanent);
            assert!(StateFiles::decode(&tpm.save().unwrap()).is_ok());
        }
        // Wait, not merely kill: prove the disposable helper was reaped before
        // its directory is deleted. Drop handles the same cleanup on failures.
        tpm.ctrl.shutdown(std::net::Shutdown::Both).unwrap();
        let _ = tpm.child.kill();
        tpm.child.wait().unwrap();
        drop(tpm);
        temp.close().unwrap();
    }
}
