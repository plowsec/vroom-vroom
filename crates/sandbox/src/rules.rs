//! Linux sandbox rules as plain data: syscall argument checks and the
//! Landlock ABI we insist on. Kept apart from `linux.rs` so the logic is
//! unit-tested on any host. Values are Linux's, the same on x86_64 and
//! aarch64 (asm-generic).
#![cfg_attr(not(test), allow(dead_code))]

/// How one syscall argument is compared, as seccomp does it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq(u64),
    Ne(u64),
    /// `(arg & mask) == value`.
    MaskedEq(u64, u64),
}

/// Argument `arg` (0..6). `wide`: all 64 bits; else the low 32, which is
/// what the kernel reads for `int`/`unsigned int` arguments (high bits
/// can't dodge a rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cond {
    pub arg: u8,
    pub wide: bool,
    pub op: Op,
}

impl Cond {
    const fn dword(arg: u8, op: Op) -> Cond {
        Cond { arg, wide: false, op }
    }

    pub fn holds(&self, args: &[u64; 6]) -> bool {
        let v = args[self.arg as usize];
        let v = if self.wide { v } else { v & 0xffff_ffff };
        match self.op {
            Op::Eq(x) => v == x,
            Op::Ne(x) => v != x,
            Op::MaskedEq(m, x) => v & m == x,
        }
    }
}

/// All conditions hold.
pub type Rule = Vec<Cond>;

/// A syscall with these rules matches when one of them does.
pub fn matches(rules: &[Rule], args: &[u64; 6]) -> bool {
    rules.iter().any(|r| r.iter().all(|c| c.holds(args)))
}

fn each(arg: u8, vals: &[u64]) -> Vec<Rule> {
    vals.iter().map(|&v| vec![Cond::dword(arg, Op::Eq(v))]).collect()
}

// ioctl requests (arg 1, unsigned int).
pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;
pub const TCSETSW: u64 = 0x5403;
pub const TIOCSCTTY: u64 = 0x540E;
pub const TIOCSPGRP: u64 = 0x5410;
pub const TIOCSTI: u64 = 0x5412;
pub const TIOCGWINSZ: u64 = 0x5413;
pub const TIOCSWINSZ: u64 = 0x5414;
pub const TIOCMBIS: u64 = 0x5416;
pub const TIOCMBIC: u64 = 0x5417;
pub const TIOCMSET: u64 = 0x5418;
pub const FIONREAD: u64 = 0x541B;
pub const TIOCLINUX: u64 = 0x541C;
pub const TIOCCONS: u64 = 0x541D;
pub const TIOCSSERIAL: u64 = 0x541F;
pub const FIONBIO: u64 = 0x5421;
pub const TIOCNOTTY: u64 = 0x5422;
pub const TIOCSETD: u64 = 0x5423;
pub const TIOCSBRK: u64 = 0x5427;
pub const TIOCSRS485: u64 = 0x542F;
pub const TIOCSIG: u64 = 0x4004_5436;
pub const TIOCVHANGUP: u64 = 0x5437;
pub const FIOCLEX: u64 = 0x5451;
pub const FIOASYNC: u64 = 0x5452;
pub const TIOCSERCONFIG: u64 = 0x5453;
pub const TIOCSLCKTRMIOS: u64 = 0x5457;
pub const FIOSETOWN: u64 = 0x8901;
pub const SIOCSPGRP: u64 = 0x8902;

