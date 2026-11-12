//! Linux: Landlock (filesystem) + seccomp-bpf (syscalls) + no_new_privs.
//! Fails closed: no enforced Landlock, no sandbox (an error, not a warning).

use std::collections::BTreeMap;
use std::path::Path;

use landlock::{
    Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, RestrictSelfAttr, Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr,
    RulesetStatus, ABI,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule, TargetArch};

use crate::rules;
use crate::{Error, Policy, Result};

fn err<E: std::fmt::Display>(e: E) -> Error {
    Error::Apply(e.to_string())
}

const ABI_RULES: ABI = ABI::V2;

/// The running kernel's Landlock ABI (< 0: none).
fn kernel_abi() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: the VERSION query reads no memory (NULL attr, size 0).
    unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, LANDLOCK_CREATE_RULESET_VERSION) }
}

/// Threads of this process now (0: unknown).
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0)
}

fn no_new_privs() -> Result<()> {
    // SAFETY: prctl with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(Error::Apply("no_new_privs failed".into()));
    }
    Ok(())
}

/// A ruleset handling every ABI 2 right, refusing to be weakened.
fn ruleset() -> Result<RulesetCreated> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI_RULES))
        .map_err(err)?
        .create()
        .map_err(err)
}

/// Allow `access` beneath `path`; on a file only the file rights of it
/// (a dir right on a file is an error under HardRequirement).
fn allow(rs: RulesetCreated, path: &Path, access: BitFlags<AccessFs>) -> Result<RulesetCreated> {
    let fd = PathFd::new(path).map_err(|e| Error::Apply(format!("{}: {e}", path.display())))?;
    let access = if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) { access } else { access & AccessFs::from_file(ABI_RULES) };
    rs.add_rule(PathBeneath::new(fd, access)).map_err(err)
}

/// Same, skipping paths this host doesn't have.
fn allow_if(rs: RulesetCreated, path: &str, access: BitFlags<AccessFs>) -> Result<RulesetCreated> {
    if Path::new(path).exists() {
        allow(rs, Path::new(path), access)
    } else {
        Ok(rs)
    }
}

/// Enforce `rs` on the whole process, or fail. Other threads already
/// running need TSYNC (ABI 8): Landlock otherwise confines only the
/// calling thread, and its siblings share its memory.
fn enforce(rs: RulesetCreated) -> Result<()> {
    let n = threads();
    rules::landlock_check(kernel_abi(), n).map_err(Error::Apply)?;
    let rs = if n == 1 { rs } else { rs.all_threads(true).map_err(err)? };
    let st = rs.restrict_self().map_err(err)?;
    if st.ruleset != RulesetStatus::FullyEnforced || (n != 1 && !st.all_threads) {
        return Err(Error::Apply(format!("landlock not enforced: {:?}, all threads {}", st.ruleset, st.all_threads)));
    }
    log::info!("landlock: enforced ({n} threads)");
    Ok(())
}

fn landlock(p: &Policy) -> Result<()> {
    let mut rs = ruleset()?;
    let file_rw = AccessFs::from_file(ABI_RULES);
    for f in &p.rw_files {
        rs = allow(rs, f, file_rw)?;
    }
    for f in &p.ro_files {
        rs = allow(rs, f, AccessFs::ReadFile.into())?;
    }
    if p.gpu {
        // Vulkan loader + driver files (read), the render node (read-write).
        let read_dir = AccessFs::ReadFile | AccessFs::ReadDir;
        for d in ["/usr/share/vulkan", "/etc/vulkan", "/usr/lib", "/sys/dev/char", "/sys/devices"] {
            rs = allow_if(rs, d, read_dir)?;
        }
        rs = allow_if(rs, "/dev/dri", file_rw | AccessFs::ReadDir)?;
    }
    for d in &p.write_dirs {
        rs = allow(rs, d, AccessFs::from_all(ABI_RULES))?;
    }
    for d in &p.share_ro {
        rs = allow(rs, d, AccessFs::ReadFile | AccessFs::ReadDir)?;
    }
    // What virtio-fs does: no device nodes, sockets, FIFOs or symlinks.
    let share = AccessFs::ReadFile
        | AccessFs::ReadDir
        | AccessFs::WriteFile
        | AccessFs::RemoveFile
        | AccessFs::RemoveDir
        | AccessFs::MakeDir
        | AccessFs::MakeReg
        | AccessFs::Refer;
    for d in &p.share_rw {
        rs = allow(rs, d, share)?;
    }
    enforce(rs)
}

