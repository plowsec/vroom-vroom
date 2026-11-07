//! Bounded native Vmm Linux acceptance. See ../X64_LINUX.md.
#![forbid(unsafe_code)]

// The proven native harness parser, without its ARM module declaration.
// Kept here so the example needs no additional Cargo dependency.
#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
mod evidence {
//! Bounded serial evidence shared by the native Linux harness and tests.
#![forbid(unsafe_code)]
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

pub const SERIAL_LIMIT: usize = 128 * 1024;

#[derive(Debug)]
pub struct SerialEvidence {
    prefix: String,
    line: Vec<u8>,
    line_overflow: bool,
    pub bytes: Vec<u8>,
    pub heartbeats: u64,
    pub error: Option<String>,
}

impl SerialEvidence {
    pub fn new(token: &str) -> Result<Self, String> {
        if token.len() != 32
            || !token
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err("token must be exactly 32 lowercase hexadecimal characters".into());
        }
        Ok(Self {
            prefix: format!("VROOM_HEARTBEAT:{token}:"),
            line: Vec::new(),
            line_overflow: false,
            bytes: Vec::new(),
            heartbeats: 0,
            error: None,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > SERIAL_LIMIT {
            self.error = Some("serial output limit exceeded".into());
            return Err(io::Error::other("serial output limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        for &byte in bytes {
            if byte == b'\n' {
                if !self.line_overflow {
                    let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
                    if let Some(value) = line.strip_prefix(self.prefix.as_bytes()) {
                        let sequence = std::str::from_utf8(value)
                            .ok()
                            .and_then(|s| s.parse::<u64>().ok());
                        if sequence == Some(self.heartbeats)
                            && value == self.heartbeats.to_string().as_bytes()
                        {
                            self.heartbeats += 1;
                        } else {
                            self.error =
                                Some("heartbeat sequence malformed, repeated or skipped".into());
                        }
                    }
                }
                self.line.clear();
                self.line_overflow = false;
            } else if self.line.len() < 256 {
                self.line.push(byte);
            } else {
                self.line_overflow = true;
            }
        }
        Ok(bytes.len())
    }
}

#[derive(Clone, Debug)]
pub struct SerialSink(pub Arc<Mutex<SerialEvidence>>);
impl Write for SerialSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("serial evidence lock poisoned"))?
            .append(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    #[test]
    fn fragmented_lines_require_exact_token_sequence_and_newline() {
        let mut e = SerialEvidence::new(TOKEN).unwrap();
        e.append(b"kernel boot\nVROOM_HEARTBEAT:another-token:0\n")
            .unwrap();
        let line = format!("VROOM_HEARTBEAT:{TOKEN}:0\r\n");
        for byte in line.as_bytes() {
            e.append(&[*byte]).unwrap();
        }
        assert_eq!(e.heartbeats, 1);
        assert!(e.error.is_none());
        e.append(format!("VROOM_HEARTBEAT:{TOKEN}:1").as_bytes())
            .unwrap();
        assert_eq!(e.heartbeats, 1);
        e.append(b"\n").unwrap();
        assert_eq!(e.heartbeats, 2);
    }
    #[test]
    fn replay_and_noncanonical_sequences_fail() {
        for value in ["1", "00", "+0", "-1", "18446744073709551616"] {
            let mut e = SerialEvidence::new(TOKEN).unwrap();
            e.append(format!("VROOM_HEARTBEAT:{TOKEN}:{value}\n").as_bytes())
                .unwrap();
            assert!(e.error.is_some());
            assert_eq!(e.heartbeats, 0);
        }
        let mut e = SerialEvidence::new(TOKEN).unwrap();
        for _ in 0..2 {
            e.append(format!("VROOM_HEARTBEAT:{TOKEN}:0\n").as_bytes())
                .unwrap();
        }
        assert!(e.error.is_some());
    }
    #[test]
    fn serial_and_token_bounds_fail_closed() {
        assert!(SerialEvidence::new("../bad").is_err());
        let mut e = SerialEvidence::new(TOKEN).unwrap();
        e.append(&vec![b'x'; SERIAL_LIMIT]).unwrap();
        assert!(e.append(b"x").is_err());
        assert!(e.error.is_some());
        assert_eq!(e.bytes.len(), SERIAL_LIMIT);
    }
}

}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod native {
    use super::evidence::{SerialEvidence, SerialSink};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};
    use vroom_config::{Boot, VmConfig};
    use vroom_vmm::{RunState, StartMode, Vmm, VmmOptions};