/// ioctls refused (EPERM) on any fd. Everything else passes: KVM, block
/// size/discard, tun, DRM and the Vulkan drivers in-process on Linux (too
/// many to list). Nothing here is used by us; the terminal raw-mode
/// restore (crates/cli/src/term.rs) needs only TCGETS/TCSETS*.
pub fn ioctl_denied() -> Vec<Rule> {
    let mut v = each(
        1,
        &[
            // Typing into the user's shell: `vroom run` holds their tty.
            TIOCSTI,
            TIOCLINUX,
            // Taking over or detaching the terminal, its job control,
            // console output, line discipline, size.
            TIOCSCTTY,
            TIOCNOTTY,
            TIOCSPGRP,
            TIOCCONS,
            TIOCSETD,
            TIOCSWINSZ,
            TIOCVHANGUP,
            TIOCSIG,
            // Serial port settings and lines.
            TIOCSSERIAL,
            TIOCSERCONFIG,
            TIOCSLCKTRMIOS,
            TIOCMBIS,
            TIOCMBIC,
            TIOCMSET,
            TIOCSBRK,
            TIOCSRS485,
            // SIGIO to a process of our choosing (as fcntl F_SETOWN).
            FIOASYNC,
            FIOSETOWN,
            SIOCSPGRP,
        ],
    );
    // Linux console: keyboard maps and function-key strings (KD*, 'K'),
    // virtual terminals (VT_*, 'V' without size bits; V4L2's have them).
    for ty in [0x4B00, 0x5600] {
        v.push(vec![Cond::dword(1, Op::MaskedEq(0xffff_ff00, ty))]);
    }
    v
}

pub const F_SETOWN: u64 = 8;
pub const F_SETSIG: u64 = 10;
pub const F_SETOWN_EX: u64 = 15;

/// fcntl commands refused: owner and signal for O_ASYNC/leases, i.e.
/// signals to any same-user process around the kill/tgkill rules. We use
/// F_GETFL/F_SETFL/F_GETFD/F_SETFD/F_DUPFD_CLOEXEC/seals/punch-hole only.
pub fn fcntl_denied() -> Vec<Rule> {
    each(1, &[F_SETOWN, F_SETSIG, F_SETOWN_EX])
}

pub const CLONE_VM: u64 = 0x100;
pub const CLONE_FS: u64 = 0x200;
pub const CLONE_FILES: u64 = 0x400;
pub const CLONE_SIGHAND: u64 = 0x800;
pub const CLONE_VFORK: u64 = 0x4000;
pub const CLONE_THREAD: u64 = 0x1_0000;
pub const CLONE_SYSVSEM: u64 = 0x4_0000;
pub const CLONE_SETTLS: u64 = 0x8_0000;
pub const CLONE_PARENT_SETTID: u64 = 0x10_0000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
pub const CLONE_DETACHED: u64 = 0x40_0000;
pub const CLONE_CHILD_SETTID: u64 = 0x100_0000;
/// NEWNS, NEWCGROUP, NEWUTS, NEWIPC, NEWUSER, NEWPID, NEWNET.
pub const CLONE_NEW: [u64; 7] = [0x2_0000, 0x200_0000, 0x400_0000, 0x800_0000, 0x1000_0000, 0x2000_0000, 0x4000_0000];

/// clone flags refused (arg 0; the kernel reads the low 32 bits): no new
/// process (fork, posix_spawn), no namespaces. Threads (Rust std through
/// glibc/musl pthread_create) always pass CLONE_THREAD.
pub fn clone_denied() -> Vec<Rule> {
    let mut v = vec![vec![Cond::dword(0, Op::MaskedEq(CLONE_THREAD, 0))]];
    v.extend(CLONE_NEW.iter().map(|&f| vec![Cond::dword(0, Op::MaskedEq(f, f))]));
    v
}

pub const AF_UNIX: u64 = 1;

/// socketpair: AF_UNIX only (control.rs's update pipe, usb disks).
pub fn socketpair_denied() -> Vec<Rule> {
    vec![vec![Cond::dword(0, Op::Ne(AF_UNIX))]]
}

/// sendto with a destination (arg 4, pointer): a Unix datagram socket
/// could reach any pathname socket (journald, syslog). All our sends are
/// on connected sockets (send() is sendto with NULL on both arches).
pub fn sendto_denied() -> Vec<Rule> {
    vec![vec![Cond { arg: 4, wide: true, op: Op::Ne(0) }]]
}

pub const PR_SET_DUMPABLE: u64 = 4;
pub const PR_SET_KEEPCAPS: u64 = 8;
pub const PR_SET_NAME: u64 = 15;
pub const PR_SET_SECUREBITS: u64 = 28;
pub const PR_SET_MM: u64 = 35;
pub const PR_SET_CHILD_SUBREAPER: u64 = 36;
pub const PR_CAP_AMBIENT: u64 = 47;
pub const PR_SET_SPECULATION_CTRL: u64 = 53;
pub const PR_SET_IO_FLUSHER: u64 = 57;
pub const PR_SCHED_CORE: u64 = 62;
pub const PR_SET_PTRACER: u64 = 0x5961_6d61;
pub const PR_SET_VMA: u64 = 0x5356_4d41;

