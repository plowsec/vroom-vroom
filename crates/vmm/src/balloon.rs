//! When the Mac runs short of memory, the guest gives back its caches.
//!
//! The balloon's free page reporting already hands the Mac whatever the
//! guest frees. What it keeps is its page cache: after a big build or an
//! indexer, most of a 12 GiB guest, held by the Mac as anonymous memory it
//! can only compress or swap. When the Mac runs short, the guest is asked
//! to inflate the balloon by what it could spare, which makes it drop
//! caches; once the Mac has room again, it gets the pages back (they cost
//! the Mac nothing until the guest uses them). A guest running low on
//! memory gets them back at once, whatever the Mac.
//!
//! Short is what a memory-hungry app's guard (a local LLM server's, say)
//! would count: free pages under a floor (a sixteenth of what is wired,
//! 2.5 percent of RAM, 1 GiB at the least; twice that to be safe), or
//! free, purgeable and file-backed pages together under an eighth of RAM.
//! The file-backed pages alone don't make room: much of them can be an
//! app's mapped weights, which macOS compresses other memory to keep.

use std::time::{Duration, Instant};

use vroom_virtio::devices::balloon::Control;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// What the Mac has, in bytes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HostMemory {
    pub total: u64,
    /// Free and purgeable pages.
    pub free: u64,
    /// Those and the file-backed (not speculative) pages: what the kernel
    /// hands out without compressing anything, if it drops file pages.
    pub supply: u64,
    pub wired: u64,
    /// What the compressor occupies.
    pub compressor: u64,
}

#[cfg(target_os = "macos")]
pub(crate) fn host_memory() -> Option<HostMemory> {
    // SAFETY: a C struct of integers: all zeros is a valid value.
    let mut info: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: the buffer is a vm_statistics64 and count says its size in
    // 32-bit words; the kernel fills at most that.
    #[allow(deprecated)]
    let rc = unsafe {
        libc::host_statistics64(libc::mach_host_self(), libc::HOST_VM_INFO64, (&mut info as *mut libc::vm_statistics64).cast(), &mut count)
    };
    if rc != 0 {
        return None;
    }
    let page = vroom_memory::page_size() as u64;
    let mut total = 0u64;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: hw.memsize is a u64; len says so.
    let rc = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut total as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || total == 0 {
        return None;
    }
    let free = u64::from(info.free_count) + u64::from(info.purgeable_count);
    let file = u64::from(info.external_page_count).saturating_sub(u64::from(info.speculative_count));
    Some(HostMemory { total, free: free * page, supply: (free + file) * page, wired: u64::from(info.wire_count) * page, compressor: u64::from(info.compressor_page_count) * page })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn host_memory() -> Option<HostMemory> {
    // MemAvailable is the same idea on Linux.
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = |key: &str| -> Option<u64> {
        s.lines().find(|l| l.starts_with(key))?.split_whitespace().nth(1)?.parse::<u64>().ok().map(|k| k << 10)
    };
    // MemFree is small on a healthy Linux (the page cache has the rest):
    // what is available counts as free too.
    let supply = kib("MemAvailable:")?;
    Some(HostMemory { total: kib("MemTotal:")?, free: supply, supply, wired: 0, compressor: 0 })
}

/// Free memory under which the Mac is short while it compresses: twice
/// the floor such a guard aborts at.
pub(crate) fn free_floor(host: &HostMemory) -> u64 {
    2 * GIB.max(host.total / 40).max(host.wired / 16)
}

/// The supply floors, by the Mac's RAM: under `low` the Mac is short;
/// over `high` it has room again.
pub(crate) fn floors(total: u64) -> (u64, u64) {
    floors_for(total, cfg!(target_os = "linux"))
}

/// `small`: the host may have little RAM (Linux): never more than a
/// quarter of it, and three eighths. The Mac's 6 and 10 GiB would call
/// an 8 GiB host short for ever and squeeze every guest.
fn floors_for(total: u64, small: bool) -> (u64, u64) {
    let low = (6 * GIB).max(total / 8);
    let high = low + (4 * GIB).max(total / 16);
    if small {
        (low.min(total / 4), high.min(total / 8 * 3))
    } else {
        (low, high)
    }
}

/// The Mac now: its memory, and whether it compresses (128 MiB in the
/// last two seconds). Few free pages alone are macOS's way; few while it
/// compresses mean it keeps file pages (a model's weights) and squeezes
/// anonymous memory instead, which is what aborts a prefill.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mac {
    pub mem: HostMemory,
    pub compressing: bool,
}

