//! Execute patches: the guest runs other instructions (a debugger's BRKs)
//! at some RAM addresses, while its reads and writes still see RAM. A
//! patched stage-2 page is mapped execute-only onto a host copy of RAM
//! with the patches applied ("execute view"). A read or write of it faults:
//! one of a single register is emulated against RAM; any other (pairs,
//! vector registers, exclusives, stage-1 table walks) maps RAM read/write
//! without execute ("data view") until the next instruction fetch from the
//! page maps the copy back. An instruction on the page that accesses the
//! page that way runs one step with RAM mapped with every permission
//! ("native view"). RAM itself never holds a patch.

use super::ffi;
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use vroom_memory::MmapRegion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum View {
    /// The patched copy, execute-only.
    Execute,
    /// RAM, read/write.
    Data,
    /// RAM, read/write/execute: one instruction's step.
    Native,
}

#[derive(Debug)]
struct Page {
    /// Host address of the RAM behind the page.
    ram: usize,
    copy: MmapRegion,
    /// Offset in the page -> (patch, RAM's word when it was patched).
    patches: BTreeMap<u64, ([u8; 4], [u8; 4])>,
    view: View,
}

#[derive(Debug, Default)]
struct Pages {
    pages: BTreeMap<u64, Page>,
    /// Patched addresses whose RAM word changed: their patch was dropped.
    stale: Vec<u64>,
}

#[derive(Debug)]
pub(super) struct ExecutePatches {
    granule: u64,
    any: AtomicBool,
    pages: Mutex<Pages>,
}

