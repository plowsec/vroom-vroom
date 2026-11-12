//! Linux sandbox rules as plain data: syscall argument checks and the
//! Landlock ABI we insist on. Kept apart from `linux.rs` so the logic is
//! unit-tested on any host. Values are Linux's, the same on x86_64 and
//! aarch64 (asm-generic).
#![cfg_attr(not(test), allow(dead_code))]

/// Landlock ABI 2 (Linux 5.19): the rules use `Refer` (renames in shares).
pub const LANDLOCK_ABI_MIN: i64 = 2;
/// ABI 8 (Linux 7.0): LANDLOCK_RESTRICT_SELF_TSYNC. Without it only the
/// calling thread is confined, and the others share its memory: no
/// boundary at all.
pub const LANDLOCK_ABI_TSYNC: i64 = 8;

/// The ABI needed with `threads` running (0: unknown).
pub fn landlock_abi_needed(threads: usize) -> i64 {
    if threads == 1 {
        LANDLOCK_ABI_MIN
    } else {
        LANDLOCK_ABI_TSYNC
    }
}

/// `kernel`: landlock_create_ruleset(VERSION)'s answer (< 0: none).
pub fn landlock_check(kernel: i64, threads: usize) -> Result<(), String> {
    let need = landlock_abi_needed(threads);
    if kernel >= need {
        return Ok(());
    }
    let have = if kernel < 1 { "no Landlock (kernel too old or lsm= lacks landlock)".to_string() } else { format!("Landlock ABI {kernel}") };
    let why = match need {
        LANDLOCK_ABI_MIN => "ABI 2 (Linux 5.19)".to_string(),
        _ => format!("ABI 8 (Linux 7.0) to confine all {} threads", if threads == 0 { "?".to_string() } else { threads.to_string() }),
    };
    Err(format!("landlock: {have}; need {why}. --no-sandbox runs the VM unconfined"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landlock_abi() {
        assert!(landlock_check(2, 1).is_ok());
        assert!(landlock_check(1, 1).is_err());
        assert!(landlock_check(-1, 1).is_err());
        assert!(landlock_check(7, 4).is_err(), "threads need TSYNC");
        assert!(landlock_check(8, 4).is_ok());
        assert!(landlock_check(9, 0).is_ok());
        assert!(landlock_check(7, 0).is_err(), "unknown thread count");
    }

}
