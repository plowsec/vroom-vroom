//! Memory watches: any number of guest-physical ranges whose reads or
//! writes stop the vCPU, on stage-2 permissions instead of the two hardware
//! watchpoints. A watched page loses write permission (watching writes) or
//! read and write (watching reads); it keeps execute, so code on it runs.
//! A fault on it that touches a watched range of the watched kind stops the
//! vCPU before the access (a Watchpoint stop with the data abort's
//! syndrome); its next fault there at the same PC passes. A fault outside
//! the ranges passes at once: a single-register access is emulated against
//! RAM, anything else (pairs, vector registers, stage-1 table walks) steps
//! once with the page mapped with every permission, during which other
//! vCPUs' accesses to the page go unseen.

use super::ffi;
use crate::debug::MemoryWatch;
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// Bytes after the fault address an access whose size the syndrome does
/// not give (ISV=0) may touch: an LD4 of Q registers.
pub(super) const UNKNOWN_ACCESS: usize = 64;

#[derive(Debug)]
struct Page {
    /// Host address of the RAM behind the page.
    ram: usize,
    read: bool,
    write: bool,
    /// vCPUs stepping an access with the page open.
    open: u32,
}

#[derive(Debug, Default)]
struct Watches {
    ranges: Vec<MemoryWatch>,
    pages: BTreeMap<u64, Page>,
}

#[derive(Debug)]
pub(super) struct MemoryWatches {
    granule: u64,
    any: AtomicBool,
    inner: Mutex<Watches>,
    /// Faults that stopped, and faults that passed.
    pub(super) stops: AtomicU64,
    pub(super) passes: AtomicU64,
}