impl ExecutePatches {
    pub(super) fn new(granule: u64) -> Self {
        Self { granule, any: AtomicBool::new(false), pages: Mutex::default() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pages> {
        self.pages.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn page(&self, ipa: u64) -> u64 {
        ipa & !(self.granule - 1)
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.any.load(Ordering::Acquire)
    }

    /// Whether `ipa` lies on a patched page.
    pub(super) fn covers(&self, ipa: u64) -> bool {
        !self.is_empty() && self.lock().pages.contains_key(&self.page(ipa))
    }

    /// Whether `len` bytes at `ipa` lie on one page.
    pub(super) fn fits(&self, ipa: u64, len: usize) -> bool {
        self.page(ipa) == self.page(ipa + len as u64 - 1)
    }

    /// Patch the word at `gpa` (4-byte aligned) with `word`, or put RAM's
    /// back (None). `ram` is the host address of the page holding `gpa`.
    pub(super) fn set(&self, gpa: u64, ram: *mut u8, word: Option<[u8; 4]>) -> Result<()> {
        if gpa & 3 != 0 {
            return Err(Error::Invalid(format!("execute patch at unaligned {gpa:#x}")));
        }
        let (page, offset) = (self.page(gpa), gpa & (self.granule - 1));
        let mut pages = self.lock();
        let Pages { pages, stale } = &mut *pages;
        match word {
            Some(word) => {
                let p = match pages.entry(page) {
                    std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::btree_map::Entry::Vacant(e) => {
                        let copy = MmapRegion::new((self.granule as usize).max(vroom_memory::page_size()))
                            .map_err(|e| Error::Invalid(format!("execute copy: {e}")))?;
                        e.insert(Page { ram: ram as usize, copy, patches: BTreeMap::new(), view: View::Execute })
                    }
                };
                let under = p.ram_word(offset);
                p.patches.insert(offset, (word, under));
                p.refresh(page, self.granule, stale);
                map(page, p, self.granule)?;
            }
            None => {
                let Some(p) = pages.get_mut(&page) else { return Ok(()) };
                p.patches.remove(&offset);
                if p.patches.is_empty() {
                    let p = pages.remove(&page).expect("present");
                    unmap(page, self.granule)?;
                    // SAFETY: RAM the VM mapped and keeps alive.
                    map_raw(p.ram as *mut u8, page, self.granule, ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC)?;
                } else {
                    p.refresh(page, self.granule, stale);
                }
            }
        }
        self.any.store(!pages.is_empty(), Ordering::Release);
        Ok(())
    }

    /// Map the page holding `ipa` as `view`. A page in that view already
    /// (another vCPU got there first) stays as it is.
    pub(super) fn show(&self, ipa: u64, view: View) -> Result<()> {
        let page = self.page(ipa);
        let mut pages = self.lock();
        let Pages { pages, stale } = &mut *pages;
        let Some(p) = pages.get_mut(&page) else { return Ok(()) };
        if p.view == view {
            return Ok(());
        }
        if view == View::Execute {
            p.refresh(page, self.granule, stale);
        }
        p.view = view;
        map(page, p, self.granule)
    }

    /// An emulated read: `buf.len()` bytes of RAM at `ipa`, on one page.
    pub(super) fn read(&self, ipa: u64, buf: &mut [u8]) -> bool {
        let pages = self.lock();
        let Some(p) = pages.pages.get(&self.page(ipa)) else { return false };
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

    /// An emulated write of `bytes` to RAM at `ipa`, on one page. The copy
    /// follows; a patched word it changes loses its patch.
    pub(super) fn write(&self, ipa: u64, bytes: &[u8]) -> Result<bool> {
        let page = self.page(ipa);
        let mut pages = self.lock();
        let Pages { pages, stale } = &mut *pages;
        let Some(p) = pages.get_mut(&page) else { return Ok(false) };
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
        if p.refresh(page, self.granule, stale) && p.patches.is_empty() {
            let p = pages.remove(&page).expect("present");
            unmap(page, self.granule)?;
            map_raw(p.ram as *mut u8, page, self.granule, ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC)?;
            self.any.store(!pages.is_empty(), Ordering::Release);
        }
        Ok(true)
    }

    /// Catch the copies up with RAM the guest changed without faulting
    /// here (device DMA, a data view). Returns the patched addresses whose
    /// RAM word changed since the last call: those patches are gone.
    pub(super) fn refresh(&self) -> Result<Vec<u64>> {
        let mut pages = self.lock();
        let Pages { pages, stale } = &mut *pages;
        let mut emptied = Vec::new();
        for (&page, p) in pages.iter_mut() {
            if p.refresh(page, self.granule, stale) && p.patches.is_empty() {
                emptied.push(page);
            }
        }
        for page in emptied {
            let p = pages.remove(&page).expect("present");
            unmap(page, self.granule)?;
            map_raw(p.ram as *mut u8, page, self.granule, ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC)?;
        }
        self.any.store(!pages.is_empty(), Ordering::Release);
        Ok(std::mem::take(stale))
    }

    /// Map every patched page again, after RAM was mapped whole over them.
    pub(super) fn remap(&self) -> Result<()> {
        let mut pages = self.lock();
        let Pages { pages, stale } = &mut *pages;
        for (&page, p) in pages.iter_mut() {
            p.refresh(page, self.granule, stale);
            map(page, p, self.granule)?;
        }
        Ok(())
    }
}

impl Page {
    fn ram_word(&self, offset: u64) -> [u8; 4] {
        // SAFETY: an aligned word inside the page's RAM.
        unsafe { ((self.ram + offset as usize) as *const u32).read_volatile() }.to_ne_bytes()
    }

    /// Bring the copy up to RAM plus the patches, dropping the patches
    /// whose RAM word changed. True if a patch was dropped.
    fn refresh(&mut self, page: u64, granule: u64, stale: &mut Vec<u64>) -> bool {
        let before = self.patches.len();
        let ram = self.ram;
        self.patches.retain(|&offset, (_, under)| {
            // SAFETY: as in ram_word().
            let now = unsafe { ((ram + offset as usize) as *const u32).read_volatile() }.to_ne_bytes();
            if now != *under {
                stale.push(page + offset);
            }
            now == *under
        });
        let copy = self.copy.as_ptr().cast::<u32>();
        let mut changed = false;
        for i in 0..(granule / 4) as usize {
            let want = match self.patches.get(&(i as u64 * 4)) {
                Some((word, _)) => u32::from_ne_bytes(*word),
                // SAFETY: as in ram_word().
                None => unsafe { (ram as *const u32).add(i).read_volatile() },
            };
            // SAFETY: the copy is `granule` bytes we own; vCPUs may be
            // executing it, and words change whole.
            unsafe {
                if copy.add(i).read_volatile() != want {
                    copy.add(i).write_volatile(want);
                    changed = true;
                }
            }
        }
        if changed {
            // SAFETY: the copy is a live mapping of `granule` bytes.
            unsafe { ffi::sys_icache_invalidate(self.copy.as_ptr().cast(), granule as usize) };
        }
        self.patches.len() != before
    }
}

fn map(page: u64, p: &Page, granule: u64) -> Result<()> {
    unmap(page, granule)?;
    let (pointer, flags) = match p.view {
        View::Execute => (p.copy.as_ptr(), ffi::HV_MEMORY_EXEC),
        View::Data => (p.ram as *mut u8, ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE),
        View::Native => (p.ram as *mut u8, ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC),
    };
    map_raw(pointer, page, granule, flags)
}

fn unmap(page: u64, granule: u64) -> Result<()> {
    // SAFETY: a page of guest RAM we mapped; a vCPU touching it meanwhile
    // faults, waits for the lock and runs the access again.
    ffi::check("hv_vm_unmap", unsafe { ffi::hv_vm_unmap(page, granule as usize) })
}

fn map_raw(pointer: *mut u8, page: u64, granule: u64, flags: u64) -> Result<()> {
    // SAFETY: `pointer` is the page's RAM (kept alive by the VM) or its
    // copy (kept alive by its Page until unmapped).
    ffi::check("hv_vm_map", unsafe { ffi::hv_vm_map(pointer.cast(), page, granule as usize, flags) })
}