    type Result<T> = std::result::Result<T, String>;
    type Evidence = Arc<Mutex<SerialEvidence>>;
    const CMDLINE: &str = "console=ttyS0,115200 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init panic=-1 nokaslr acpi=off pci=off noapic";

    fn serial(evidence: &Evidence) -> Result<(u64, usize)> {
        let s = evidence.lock().map_err(|_| "serial evidence lock poisoned")?;
        if let Some(error) = &s.error { return Err(error.clone()); }
        Ok((s.heartbeats, s.bytes.len()))
    }
    fn no_event(vm: &Vmm) -> Result<()> {
        if let Some(event) = vm.next_event(Duration::ZERO) {
            return Err(format!("unexpected VM event: {event:?}"));
        }
        Ok(())
    }
    fn heartbeats(vm: &Vmm, evidence: &Evidence, want: u64, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            no_event(vm)?;
            if serial(evidence)?.0 >= want { return Ok(()); }
            if Instant::now() >= deadline { return Err("PID1 heartbeat timed out".into()); }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn run(args: &[String], report: &mut serde_json::Value) -> Result<()> {
        if args.len() != 5 {
            return Err("usage: linux_x86_64 KERNEL INITRAMFS TOKEN_HEX32 NEW_STATE_DIRECTORY".into());
        }
        let evidence = Arc::new(Mutex::new(SerialEvidence::new(&args[3])?));
        let state_dir = PathBuf::from(&args[4]);
        if !state_dir.is_absolute() || state_dir.exists() {
            return Err("state directory must be a new absolute path owned by the supervising process".into());
        }
        let mut config = VmConfig::from_toml("name='native-vmm-linux'\ncpus=1\nmemory_mib=512\nballoon=false\n[clipboard]\nhost_to_guest=false\nguest_to_host=false\n[boot]\ntype='kernel'\nkernel='unused'\n")
            .map_err(|e| e.to_string())?;
        config.boot = Boot::Kernel { kernel: args[1].as_str().into(), initrd: Some(args[2].as_str().into()), cmdline: CMDLINE.into() };
        let hv = vroom_hv::default_hypervisor().map_err(|e| e.to_string())?;
        let mut vm = Vmm::start(config, hv, VmmOptions {
            serial_out: Box::new(SerialSink(evidence.clone())), start: StartMode::BootPaused,
            clipboard: None, ui: None, state_dir: state_dir.clone(), sandbox: true,
        }).map_err(|e| e.to_string())?;
        let checks = (|| -> Result<()> {
            if vm.state() != RunState::Paused || vm.vcpu_run_calls() != 0 {
                return Err("BootPaused returned without an acknowledged zero-entry state".into());
            }
            no_event(&vm)?;
            let initial = serial(&evidence)?;
            std::thread::sleep(Duration::from_millis(350));
            if vm.vcpu_run_calls() != 0 || serial(&evidence)? != initial {
                return Err("worker or serial progress during BootPaused".into());
            }
            report["boot_paused_zero_run_calls_verified"] = json!(true);
            // Same API as the CLI. Landlock may fall back; successful
            // application does not establish complete process isolation.
            vroom_sandbox::apply(&vroom_sandbox::Policy {
                write_dirs: vec![state_dir], ..Default::default()
            }).map_err(|e| format!("sandbox apply failed: {e}"))?;
            report["sandbox_apply_succeeded"] = json!(true);
            vm.resume().map_err(|e| e.to_string())?;
            heartbeats(&vm, &evidence, 5, Duration::from_secs(45))?;
            report["pid1_heartbeat_verified"] = json!(true);
            vm.pause().map_err(|e| e.to_string())?;
            no_event(&vm)?;
            if vm.state() != RunState::Paused { return Err("pause did not park the VMM".into()); }
            let paused = serial(&evidence)?;
            let calls = vm.vcpu_run_calls();
            if calls == 0 { return Err("heartbeat without worker run calls".into()); }
            report["pause_acknowledged"] = json!(true);
            report["heartbeats_before_pause"] = json!(paused.0);
            report["run_calls_at_pause"] = json!(calls);
            std::thread::sleep(Duration::from_millis(350));
            no_event(&vm)?;
            if serial(&evidence)? != paused || vm.vcpu_run_calls() != calls {
                return Err("guest progress after acknowledged pause".into());
            }
            report["paused_quiescence_verified"] = json!(true);
            vm.resume().map_err(|e| e.to_string())?;
            heartbeats(&vm, &evidence, paused.0 + 5, Duration::from_secs(5))?;
            if vm.vcpu_run_calls() <= calls { return Err("no worker progress after resume".into()); }
            report["resume_progress_verified"] = json!(true);
            no_event(&vm)
        })();
        // Stop after ordinary success/failure. The independent process
        // watchdog bounds an ioctl or thread join that cannot make progress.
        let cleanup = vm.shutdown_checked().map_err(|e| e.to_string());
        // Join makes any last worker failure observable before final pass.
        let stopped_events = no_event(&vm);
        let stopped_serial = serial(&evidence);
        if cleanup.is_ok() && stopped_events.is_ok() && vm.state() == RunState::Stopped {
            report["host_stop_and_join_verified"] = json!(true);
        }
        report["run_calls_total"] = json!(vm.vcpu_run_calls());
        if let Ok(s) = evidence.lock() {
            report["heartbeats_observed"] = json!(s.heartbeats);
            report["serial_bytes"] = json!(s.bytes.len());
            if checks.is_err() || cleanup.is_err() || stopped_events.is_err() || stopped_serial.is_err() {
                eprintln!("serial tail:\n{}", String::from_utf8_lossy(&s.bytes[s.bytes.len().saturating_sub(16384)..]));
            }
        }
        cleanup?;
        stopped_events?;
        stopped_serial?;
        checks
    }
    pub fn main() {
        let (cancel, wait) = mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if wait.recv_timeout(Duration::from_secs(65)).is_err() {
                println!("{}", json!({"schema_version":1,"status":"unknown","scope":"native_vmm_linux_x64",
                    "error":"65-second process watchdog expired","guest_poweroff_verified":false}));
                std::process::exit(2);
            }
        });
        let mut report = json!({"schema_version":1,"status":"unknown","scope":"native_vmm_linux_x64",
            "engine":"vroom_vmm_kvm","vcpus":1,"ram_mib":512,"runtime_limit_seconds":65,
            "guest_disks":0,"guest_nics":0,"boot_paused_zero_run_calls_verified":false,
            "sandbox_apply_succeeded":false,"landlock_enforcement_verified":false,
            "pid1_heartbeat_verified":false,"pause_acknowledged":false,"paused_quiescence_verified":false,
            "resume_progress_verified":false,"host_stop_and_join_verified":false,
            "guest_poweroff_verified":false,"serial_receive_verified":false,"serial_irq_delivery_verified":false,
            "guest_rpc_verified":false,"security_features_verified":false,"snapshots_verified":false});
        let result = run(&std::env::args().collect::<Vec<_>>(), &mut report);
        let code = if let Err(error) = result { report["status"] = json!("fail"); report["error"] = json!(error); 1 }
            else { report["status"] = json!("pass"); 0 };
        println!("{report}");
        let _ = cancel.send(()); let _ = watchdog.join();
        std::process::exit(code);
    }
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() { native::main(); }

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    println!("{{\"status\":\"unknown\",\"scope\":\"native_vmm_linux_x64\",\"error\":\"requires Linux x86_64 KVM; no guest was run\"}}");
    std::process::exit(2);
}
