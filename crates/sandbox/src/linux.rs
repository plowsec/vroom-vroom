//! Linux: Landlock (filesystem) + seccomp-bpf (syscalls) + no_new_privs.

use std::collections::BTreeMap;

use landlock::{Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI};
use seccompiler::{BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule, TargetArch};

use crate::{Error, Policy, Result};

fn err<E: std::fmt::Display>(e: E) -> Error {
    Error::Apply(e.to_string())
}

fn landlock(p: &Policy) -> Result<()> {
    let abi = ABI::V2;
    let mut rs = Ruleset::default().handle_access(AccessFs::from_all(abi)).map_err(err)?.create().map_err(err)?;
    let file_rw = AccessFs::from_file(abi);
    let file_ro = AccessFs::ReadFile;
    for f in &p.rw_files {
        rs = rs.add_rule(PathBeneath::new(PathFd::new(f).map_err(err)?, file_rw)).map_err(err)?;
    }
    for f in &p.ro_files {
        rs = rs.add_rule(PathBeneath::new(PathFd::new(f).map_err(err)?, file_ro)).map_err(err)?;
    }
    if p.gpu {
        // Vulkan loader + driver files (read), the render node (read-write).
        let read_dir = AccessFs::ReadFile | AccessFs::ReadDir;
        for d in ["/usr/share/vulkan", "/etc/vulkan", "/usr/lib", "/sys/dev/char", "/sys/devices"] {
            if let Ok(fd) = PathFd::new(d) {
                rs = rs.add_rule(PathBeneath::new(fd, read_dir)).map_err(err)?;
            }
        }
        if let Ok(fd) = PathFd::new("/dev/dri") {
            rs = rs.add_rule(PathBeneath::new(fd, file_rw | AccessFs::ReadDir)).map_err(err)?;
        }
    }
    for d in &p.write_dirs {
        rs = rs.add_rule(PathBeneath::new(PathFd::new(d).map_err(err)?, AccessFs::from_all(abi))).map_err(err)?;
    }
    for d in &p.share_ro {
        rs = rs.add_rule(PathBeneath::new(PathFd::new(d).map_err(err)?, AccessFs::ReadFile | AccessFs::ReadDir)).map_err(err)?;
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
        rs = rs.add_rule(PathBeneath::new(PathFd::new(d).map_err(err)?, share)).map_err(err)?;
    }
    let st = rs.restrict_self().map_err(err)?;
    log::info!("landlock: {:?}", st.ruleset);
    Ok(())
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

pub fn apply(p: &Policy) -> Result<()> {
    // SAFETY: prctl with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(Error::Apply("no_new_privs failed".into()));
    }
    if let Err(e) = landlock(p) {
        // Old kernels: no Landlock. Seccomp still applies.
        log::warn!("landlock unavailable: {e}");
    }
    seccomp(p)
}
