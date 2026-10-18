//! Guest writes to RAM, logged on stage 2 for snapshot resets: logged RAM
//! is mapped read+exec only, the first write to a page faults here, the
//! page is marked and made writable again. Taking the log protects the
//! marked pages again, so a reset only copies back what the guest wrote.

use super::ffi;
use crate::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use vroom_memory::GuestMemory;

/// One RAM region: start, length, one bit per stage-2 page.
type LogRegion = (u64, u64, Vec<u64>);

#[derive(Debug)]
pub(super) struct DirtyLog {
    granule: u64,
    on: AtomicBool,
    regions: Mutex<Vec<LogRegion>>,
}

impl DirtyLog {
    pub(super) fn new(granule: u64) -> Self {
        Self { granule, on: AtomicBool::new(false), regions: Mutex::new(Vec::new()) }
    }

    /// Write-protect all of `mem` and start logging. vCPUs must be stopped.
    pub(super) fn start(&self, mem: &GuestMemory) -> Result<()> {
        let mut regions = self.regions.lock().unwrap_or_else(|e| e.into_inner());
        regions.clear();
        for r in mem.regions() {
            let pages = r.len().div_ceil(self.granule);
            regions.push((r.start().raw(), r.len(), vec![0; pages.div_ceil(64) as usize]));
            protect(r.start().raw(), r.len(), false)?;
        }
        self.on.store(true, Ordering::Release);
        Ok(())
    }

    /// A stage-2 fault at `ipa` on a write. True if it was a logged page:
    /// it's writable now and the guest can retry. A page already marked
    /// was made writable by another vCPU that faulted on it at the same
    /// time: retry too (making it writable again costs nothing).
    pub(super) fn fault(&self, ipa: u64) -> Result<bool> {
        if !self.on.load(Ordering::Acquire) {
            return Ok(false);
        }
        let mut regions = self.regions.lock().unwrap_or_else(|e| e.into_inner());
        let Some((start, _, bits)) = regions.iter_mut().find(|(s, l, _)| ipa >= *s && ipa - *s < *l) else {
            return Ok(false);
        };
        let page = (ipa - *start) / self.granule;
        bits[(page / 64) as usize] |= 1 << (page % 64);
        protect(*start + page * self.granule, self.granule, true)?;
        Ok(true)
    }

    /// Pages written since the last take (address, length), coalesced, and
    /// write-protected again. vCPUs must be stopped.
    pub(super) fn take(&self) -> Result<Vec<(u64, u64)>> {
        let mut regions = self.regions.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<(u64, u64)> = Vec::new();
        for (start, len, bits) in regions.iter_mut() {
            for (word_index, word) in bits.iter_mut().enumerate() {
                let mut w = std::mem::take(word);
                while w != 0 {
                    let page = word_index as u64 * 64 + w.trailing_zeros() as u64;
                    w &= w - 1;
                    let gpa = *start + page * self.granule;
                    let size = self.granule.min(*start + *len - gpa);
                    match out.last_mut() {
                        Some((s, l)) if *s + *l == gpa => *l += size,
                        _ => out.push((gpa, size)),
                    }
                }
            }
        }
        for &(gpa, len) in &out {
            protect(gpa, len, false)?;
        }
        Ok(out)
    }
}

fn protect(ipa: u64, size: u64, write: bool) -> Result<()> {
    let flags = ffi::HV_MEMORY_READ | ffi::HV_MEMORY_EXEC | if write { ffi::HV_MEMORY_WRITE } else { 0 };
    // SAFETY: changes permissions of RAM we mapped; no host memory involved.
    ffi::check("hv_vm_protect", unsafe { ffi::hv_vm_protect(ipa, size as usize, flags) })
}
