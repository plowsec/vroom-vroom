//! Linux: Landlock (filesystem) + seccomp-bpf (syscalls) + no_new_privs.
//! Fails closed: no enforced Landlock, no sandbox (an error, not a warning).

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering::SeqCst};

use landlock::{
    Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, RestrictSelfAttr, Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr,
    RulesetStatus, ABI,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule, TargetArch};

use crate::rules::{self, Cond, Op, Rule};
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

/// Enforce `rs` on every thread, or fail. Landlock confines only the
/// calling thread, and its siblings share its memory: ABI 8 (Linux 7.0)
/// has TSYNC for that; older kernels get `each_thread`.
fn enforce(rs: RulesetCreated) -> Result<()> {
    let abi = kernel_abi();
    rules::landlock_check(abi).map_err(Error::Apply)?;
    if rules::landlock_tsync(abi) {
        let st = rs.all_threads(true).map_err(err)?.restrict_self().map_err(err)?;
        if st.ruleset != RulesetStatus::FullyEnforced || !st.all_threads {
            return Err(Error::Apply(format!("landlock not enforced: {:?}, all threads {}", st.ruleset, st.all_threads)));
        }
        log::info!("landlock: enforced (TSYNC)");
        return Ok(());
    }
    // HardRequirement on ABI 2 rights and an ABI >= 2 kernel: what the
    // fd holds is enforced in full.
    let fd: Option<OwnedFd> = rs.into();
    let fd = fd.ok_or_else(|| Error::Apply("landlock: no ruleset".into()))?;
    each_thread(ThreadLock { landlock: fd.as_raw_fd() })
}

// ---- every thread, one by one (no TSYNC) ----
//
// libpsx's way: we signal each other thread (tgkill) with a handler
// doing the raw syscalls on itself, until a fresh /proc/self/task listing
// shows every thread but us done; then us. Threads made meanwhile by a
// confined thread inherit; by an unconfined one, they show up in the
// next listing. All before seccomp: the handler's syscalls, tgkill and
// the listing need no filter rule.
//
// SIGSYS: nothing uses it before seccomp (our filters kill or return
// errors, never trap), and libraries that block every signal in their
// workers (Mesa's util_queue) leave it open. A thread blocking it anyway,
// or not answering within SYNC_TIMEOUT: an error, the VM doesn't start.
// A thread in a syscall: the signal interrupts it; with SA_RESTART most
// restart, the rest (epoll/poll/sleeps) see EINTR, as on a vCPU kick.

const SYNC_SIGNAL: libc::c_int = libc::SIGSYS;
const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const ACK_SLOTS: usize = 4096;

/// What each thread does to itself. Raw syscalls only: a signal handler.
#[derive(Debug, Clone, Copy)]
struct ThreadLock {
    /// Landlock ruleset fd to enter.
    landlock: i32,
}

static LOCK_FD: AtomicI32 = AtomicI32::new(-1);
static ACKED: [AtomicI32; ACK_SLOTS] = [const { AtomicI32::new(0) }; ACK_SLOTS];
static N_ACKED: AtomicUsize = AtomicUsize::new(0);
/// First failure's errno; -1: more threads than ACK_SLOTS.
static FAILED: AtomicI32 = AtomicI32::new(0);

fn errno() -> i32 {
    // SAFETY: this thread's errno slot.
    unsafe { *libc::__errno_location() }
}

/// prctl(op, arg, 0, 0, 0) succeeded. Async-signal-safe.
fn prctl(op: libc::c_int, arg: libc::c_ulong) -> bool {
    let zero: libc::c_ulong = 0;
    // SAFETY: integer arguments only; the ops we pass read no memory.
    unsafe { libc::prctl(op, arg, zero, zero, zero) == 0 }
}

/// Lock the calling thread down as `l` says; 0 or an errno.
/// Async-signal-safe: raw syscalls on the stack, no allocation, no locks.
fn lock_this_thread(l: ThreadLock) -> i32 {
    // Per thread too; Landlock wants it (no CAP_SYS_ADMIN here).
    if !prctl(libc::PR_SET_NO_NEW_PRIVS, 1) {
        return errno();
    }
    // SAFETY: a ruleset fd the caller keeps open, flags 0; reads no memory.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, l.landlock, 0u32) } != 0 {
        return errno();
    }
    0
}