/// prctl options refused. A deny-list: thread names (PR_SET_NAME from
/// pthread_setname_np) and glibc's PR_SET_VMA are ours, but the Vulkan
/// drivers loaded in-process with 3D may use others.
pub fn prctl_denied() -> Vec<Rule> {
    each(
        0,
        &[
            PR_SET_DUMPABLE,
            PR_SET_PTRACER,
            PR_SET_KEEPCAPS,
            PR_SET_SECUREBITS,
            PR_CAP_AMBIENT,
            PR_SET_MM,
            PR_SET_CHILD_SUBREAPER,
            PR_SET_SPECULATION_CTRL,
            PR_SET_IO_FLUSHER,
            PR_SCHED_CORE,
        ],
    )
}

/// kill/tgkill/rt_*sigqueueinfo to anyone but `pid` (arg 0, pid_t).
pub fn signal_denied(pid: u32) -> Vec<Rule> {
    vec![vec![Cond::dword(0, Op::Ne(pid as u64))]]
}

/// prlimit64 on another process, or setting a limit (glibc's getrlimit is
/// prlimit64(0, r, NULL, old)).
pub fn prlimit_denied() -> Vec<Rule> {
    vec![vec![Cond::dword(0, Op::Ne(0))], vec![Cond { arg: 2, wide: true, op: Op::Ne(0) }]]
}

/// Landlock ABI 2 (Linux 5.19): the rules use `Refer` (renames in shares).
pub const LANDLOCK_ABI_MIN: i64 = 2;
/// ABI 8 (Linux 7.0): LANDLOCK_RESTRICT_SELF_TSYNC, the kernel confines
/// every thread. Older: we do it thread by thread (`linux::each_thread`).
pub const LANDLOCK_ABI_TSYNC: i64 = 8;

/// The kernel confines all threads itself.
pub fn landlock_tsync(kernel: i64) -> bool {
    kernel >= LANDLOCK_ABI_TSYNC
}

/// `kernel`: landlock_create_ruleset(VERSION)'s answer (< 0: none).
pub fn landlock_check(kernel: i64) -> Result<(), String> {
    if kernel >= LANDLOCK_ABI_MIN {
        return Ok(());
    }
    let have = if kernel < 1 { "no Landlock (kernel too old or lsm= lacks landlock)".to_string() } else { format!("Landlock ABI {kernel}") };
    Err(format!("landlock: {have}; need ABI 2 (Linux 5.19). --no-sandbox runs the VM unconfined"))
}

/// One round of confining every thread: whom to signal now (listed, not
/// us, not signalled yet) and whether we're done (every listed thread but
/// us answered). Done only on a fresh listing: a thread made after it is
/// made by a confined one and inherits its domain.
pub fn sync_pass(listed: &[i32], me: i32, acked: &[i32], signalled: &[i32]) -> (Vec<i32>, bool) {
    let others = || listed.iter().copied().filter(move |&t| t != me);
    let todo = others().filter(|t| !signalled.contains(t)).collect();
    let done = others().all(|t| acked.contains(&t));
    (todo, done)
}

/// `/proc/self/task/<tid>/status`: is signal `sig` in SigBlk? None:
/// no such line.
pub fn sig_blocked(status: &str, sig: i32) -> Option<bool> {
    let hex = status.lines().find_map(|l| l.strip_prefix("SigBlk:"))?.trim();
    let mask = u64::from_str_radix(hex, 16).ok()?;
    Some((1..=64).contains(&sig) && mask & (1u64 << (sig - 1)) != 0)
}

/// x86_64 only: kill x32-ABI syscalls (nr | 0x4000_0000) and foreign
/// arches. A default-allow filter keyed on x86_64 numbers misses them.
/// Instructions as (code, jt, jf, k).
pub const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
pub const AUDIT_ARCH_X86_64: u32 = 62 | 0x8000_0000 | 0x4000_0000;
pub const X32_SYSCALL_BIT: u32 = 0x4000_0000;