impl MemoryWatches {
    pub(super) fn new(granule: u64) -> Self {
        Self { granule, any: AtomicBool::new(false), inner: Mutex::default(), stops: AtomicU64::new(0), passes: AtomicU64::new(0) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Watches> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn page(&self, ipa: u64) -> u64 {
        ipa & !(self.granule - 1)
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.any.load(Ordering::Acquire)
    }

    /// Whether `ipa` lies on a watched page.
    pub(super) fn covers(&self, ipa: u64) -> bool {
        !self.is_empty() && self.lock().pages.contains_key(&self.page(ipa))
    }

    /// Whether `len` bytes at `ipa` lie on one page.
    pub(super) fn fits(&self, ipa: u64, len: usize) -> bool {
        self.page(ipa) == self.page(ipa + len as u64 - 1)
    }

    /// Whether a `write` (or read) of `len` bytes at `ipa` touches a range
    /// watching it.
    pub(super) fn matches(&self, ipa: u64, len: usize, write: bool) -> bool {
        let end = ipa.saturating_add(len as u64);
        self.lock().ranges.iter().any(|w| (if write { w.write } else { w.read }) && ipa < w.gpa + w.length && w.gpa < end)
    }

    /// Replace the watches. `ram` gives the host address of a stage-2 page
    /// of RAM. vCPUs must be stopped.
    pub(super) fn set(&self, ranges: &[MemoryWatch], ram: impl Fn(u64) -> Option<*mut u8>) -> Result<()> {
        let mut want: BTreeMap<u64, (bool, bool)> = BTreeMap::new();
        for w in ranges {
            if w.length == 0 || (!w.read && !w.write) {
                return Err(Error::Invalid(format!("memory watch at {:#x}: empty", w.gpa)));
            }
            let last = w.gpa.checked_add(w.length - 1).ok_or_else(|| Error::Invalid(format!("memory watch at {:#x} wraps", w.gpa)))?;
            let mut page = self.page(w.gpa);
            while page <= last {
                let entry = want.entry(page).or_default();
                entry.0 |= w.read;
                entry.1 |= w.write;
                page += self.granule;
            }
        }
        let mut pages = BTreeMap::new();
        for (&page, &(read, write)) in &want {
            let ram = ram(page).ok_or_else(|| Error::Invalid(format!("memory watch outside RAM at {page:#x}")))?;
            pages.insert(page, Page { ram: ram as usize, read, write, open: 0 });
        }
        let mut inner = self.lock();
        for &page in inner.pages.keys().filter(|p| !pages.contains_key(p)) {
            protect(page, self.granule, ALL)?;
        }
        for (&page, p) in &pages {
            protect(page, self.granule, p.flags())?;
        }
        *inner = Watches { ranges: ranges.to_vec(), pages };
        self.any.store(!inner.pages.is_empty(), Ordering::Release);
        Ok(())
    }

    /// Protect every watched page again, after RAM was mapped whole over them.
    pub(super) fn reprotect(&self) -> Result<()> {
        let inner = self.lock();
        for (&page, p) in &inner.pages {
            protect(page, self.granule, if p.open > 0 { ALL } else { p.flags() })?;
        }
        Ok(())
    }

    /// Map the page holding `ipa` with every permission while a vCPU steps
    /// an access on it.
    pub(super) fn open(&self, ipa: u64) -> Result<()> {
        let page = self.page(ipa);
        let mut inner = self.lock();
        let Some(p) = inner.pages.get_mut(&page) else { return Ok(()) };
        p.open += 1;
        if p.open == 1 {
            protect(page, self.granule, ALL)?;
        }
        Ok(())
    }

    /// The step is over: watch the page again.
    pub(super) fn close(&self, page: u64) -> Result<()> {
        let mut inner = self.lock();
        let Some(p) = inner.pages.get_mut(&page) else { return Ok(()) };
        p.open = p.open.saturating_sub(1);
        if p.open == 0 {
            protect(page, self.granule, p.flags())?;
        }
        Ok(())
    }

    /// An emulated read of `buf.len()` bytes of RAM at `ipa`, on one page.
    pub(super) fn read(&self, ipa: u64, buf: &mut [u8]) -> bool {
        let inner = self.lock();
        let Some(p) = inner.pages.get(&self.page(ipa)) else { return false };
        let at = (p.ram + (ipa & (self.granule - 1)) as usize) as *const u8;
        // SAFETY: inside the page's RAM (fits()); naturally aligned
        // accesses of 1, 2, 4, 8 bytes are single loads.
        unsafe {
            match buf.len() {
                1 => buf[0] = at.read_volatile(),
                2 if (at as usize) & 1 == 0 => buf.copy_from_slice(&at.cast::<u16>().read_volatile().to_ne_bytes()),
                4 if (at as usize) & 3 == 0 => buf.copy_from_slice(&at.cast::<u32>().read_volatile().to_ne_bytes()),
                8 if (at as usize) & 7 == 0 => buf.copy_from_slice(&at.cast::<u64>().read_volatile().to_ne_bytes()),
                _ => std::ptr::copy_nonoverlapping(at, buf.as_mut_ptr(), buf.len()),
            }
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        true
    }

    /// An emulated write of `bytes` to RAM at `ipa`, on one page.
    pub(super) fn write(&self, ipa: u64, bytes: &[u8]) -> bool {
        let inner = self.lock();
        let Some(p) = inner.pages.get(&self.page(ipa)) else { return false };
        std::sync::atomic::fence(Ordering::SeqCst);
        let at = (p.ram + (ipa & (self.granule - 1)) as usize) as *mut u8;
        // SAFETY: as in read().
        unsafe {
            match bytes.len() {
                1 => at.write_volatile(bytes[0]),
                2 if (at as usize) & 1 == 0 => at.cast::<u16>().write_volatile(u16::from_ne_bytes([bytes[0], bytes[1]])),
                4 if (at as usize) & 3 == 0 => at.cast::<u32>().write_volatile(u32::from_ne_bytes(bytes.try_into().expect("4"))),
                8 if (at as usize) & 7 == 0 => at.cast::<u64>().write_volatile(u64::from_ne_bytes(bytes.try_into().expect("8"))),
                _ => std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len()),
            }
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        true
    }
}

const ALL: u64 = ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC;

impl Page {
    fn flags(&self) -> u64 {
        match (self.read, self.write) {
            (true, _) => ffi::HV_MEMORY_EXEC,
            (false, true) => ffi::HV_MEMORY_READ | ffi::HV_MEMORY_EXEC,
            (false, false) => ALL,
        }
    }
}

fn protect(page: u64, granule: u64, flags: u64) -> Result<()> {
    // SAFETY: changes the permissions of a page of RAM the VM mapped; a
    // vCPU touching it meanwhile faults and runs the access again.
    ffi::check("hv_vm_protect", unsafe { ffi::hv_vm_protect(page, granule as usize, flags) })
}
