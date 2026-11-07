use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use vroom_tpm_reference_init::tpm::{Bundle, Tpm, Transport, MAX_PACKET};
use vroom_tpm_reference_init::{eventlog, fail, hash, hex, require, token, Failure, Result};

fn mount(kind: &[u8], path: &[u8]) -> Result<()> {
    // SAFETY: static NUL-terminated strings; no mount data. PID1-only guard
    // precedes all calls; these mount guest virtual filesystems, never disks.
    let rc = unsafe {
        libc::mount(
            kind.as_ptr().cast(),
            path.as_ptr().cast(),
            kind.as_ptr().cast(),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            std::ptr::null(),
        )
    };
    require(rc == 0, "mount_guest_filesystem")
}
fn read_bounded(path: &str, max: usize) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| fail("open_guest_input"))?;
    require(
        file.metadata()
            .map_err(|_| fail("guest_input_metadata"))?
            .file_type()
            .is_file(),
        "guest_input_not_regular",
    )?;
    let mut out = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| fail("read_guest_input"))?;
    require(out.len() <= max, "guest_input_too_large")?;
    Ok(out)
}
struct Device(File);
impl Transport for Device {
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        require((10..=MAX_PACKET).contains(&request.len()), "command_length")?;
        // One complete TPM character-device write/read. Partial writes are not
        // retried as another command. The host owns the guest's hard watchdog.
        let count = self
            .0
            .write(request)
            .map_err(|_| fail("tpm_device_write"))?;
        require(count == request.len(), "short_tpm_write")?;
        let mut bytes = vec![0; MAX_PACKET];
        let count = self
            .0
            .read(&mut bytes)
            .map_err(|_| fail("tpm_device_read"))?;
        require(count >= 10, "short_tpm_read")?;
        bytes.truncate(count);
        Ok(bytes)
    }
}
fn console() -> Result<()> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/dev/console")
        .map_err(|_| fail("open_console"))?;
    require(
        f.metadata()
            .map_err(|_| fail("console_metadata"))?
            .file_type()
            .is_char_device(),
        "console_type",
    )?;
    for fd in [0, 1, 2] {
        // SAFETY: f is a live guest console descriptor and fd is standard I/O.
        require(unsafe { libc::dup2(f.as_raw_fd(), fd) } >= 0, "console_dup")?;
    }
    // Keep ownership correct even if the kernel initially left a std fd closed.
    if f.as_raw_fd() <= 2 {
        std::mem::forget(f);
    }
    Ok(())
}

fn run() -> Result<String> {
    for path in ["/proc", "/sys", "/dev"] {
        fs::create_dir_all(path).map_err(|_| fail("create_mountpoint"))?;
    }
    mount(b"proc\0", b"/proc\0")?;
    mount(b"sysfs\0", b"/sys\0")?;
    mount(b"devtmpfs\0", b"/dev\0")?;
    console()?;
    mount(b"securityfs\0", b"/sys/kernel/security\0")?;
    let tok = token(&read_bounded("/etc/vroom-reference-token", 32)?)?;
    let log = read_bounded(
        "/sys/kernel/security/tpm0/binary_bios_measurements",
        eventlog::MAX_LOG,
    )?;
    let replay = eventlog::replay(&log)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/dev/tpm0")
        .map_err(|_| fail("open_tpm_device"))?;
    require(
        file.metadata()
            .map_err(|_| fail("tpm_device_metadata"))?
            .file_type()
            .is_char_device(),
        "tpm_device_type",
    )?;
    let mut tpm = Tpm(Device(file));
    tpm.family()?;
    let pcr = tpm.pcr7()?;
    require(pcr == replay.pcr, "event_log_pcr7_replay_mismatch")?;
    let size = tpm.nv_size()?;
    let (parent, parent_name) = tpm.primary()?;
    let (phase, bundle) = if let Some(size) = size {
        let b = Bundle::decode(&tpm.nv_read(size)?, &tok)?;
        require(
            b.pcr == pcr && b.parent_name == parent_name,
            "cold_boot_identity_or_pcr_mismatch",
        )?;
        ("unseal", b)
    } else {
        let policy = tpm.trial_policy(&pcr)?;
        let mut secret = tpm.random()?;
        let secret_hash = hash(&secret);
        let created = tpm.create(parent, &secret, &policy);
        secret.fill(0);
        let (private, public) = created?;
        (
            "seal",
            Bundle {
                token: tok,
                pcr,
                policy,
                secret_hash,
                parent_name,
                private,
                public,
            },
        )
    };
    let object = tpm.load(parent, &bundle)?;
    tpm.verify_policy_unseal(object, &bundle)?;
    tpm.flush(object)?;
    tpm.flush(parent)?;
    if phase == "seal" {
        tpm.nv_store(&bundle.encode()?)?;
    }
    require(tpm.pcr7()? == pcr, "pcr7_changed_during_test")?;
    tpm.shutdown()?;
    let tok = std::str::from_utf8(&tok).map_err(|_| fail("invalid_token"))?;
    Ok(format!("{{\"schema\":1,\"token\":\"{tok}\",\"phase\":\"{phase}\",\"status\":\"pass\",\"pcr7_sha256\":\"{}\",\"event_log_sha256\":\"{}\",\"replay_matches\":true,\"pcr7_event_count\":{},\"secure_boot_measured\":true,\"sealed_blob_sha256\":\"{}\",\"secret_hash\":\"{}\",\"password_auth_rejected\":true,\"empty_policy_rejected\":true,\"policy_unseal_verified\":true}}",
        hex(&pcr),hex(&hash(&log)),replay.events,hex(&bundle.blob_hash()?),hex(&bundle.secret_hash)))
}

pub fn main() {
    if std::process::id() != 1 || !Path::new("/etc/vroom-reference-token").exists() {
        eprintln!("reference init requires PID1 and its disposable initramfs token");
        std::process::exit(2);
    }
    std::panic::set_hook(Box::new(|_| {
        eprintln!("VROOM_TPM_REFERENCE {{\"schema\":1,\"status\":\"fail\",\"stage\":\"panic\"}}");
    }));
    match run() {
        Ok(report) => println!("VROOM_TPM_REFERENCE {report}"),
        Err(Failure { stage, rc }) => {
            let rc = rc.map_or_else(|| "null".into(), |x| format!("{x}"));
            println!("VROOM_TPM_REFERENCE {{\"schema\":1,\"status\":\"fail\",\"stage\":\"{stage}\",\"tpm_rc\":{rc}}}");
        }
    }
    let _ = std::io::stdout().flush();
    // SAFETY: guest-only PID1 gate. Host requires pass report plus QEMU shutdown.
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    loop {
        std::thread::park();
    }
}