extern "C" fn on_sync_signal(_: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // Only our own tgkill (SI_TKILL from this process).
    // SAFETY: the kernel's siginfo for this delivery; getpid has no
    // preconditions.
    let ours = !info.is_null() && unsafe { (*info).si_code == libc::SI_TKILL && (*info).si_pid() == libc::getpid() };
    if !ours {
        return;
    }
    let saved = errno();
    let l = ThreadLock { landlock: LOCK_FD.load(SeqCst) };
    let r = lock_this_thread(l);
    if r != 0 {
        let _ = FAILED.compare_exchange(0, r, SeqCst, SeqCst);
    }
    // SAFETY: gettid has no preconditions.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let i = N_ACKED.fetch_add(1, SeqCst);
    match ACKED.get(i) {
        Some(slot) => slot.store(tid, SeqCst),
        None => {
            let _ = FAILED.compare_exchange(0, -1, SeqCst, SeqCst);
        }
    }
    // SAFETY: this thread's errno slot, as the interrupted code left it.
    unsafe { *libc::__errno_location() = saved };
}

fn task_ids() -> Result<Vec<i32>> {
    let d = std::fs::read_dir("/proc/self/task").map_err(|e| Error::Apply(format!("/proc/self/task: {e}")))?;
    Ok(d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect())
}

fn thread_name(tid: i32) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm")).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// `l` on every thread of the process.
fn each_thread(l: ThreadLock) -> Result<()> {
    N_ACKED.store(0, SeqCst);
    FAILED.store(0, SeqCst);
    for a in &ACKED {
        a.store(0, SeqCst);
    }
    LOCK_FD.store(l.landlock, SeqCst);
    // SAFETY: zeroed sigaction is a valid "no flags, empty mask" value;
    // the handler only makes raw syscalls and touches atomics.
    let (mut old, mut sa): (libc::sigaction, libc::sigaction) = unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    sa.sa_sigaction = on_sync_signal as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
    // SAFETY: valid sigaction structs we own.
    if unsafe { libc::sigemptyset(&mut sa.sa_mask) | libc::sigaction(SYNC_SIGNAL, &sa, &mut old) } != 0 {
        return Err(Error::Apply(format!("sigaction: {}", std::io::Error::last_os_error())));
    }
    if old.sa_sigaction != libc::SIG_DFL && old.sa_sigaction != libc::SIG_IGN {
        // SAFETY: putting back what was there.
        unsafe { libc::sigaction(SYNC_SIGNAL, &old, std::ptr::null_mut()) };
        return Err(Error::Apply("SIGSYS has a handler already: can't confine the other threads".into()));
    }
    sync_threads()?;
    // Everyone answered: nothing of ours is pending. On failure the
    // handler stays (a late delivery mustn't kill; we exit anyway).
    // SAFETY: putting back what was there.
    unsafe { libc::sigaction(SYNC_SIGNAL, &old, std::ptr::null_mut()) };
    // Us last: Landlock would hide /proc from us. We make no thread in
    // between, and the others' new threads inherit from them.
    match lock_this_thread(l) {
        0 => Ok(()),
        r => Err(Error::Apply(format!("lock-down: {}", std::io::Error::from_raw_os_error(r)))),
    }
}

