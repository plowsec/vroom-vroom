//! Idle skip: while every vCPU waits in WFI, the guest's counter jumps to
//! the earliest armed timer instead of the host sleeping until it. Guest
//! time then passes as fast as the guest handles its timers.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::ffi;
use super::vcpu::HvfKicker;
use crate::VcpuKicker;

#[derive(Debug, Default)]
pub(super) struct IdleSkip {
    on: AtomicBool,
    /// Bumped on every jump: a vCPU that applied an older one reloads the
    /// VM-wide offset.
    epoch: AtomicU64,
    state: Mutex<Waiting>,
    rebasing: AtomicBool,
    rebase: Mutex<Option<u64>>,
}

#[derive(Default)]
struct Waiting {
    /// Every vCPU, by index.
    kickers: Vec<Arc<HvfKicker>>,
    /// The vCPUs in WFI, with their timer's compare value if armed.
    waiting: Vec<(u32, Option<u64>)>,
    /// Devices have work the guest waits for: no jump.
    busy: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl std::fmt::Debug for Waiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waiting").field("waiting", &self.waiting).finish_non_exhaustive()
    }
}

impl IdleSkip {
    pub(super) fn set(&self, busy: Option<Arc<dyn Fn() -> bool + Send + Sync>>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.on.store(busy.is_some(), Ordering::SeqCst);
        s.busy = busy;
    }

    pub(super) fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// The VM-wide offset changed: vCPUs take it before they next enter
    /// the guest.
    pub(super) fn moved(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// `Vm::set_clock`: the guest counter goes on from `value` once a vCPU
    /// enters the guest again (not from now: a resumed vCPU's thread takes
    /// a while to get there).
    pub(super) fn rebase(&self, value: u64) {
        *self.rebase.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
        self.rebasing.store(true, Ordering::Release);
        self.moved();
    }

    /// The guest counter while a rebase waits for a vCPU: it stands still.
    pub(super) fn pending(&self) -> Option<u64> {
        if !self.rebasing.load(Ordering::Acquire) {
            return None;
        }
        *self.rebase.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The first vCPU into the guest after `rebase` sets the offset.
    pub(super) fn take_rebase(&self, offset: &AtomicU64) {
        if !self.rebasing.load(Ordering::Acquire) {
            return;
        }
        let mut pending = self.rebase.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = pending.take() {
            // SAFETY: no preconditions.
            offset.store(unsafe { ffi::mach_absolute_time() }.wrapping_sub(value), Ordering::SeqCst);
            self.rebasing.store(false, Ordering::Release);
            self.moved();
        }
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub(super) fn add(&self, kicker: Arc<HvfKicker>) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).kickers.push(kicker);
    }

    /// vCPU `index` waits for `cval` (guest counter). The last vCPU to go
    /// idle moves the counter to the earliest timer and wakes the others.
    pub(super) fn wait(&self, index: u32, cval: Option<u64>, offset: &AtomicU64) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match s.waiting.iter_mut().find(|(i, _)| *i == index) {
            Some(entry) => entry.1 = cval,
            None => s.waiting.push((index, cval)),
        }
        if s.waiting.len() < s.kickers.len() || s.busy.as_ref().is_some_and(|busy| busy()) {
            return;
        }
        let Some(earliest) = s.waiting.iter().filter_map(|(_, c)| *c).min() else { return };
        let off = offset.load(Ordering::SeqCst);
        // SAFETY: no preconditions.
        let now = unsafe { ffi::mach_absolute_time() }.wrapping_sub(off);
        if earliest <= now {
            return;
        }
        offset.store(off.wrapping_sub(earliest - now), Ordering::SeqCst);
        self.moved();
        for (i, k) in s.kickers.iter().enumerate() {
            if i != index as usize {
                k.wake();
            }
        }
    }

    pub(super) fn leave(&self, index: u32) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).waiting.retain(|(i, _)| *i != index);
    }
}