impl Mac {
    fn short(&self) -> bool {
        self.mem.supply < floors(self.mem.total).0 || (self.mem.free < free_floor(&self.mem) && self.compressing)
    }

    /// How much it has over what counts as room, if it has room.
    fn room(&self) -> Option<u64> {
        if self.compressing {
            return None;
        }
        let free = self.mem.free.checked_sub(free_floor(&self.mem) + GIB)?;
        let supply = self.mem.supply.checked_sub(floors(self.mem.total).1)?;
        Some(free.min(supply)).filter(|&r| r > 0)
    }
}

/// What the guest keeps available (Linux: MemAvailable) when it inflates.
pub(crate) fn keep(guest_ram: u64) -> u64 {
    GIB.max(guest_ram / 8)
}

/// The guest now. `available`: its MemAvailable, said since the target
/// last changed; `settled`: said since the balloon reached the target.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Guest {
    pub ram: u64,
    pub available: Option<u64>,
    pub settled: Option<u64>,
    pub target: u64,
}

/// One step: the balloon's new target, or None to leave it.
pub(crate) fn decide(mac: Mac, guest: Guest) -> Option<u64> {
    let keep = keep(guest.ram);
    let target = guest.target;
    if let Some(available) = guest.available.filter(|&a| a < keep && target > 0) {
        // The guest needs memory: give it back what it lacks, twice over.
        return Some(target.saturating_sub(GIB.max(2 * (keep - available))));
    }
    if mac.short() {
        // Only once it holds what it was asked for, by what it said since:
        // earlier figures count the pages it was handing over.
        let spare = guest.settled?.saturating_sub(keep);
        let want = (target + spare).min(guest.ram.saturating_sub(keep));
        return (spare >= 64 * MIB && want > target).then_some(want);
    }
    match mac.room() {
        Some(r) if target > 0 => Some(target.saturating_sub(r)),
        _ => None,
    }
}