pub fn x32_guard() -> Vec<(u16, u8, u8, u32)> {
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const RET_K: u16 = 0x06;
    vec![
        (LD_W_ABS, 0, 0, 4), // seccomp_data.arch
        (JEQ_K, 1, 0, AUDIT_ARCH_X86_64),
        (RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS),
        (LD_W_ABS, 0, 0, 0), // seccomp_data.nr
        (JGE_K, 0, 1, X32_SYSCALL_BIT),
        (RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS),
        (RET_K, 0, 0, SECCOMP_RET_ALLOW),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(i: usize, v: u64) -> [u64; 6] {
        let mut x = [0; 6];
        x[i] = v;
        x
    }

    #[test]
    fn terminal_injection_is_refused_high_bits_or_not() {
        let d = ioctl_denied();
        for r in [TIOCSTI, TIOCLINUX, TIOCSETD, TIOCCONS, TIOCSCTTY, TIOCSPGRP, FIOASYNC, FIOSETOWN] {
            assert!(matches(&d, &a(1, r)), "{r:#x}");
            assert!(matches(&d, &a(1, 0xdead_beef_0000_0000 | r)), "{r:#x} with high bits");
        }
        // KDSKBSENT (function-key strings), KDSKBENT, VT_ACTIVATE.
        for r in [0x4B49, 0x4B47, 0x5606] {
            assert!(matches(&d, &a(1, r)), "{r:#x}");
        }
    }

    #[test]
    fn ioctls_we_use_pass() {
        let d = ioctl_denied();
        for r in [
            TCGETS, TCSETS, TCSETSW, TIOCGWINSZ, FIONREAD, FIONBIO, FIOCLEX,
            0xAE80,      // KVM_RUN
            0xAE41,      // KVM_CREATE_VCPU
            0x4020_AE46, // KVM_SET_USER_MEMORY_REGION
            0x8008_1272, // BLKGETSIZE64
            0x1277,      // BLKDISCARD
            0x1268,      // BLKSSZGET
            0x4004_54CA, // TUNSETIFF
            0xC040_6400, // DRM_IOCTL_VERSION
            0x8068_5600, // VIDIOC_QUERYCAP ('V' with size bits)
            0xC018_4B00, // 'K' with size bits isn't the console's
        ] {
            assert!(!matches(&d, &a(1, r)), "{r:#x}");
        }
    }

    #[test]
    fn fcntl_owner_and_signal_are_refused() {
        let d = fcntl_denied();
        assert!(matches(&d, &a(1, F_SETOWN)) && matches(&d, &a(1, F_SETOWN_EX)) && matches(&d, &a(1, F_SETSIG)));
        // F_GETFD F_SETFD F_GETFL F_SETFL F_DUPFD_CLOEXEC F_ADD_SEALS F_GET_SEALS
        for c in [1, 2, 3, 4, 1030, 1033, 1034] {
            assert!(!matches(&d, &a(1, c)), "{c}");
        }
    }

    #[test]
    fn clone_threads_only() {
        let d = clone_denied();
        let glibc = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SYSVSEM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
        assert_eq!(glibc, 0x3d0f00);
        assert!(!matches(&d, &a(0, glibc)));
        assert!(!matches(&d, &a(0, glibc | CLONE_DETACHED)), "musl");
        // glibc fork, posix_spawn's vfork.
        assert!(matches(&d, &a(0, CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID | 17)));
        assert!(matches(&d, &a(0, CLONE_VM | CLONE_VFORK | 17)));
        for f in CLONE_NEW {
            assert!(matches(&d, &a(0, glibc | f)), "{f:#x}");
        }
    }

    #[test]
    fn sockets_and_sends() {
        assert!(!matches(&socketpair_denied(), &a(0, AF_UNIX)));
        assert!(matches(&socketpair_denied(), &a(0, 2)));
        assert!(!matches(&sendto_denied(), &a(4, 0)));
        assert!(matches(&sendto_denied(), &a(4, 0x7fff_0000_1000)));
    }

    #[test]
    fn prctl_names_pass() {
        let d = prctl_denied();
        assert!(!matches(&d, &a(0, PR_SET_NAME)) && !matches(&d, &a(0, PR_SET_VMA)));
        assert!(matches(&d, &a(0, PR_SET_PTRACER)) && matches(&d, &a(0, PR_SET_DUMPABLE)));
    }

    #[test]
    fn signals_and_limits() {
        assert!(!matches(&signal_denied(42), &a(0, 42)));
        assert!(matches(&signal_denied(42), &a(0, 1)));
        assert!(matches(&signal_denied(42), &a(0, u32::MAX as u64)), "kill(-1)");
        assert!(!matches(&prlimit_denied(), &[0, 7, 0, 0x1000, 0, 0]), "getrlimit");
        assert!(matches(&prlimit_denied(), &[1, 7, 0, 0x1000, 0, 0]));
        assert!(matches(&prlimit_denied(), &[0, 7, 0x1000, 0, 0, 0]));
    }

    #[test]
    fn landlock_abi() {
        assert!(landlock_check(2).is_ok());
        assert!(landlock_check(1).is_err());
        assert!(landlock_check(-1).is_err());
        assert!(!landlock_tsync(7) && landlock_tsync(8) && landlock_tsync(9));
    }

    #[test]
    fn every_thread_answers_before_done() {
        // We (1) are confined already; 2 and 3 run.
        assert_eq!(sync_pass(&[1, 2, 3], 1, &[], &[]), (vec![2, 3], false));
        assert_eq!(sync_pass(&[1, 2, 3], 1, &[2], &[2, 3]), (vec![], false));
        // 3 made 4 before it answered: 4 is new, not done yet.
        assert_eq!(sync_pass(&[1, 2, 3, 4], 1, &[2, 3], &[2, 3]), (vec![4], false));
        assert_eq!(sync_pass(&[1, 2, 3, 4], 1, &[2, 3, 4], &[2, 3, 4]), (vec![], true));
        // 2 exited before answering: not listed, not waited for.
        assert_eq!(sync_pass(&[1, 3], 1, &[3], &[2, 3]), (vec![], true));
        assert_eq!(sync_pass(&[1], 1, &[], &[]), (vec![], true));
    }

    #[test]
    fn blocked_signal_mask() {
        let st = "Name:\tusb-events\nSigPnd:\t0000000000000000\nSigBlk:\t0000000000000080\nSigIgn:\t0000000000001000\n";
        assert_eq!(sig_blocked(st, 8), Some(true));
        assert_eq!(sig_blocked(st, 31), Some(false), "SIGSYS");
        // Mesa's worker threads: all but SIGSYS (and glibc's 32, 33).
        let mesa = "SigBlk:\tfffffffe3fffffff\n";
        assert_eq!(sig_blocked(mesa, 31), Some(false));
        assert_eq!(sig_blocked(mesa, 34), Some(true));
        assert_eq!(sig_blocked("Name:\tx\n", 31), None);
    }

    /// A tiny BPF interpreter for the guard's few opcodes.
    fn run(prog: &[(u16, u8, u8, u32)], nr: u32, arch: u32) -> u32 {
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let (code, jt, jf, k) = prog[pc];
            pc += 1;
            match code {
                0x20 => acc = if k == 0 { nr } else { arch },
                0x15 => pc += if acc == k { jt } else { jf } as usize,
                0x35 => pc += if acc >= k { jt } else { jf } as usize,
                0x06 => return k,
                _ => panic!("opcode {code:#x}"),
            }
        }
    }

    #[test]
    fn x32_and_foreign_arches_die() {
        let g = x32_guard();
        assert_eq!(run(&g, 59, AUDIT_ARCH_X86_64), SECCOMP_RET_ALLOW);
        assert_eq!(run(&g, X32_SYSCALL_BIT | 59, AUDIT_ARCH_X86_64), SECCOMP_RET_KILL_PROCESS);
        assert_eq!(run(&g, 11, 3 | 0x4000_0000), SECCOMP_RET_KILL_PROCESS, "i386");
    }
}