/// Syscalls the VMM needs after setup. Everything else kills the process.
fn allowed(p: &Policy) -> Vec<i64> {
    use libc::*;
    let mut v: Vec<i64> = vec![
        SYS_read, SYS_write, SYS_pread64, SYS_pwrite64, SYS_readv, SYS_writev, SYS_preadv,
        SYS_pwritev, SYS_close, SYS_fstat, SYS_fstatfs, SYS_newfstatat, SYS_statx, SYS_lseek, SYS_mmap,
        SYS_munmap, SYS_mprotect, SYS_madvise, SYS_mremap, SYS_brk, SYS_futex, SYS_ioctl,
        SYS_epoll_pwait, SYS_epoll_ctl, SYS_epoll_create1, SYS_eventfd2, SYS_ppoll,
        SYS_clock_gettime, SYS_clock_nanosleep, SYS_nanosleep, SYS_sched_yield,
        SYS_sched_getaffinity, SYS_rt_sigaction, SYS_rt_sigprocmask, SYS_rt_sigreturn,
        SYS_sigaltstack, SYS_exit, SYS_exit_group, SYS_getrandom, SYS_fcntl, SYS_openat,
        SYS_mkdirat, SYS_renameat, SYS_renameat2, SYS_fchmod, SYS_fchmodat, SYS_fsync, SYS_fdatasync, SYS_ftruncate, SYS_fallocate, SYS_flock,
        SYS_getpid, SYS_gettid, SYS_clone, SYS_clone3, SYS_set_robust_list,
        SYS_rseq, SYS_prctl, SYS_getdents64, SYS_restart_syscall, SYS_socketpair,
        SYS_recvfrom, SYS_sendto, SYS_recvmsg, SYS_sendmsg, SYS_shutdown,
        SYS_getsockname, SYS_getpeername, SYS_getsockopt, SYS_setsockopt,
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend_from_slice(&[SYS_epoll_wait, SYS_poll, SYS_rename, SYS_mkdir, SYS_chmod, SYS_arch_prctl]);
    if p.network_outbound {
        // No bind: UDP flows use implicit binds via connect(). socket()
        // gets an argument rule in `seccomp` (IP families only).
        v.extend_from_slice(&[SYS_connect]);
    }
    if !p.listen_ports.is_empty() {
        v.extend_from_slice(&[SYS_accept4, SYS_accept]);
    }
    v
}

fn seccomp(p: &Policy) -> Result<()> {
    let arch = if cfg!(target_arch = "x86_64") { TargetArch::x86_64 } else { TargetArch::aarch64 };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = allowed(p).into_iter().map(|n| (n, vec![])).collect();
    // Signals only to our own threads (vCPU kicks, abort). No tkill/kill.
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() } as u64;
    let own = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, pid).map_err(err)?;
    rules.insert(libc::SYS_tgkill, vec![SeccompRule::new(vec![own]).map_err(err)?]);
    // Stopping our helpers on exit, and reaping them (own children only).
    let kill_rules = p
        .helper_pids
        .iter()
        .map(|&pid| {
            let c = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, pid as u64).map_err(err)?;
            SeccompRule::new(vec![c]).map_err(err)
        })
        .collect::<Result<Vec<_>>>()?;
    if !kill_rules.is_empty() {
        rules.insert(libc::SYS_kill, kill_rules);
    }
    rules.insert(libc::SYS_wait4, vec![]);
    rules.insert(libc::SYS_waitid, vec![]);
    if p.network_outbound {
        // New sockets: IPv4/IPv6 only. No AF_UNIX (ssh-agent, docker.sock,
        // D-Bus...), no netlink, no packet sockets.
        let fam = |f: i32| -> Result<SeccompRule> {
            let c = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, f as u64).map_err(err)?;
            SeccompRule::new(vec![c]).map_err(err)
        };
        rules.insert(libc::SYS_socket, vec![fam(libc::AF_INET)?, fam(libc::AF_INET6)?]);
    }
    let filter = SeccompFilter::new(rules, SeccompAction::KillProcess, SeccompAction::Allow, arch).map_err(err)?;
    let prog: BpfProgram = filter.try_into().map_err(err)?;
    seccompiler::apply_filter_all_threads(&prog).map_err(err)
}

/// The USB helper: libusb needs the usbfs nodes (read-write) and sysfs
/// and udev's database (read) to find devices; nothing else on disk.
pub fn apply_usb_helper() -> Result<()> {
    no_new_privs()?;
    let mut rs = ruleset()?;
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    for d in ["/sys/bus/usb", "/sys/devices", "/sys/class", "/run/udev"] {
        rs = allow_if(rs, d, read)?;
    }
    for f in ["/dev/null", "/dev/urandom"] {
        rs = allow_if(rs, f, AccessFs::ReadFile.into())?;
    }
    rs = allow_if(rs, "/dev/bus/usb", read | AccessFs::WriteFile)?;
    // libusb_init started its event thread already: TSYNC (ABI 8).
    enforce(rs)
}

pub fn apply(p: &Policy) -> Result<()> {
    no_new_privs()?;
    landlock(p)?;
    seccomp(p)
}