fn sync_threads() -> Result<()> {
    // SAFETY: getpid, gettid have no preconditions.
    let (pid, me) = unsafe { (libc::getpid(), libc::syscall(libc::SYS_gettid) as i32) };
    let mut signalled: Vec<i32> = Vec::new();
    let deadline = std::time::Instant::now() + SYNC_TIMEOUT;
    loop {
        let listed = task_ids()?;
        let n = N_ACKED.load(SeqCst).min(ACK_SLOTS);
        let acked: Vec<i32> = ACKED[..n].iter().map(|a| a.load(SeqCst)).filter(|&t| t != 0).collect();
        match FAILED.load(SeqCst) {
            0 => {}
            -1 => return Err(Error::Apply(format!("lock-down: more than {ACK_SLOTS} threads"))),
            e => return Err(Error::Apply(format!("lock-down of a thread: {}", std::io::Error::from_raw_os_error(e)))),
        }
        let (todo, done) = rules::sync_pass(&listed, me, &acked, &signalled);
        if done {
            log::info!("lock-down: every thread ({})", listed.len());
            return Ok(());
        }
        for t in todo {
            match std::fs::read_to_string(format!("/proc/self/task/{t}/status")).ok().and_then(|s| rules::sig_blocked(&s, SYNC_SIGNAL)) {
                Some(true) => return Err(Error::Apply(format!("thread {t} ({}) blocks SIGSYS: can't confine it", thread_name(t)))),
                Some(false) => {}
                // Gone, or going: next listing tells.
                None => continue,
            }
            // SAFETY: a signal to a thread of our own process.
            if unsafe { libc::syscall(libc::SYS_tgkill, pid, t, SYNC_SIGNAL) } == 0 {
                signalled.push(t);
            } else {
                match errno() {
                    libc::ESRCH | libc::EAGAIN => {}
                    e => return Err(Error::Apply(format!("tgkill {t}: {}", std::io::Error::from_raw_os_error(e)))),
                }
            }
        }
        if std::time::Instant::now() > deadline {
            let late: Vec<String> = listed.iter().filter(|&&t| t != me && !acked.contains(&t)).map(|&t| format!("{t} ({})", thread_name(t))).collect();
            return Err(Error::Apply(format!("lock-down: no answer from thread(s) {}", late.join(", "))));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
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

/// Landlock for a program we start (swtpm): built in the parent, entered
/// in the child between fork and exec with two raw syscalls.
#[derive(Debug)]
pub struct ExecSandbox {
    fd: OwnedFd,
}

impl ExecSandbox {
    /// `exe` and the system's libraries and their config read-only (and
    /// executable: the dynamic loader), `rw_dir` read-write.
    pub fn new(exe: &Path, rw_dir: &Path) -> Result<ExecSandbox> {
        rules::landlock_check(kernel_abi()).map_err(Error::Apply)?;
        let read = AccessFs::ReadFile | AccessFs::ReadDir;
        let exec = read | AccessFs::Execute;
        let mut rs = ruleset()?;
        rs = allow(rs, exe, exec)?;
        // Libraries: distro paths, Nix/Guix stores, and the prefix the
        // binary is installed in (Linuxbrew, /opt/...).
        for d in ["/usr", "/lib", "/lib64", "/lib32", "/nix/store", "/gnu/store"] {
            rs = allow_if(rs, d, exec)?;
        }
        if let Some(prefix) = std::fs::canonicalize(exe).ok().as_deref().and_then(Path::parent).and_then(Path::parent) {
            if prefix != Path::new("/") {
                rs = allow(rs, prefix, exec)?;
            }
        }
        // Loader cache, OpenSSL (libtpms) and crypto policies, time zone.
        for f in ["/etc/ld.so.cache", "/etc/ld.so.preload", "/etc/ssl", "/etc/pki", "/etc/crypto-policies", "/etc/localtime", "/sys/devices/system/cpu"] {
            rs = allow_if(rs, f, read)?;
        }
        for f in ["/dev/urandom", "/dev/random"] {
            rs = allow_if(rs, f, AccessFs::ReadFile.into())?;
        }
        rs = allow_if(rs, "/dev/null", AccessFs::ReadFile | AccessFs::WriteFile)?;
        // State files, its log and lock: no device nodes, sockets, links.
        let state = read | AccessFs::WriteFile | AccessFs::RemoveFile | AccessFs::RemoveDir | AccessFs::MakeDir | AccessFs::MakeReg;
        rs = allow(rs, rw_dir, state)?;
        let fd: Option<OwnedFd> = rs.into();
        let fd = fd.ok_or_else(|| Error::Apply("landlock: no ruleset".into()))?;
        Ok(ExecSandbox { fd })
    }

    /// no_new_privs + landlock_restrict_self. Async-signal-safe: call it
    /// from `pre_exec` (the child of a forked multi-threaded parent is
    /// single-threaded, so no TSYNC).
    pub fn enter(&self) -> std::io::Result<()> {
        // SAFETY: prctl with constant arguments; touches no memory.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a ruleset fd we own, flags 0; reads no memory.
        if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, self.fd.as_raw_fd(), 0u32) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Syscalls the VMM needs after setup. Everything else kills the process.
/// Arguments of ioctl, fcntl, clone, socketpair, sendto and prctl are
/// checked by the EPERM filter (`refused`).
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
        // fchmod/fchmodat: disks.rs freeze_file (layer files read-only),
        // snapshot dirs 0700. Landlock doesn't cover chmod.
        SYS_mkdirat, SYS_renameat, SYS_renameat2, SYS_fchmod, SYS_fchmodat, SYS_fsync, SYS_fdatasync, SYS_ftruncate, SYS_fallocate, SYS_flock,
        // clone: threads only (see rules::clone_denied). clone3: ENOSYS
        // from its own filter, glibc then falls back to clone; allowed
        // here so the ENOSYS wins over a kill.
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

fn arch() -> TargetArch {
    if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else {
        TargetArch::aarch64
    }
}

fn seccomp_rules(rules: &[Rule]) -> Result<Vec<SeccompRule>> {
    rules
        .iter()
        .map(|r| {
            let conds = r
                .iter()
                .map(|&Cond { arg, wide, op }| {
                    let len = if wide { SeccompCmpArgLen::Qword } else { SeccompCmpArgLen::Dword };
                    let (op, v) = match op {
                        Op::Eq(v) => (SeccompCmpOp::Eq, v),
                        Op::Ne(v) => (SeccompCmpOp::Ne, v),
                        Op::MaskedEq(m, v) => (SeccompCmpOp::MaskedEq(m), v),
                    };
                    SeccompCondition::new(arg, len, op, v).map_err(err)
                })
                .collect::<Result<Vec<_>>>()?;
            SeccompRule::new(conds).map_err(err)
        })
        .collect()
}

/// Calls refused with EPERM rather than killed (a library probing them
/// gets an error): bad arguments of allowed syscalls. Shared by the VMM
/// and the USB helper.
fn refused() -> Result<BTreeMap<i64, Vec<SeccompRule>>> {
    let mut m = BTreeMap::new();
    m.insert(libc::SYS_ioctl, seccomp_rules(&rules::ioctl_denied())?);
    m.insert(libc::SYS_fcntl, seccomp_rules(&rules::fcntl_denied())?);
    m.insert(libc::SYS_clone, seccomp_rules(&rules::clone_denied())?);
    m.insert(libc::SYS_socketpair, seccomp_rules(&rules::socketpair_denied())?);
    m.insert(libc::SYS_sendto, seccomp_rules(&rules::sendto_denied())?);
    m.insert(libc::SYS_prctl, seccomp_rules(&rules::prctl_denied())?);
    Ok(m)
}

fn install(rules: BTreeMap<i64, Vec<SeccompRule>>, mismatch: SeccompAction, matched: SeccompAction) -> Result<()> {
    let filter = SeccompFilter::new(rules, mismatch, matched, arch()).map_err(err)?;
    let prog: BpfProgram = filter.try_into().map_err(err)?;
    seccompiler::apply_filter_all_threads(&prog).map_err(err)
}

/// clone3 → ENOSYS. Its flags are behind a pointer seccomp can't read;
/// glibc's pthread_create (2.34+) and posix_spawn fall back to clone,
/// whose flags we check. Rust std and musl threads use clone already.
fn no_clone3() -> Result<()> {
    install(BTreeMap::from([(libc::SYS_clone3, vec![])]), SeccompAction::Allow, SeccompAction::Errno(libc::ENOSYS as u32))
}

fn seccomp(p: &Policy) -> Result<()> {
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
    // The main filter has no seccomp(2): it goes last.
    no_clone3()?;
    install(refused()?, SeccompAction::Allow, SeccompAction::Errno(libc::EPERM as u32))?;
    install(rules, SeccompAction::KillProcess, SeccompAction::Allow)
}

/// Syscalls the USB helper never needs; it may run as root (vroom-netd),
/// where each is a way out of Landlock or into the kernel. A deny-list
/// (EPERM): libusb's own needs aren't fully known, and a kill there takes
/// the device away from the guest.
fn usb_refused(pid: u32) -> Result<BTreeMap<i64, Vec<SeccompRule>>> {
    use libc::*;
    let mut m = refused()?;
    let never: &[i64] = &[
        SYS_execve, SYS_execveat, SYS_ptrace, SYS_process_vm_readv, SYS_process_vm_writev,
        SYS_init_module, SYS_finit_module, SYS_delete_module, SYS_kexec_load, SYS_kexec_file_load,
        SYS_reboot, SYS_mount, SYS_umount2, SYS_pivot_root, SYS_chroot, SYS_swapon, SYS_swapoff,
        SYS_fsopen, SYS_fsmount, SYS_fsconfig, SYS_fspick, SYS_move_mount, SYS_open_tree, SYS_mount_setattr,
        SYS_bpf, SYS_perf_event_open, SYS_unshare, SYS_setns, SYS_acct, SYS_quotactl,
        SYS_settimeofday, SYS_clock_settime, SYS_adjtimex, SYS_clock_adjtime,
        SYS_sethostname, SYS_setdomainname, SYS_syslog, SYS_vhangup,
        SYS_open_by_handle_at, SYS_name_to_handle_at, SYS_fanotify_init, SYS_userfaultfd,
        SYS_add_key, SYS_request_key, SYS_keyctl,
        SYS_setuid, SYS_setgid, SYS_setreuid, SYS_setregid, SYS_setresuid, SYS_setresgid,
        SYS_setgroups, SYS_setfsuid, SYS_setfsgid, SYS_capset, SYS_personality,
        SYS_kcmp, SYS_pidfd_getfd, SYS_pidfd_send_signal, SYS_process_madvise, SYS_migrate_pages, SYS_move_pages,
        SYS_setpriority, SYS_sched_setscheduler, SYS_sched_setparam, SYS_sched_setattr, SYS_sched_setaffinity, SYS_ioprio_set,
        SYS_mknodat, SYS_tkill,
        // io_uring runs opens, sockets... past seccomp.
        SYS_io_uring_setup, SYS_io_uring_enter, SYS_io_uring_register,
        // New sockets: libusb's netlink/udev monitor is open before us.
        SYS_socket,
    ];
    for &n in never {
        m.insert(n, vec![]);
    }
    #[cfg(target_arch = "x86_64")]
    for n in [SYS_iopl, SYS_ioperm, SYS_modify_ldt, SYS_mknod, SYS_uselib] {
        m.insert(n, vec![]);
    }
    // Signals: our own threads only (abort, pthread_kill).
    for n in [SYS_kill, SYS_tgkill, SYS_rt_sigqueueinfo, SYS_rt_tgsigqueueinfo] {
        m.insert(n, seccomp_rules(&rules::signal_denied(pid))?);
    }
    m.insert(SYS_prlimit64, seccomp_rules(&rules::prlimit_denied())?);
    Ok(m)
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
    // libusb_init started its event thread already.
    enforce(rs)?;
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() } as u32;
    #[cfg(target_arch = "x86_64")]
    {
        let prog: BpfProgram = rules::x32_guard().into_iter().map(|(code, jt, jf, k)| seccompiler::sock_filter { code, jt, jf, k }).collect();
        seccompiler::apply_filter_all_threads(&prog).map_err(err)?;
    }
    no_clone3()?;
    install(usb_refused(pid)?, SeccompAction::Allow, SeccompAction::Errno(libc::EPERM as u32))
}

pub fn apply(p: &Policy) -> Result<()> {
    no_new_privs()?;
    landlock(p)?;
    seccomp(p)
}