/// Steer the balloon for as long as the device lives.
pub(crate) fn start(control: Control, guest_ram: u64) {
    let weak = control.downgrade();
    drop(control);
    let r = std::thread::Builder::new().name("balloon".into()).spawn(move || {
        let mut said = 0u64;
        let mut told = None;
        let mut changed = Instant::now();
        let mut settled: Option<Instant> = None;
        let mut seen: std::collections::VecDeque<(Instant, u64)> = Default::default();
        while let Some(c) = weak.upgrade() {
            let host = host_memory();
            let now = Instant::now();
            let compressing = host.is_some_and(|h| {
                seen.retain(|(t, _)| now.duration_since(*t) <= Duration::from_secs(2));
                seen.push_back((now, h.compressor));
                seen.iter().map(|&(_, b)| b).min().is_some_and(|low| h.compressor >= low + 128 * MIB)
            });
            let mac = host.map(|mem| Mac { mem, compressing });
            // Quick while the Mac is short or the guest is squeezed.
            let fast = mac.is_some_and(|m| m.room().is_none()) || c.target() > 0;
            if c.running() {
                if c.has_stats() {
                    c.request_stats();
                }
                let (target, actual) = (c.target(), c.actual());
                if actual.abs_diff(target) < 16 * MIB {
                    settled.get_or_insert(now);
                }
                // Statistics said after `since`.
                let said_after = |since: Instant| {
                    c.stats().filter(|(_, age)| now.checked_sub(*age).is_some_and(|at| at > since)).map(|(s, _)| s.available)
                };
                let guest = Guest { ram: guest_ram, available: said_after(changed), settled: settled.and_then(said_after), target };
                if let Some(m) = mac {
                    if let Some(t) = decide(m, guest) {
                        let h = m.mem;
                        // A squeeze starting, then once a minute while it lasts.
                        if t > target && (target == 0 || told.is_none_or(|w: Instant| w.elapsed() > Duration::from_secs(60))) {
                            log::info!(
                                "balloon: the Mac has {:.1} GiB free, {:.1} GiB with its file cache; the guest holds {:.1} GiB back, asked for {:.1}",
                                h.free as f64 / GIB as f64,
                                h.supply as f64 / GIB as f64,
                                actual as f64 / GIB as f64,
                                t as f64 / GIB as f64
                            );
                            told = Some(now);
                        } else if t == 0 {
                            log::info!("balloon: the guest has all its memory again");
                            told = None;
                        }
                        if t != target {
                            c.set_target(t);
                            changed = now;
                            settled = None;
                        }
                    }
                }
                let released = c.released();
                if released >= said + GIB {
                    // Reported free memory counts whether the Mac held it or not.
                    log::debug!("balloon: the guest handed back {:.1} GiB so far", released as f64 / GIB as f64);
                    said = released;
                }
            }
            drop(c);
            std::thread::sleep(if fast { Duration::from_millis(250) } else { Duration::from_secs(1) });
        }
    });
    if let Err(e) = r {
        log::warn!("balloon: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: u64 = 64 * GIB;
    const GUEST: u64 = 12 * GIB;

    fn mac(free: u64, supply: u64, compressing: bool) -> Mac {
        Mac { mem: HostMemory { total: MAC, free, supply, wired: 32 * GIB, compressor: 0 }, compressing }
    }

    fn guest(available: u64, target: u64) -> Guest {
        Guest { ram: GUEST, available: Some(available), settled: Some(available), target }
    }

    #[test]
    fn the_guest_gives_its_caches_when_the_mac_runs_short() {
        // 64 GB, 32 GiB wired: 4 GiB free floor, 8 GiB supply floor; the
        // guest keeps 1.5 GiB.
        assert_eq!(free_floor(&mac(0, 0, false).mem), 4 * GIB);
        assert_eq!(floors(MAC), (8 * GIB, 12 * GIB));
        assert_eq!(floors_for(MAC, true), (8 * GIB, 12 * GIB));
        assert_eq!(floors_for(8 * GIB, true), (2 * GIB, 3 * GIB));
        assert_eq!(floors_for(8 * GIB, false), (6 * GIB, 10 * GIB));
        assert_eq!(decide(mac(3 * GIB, 5 * GIB, false), guest(9 * GIB, 0)), Some(15 * GIB / 2));
        // Few free pages, plenty of file cache: short only while it compresses.
        assert_eq!(decide(mac(GIB / 8, 10 * GIB, false), guest(9 * GIB, 0)), None);
        assert_eq!(decide(mac(GIB / 8, 10 * GIB, true), guest(9 * GIB, 0)), Some(15 * GIB / 2));
        // More, once it holds what it was asked and said what it has left.
        assert_eq!(decide(mac(GIB, 5 * GIB, true), guest(4 * GIB, 6 * GIB)), Some(17 * GIB / 2));
        let inflating = Guest { settled: None, ..guest(4 * GIB, 6 * GIB) };
        assert_eq!(decide(mac(GIB, 5 * GIB, true), inflating), None);
        // Enough on the Mac: leave it.
        assert_eq!(decide(mac(6 * GIB, 10 * GIB, false), guest(9 * GIB, 0)), None);
        // No word from the guest: leave it.
        assert_eq!(decide(mac(0, GIB, true), Guest { available: None, settled: None, ..guest(0, 0) }), None);
    }

    #[test]
    fn it_gets_them_back() {
        // Room: not compressing, free 1 GiB over its floor, supply over 12 GiB.
        assert_eq!(decide(mac(20 * GIB, 20 * GIB, false), guest(2 * GIB, 6 * GIB)), Some(0));
        assert_eq!(decide(mac(6 * GIB, 20 * GIB, false), guest(2 * GIB, 6 * GIB)), Some(5 * GIB));
        assert_eq!(decide(mac(20 * GIB, 20 * GIB, true), guest(2 * GIB, 6 * GIB)), None);
        // Between short and room: leave it.
        assert_eq!(decide(mac(4 * GIB, 20 * GIB, false), guest(2 * GIB, 6 * GIB)), None);
        // Short of memory itself: back at once, whatever the Mac.
        assert_eq!(decide(mac(0, GIB, true), guest(GIB, 6 * GIB)), Some(5 * GIB));
    }
}
