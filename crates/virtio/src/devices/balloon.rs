//! virtio-balloon: the guest gives RAM back to the host.
//!
//! A guest keeps every page it ever touched: freed memory and its page
//! cache stay the host's to hold, so a guest given 12 GiB holds 12 GiB of
//! the Mac after one big build. Two ways back:
//! - Free page reporting: the guest hands over its free memory as it frees
//!   it (a process ends, caches drop). Always on.
//! - Inflate and deflate: the host asks for an amount ([`Control`]); the
//!   guest finds it (shrinking its caches first) and hands those pages over
//!   until the host lets it have them back.
//!
//! The guest's statistics tell whoever steers it what the guest can spare.
//! Pages handed over go to [`Release`]: the host frees them, and the guest
//! reads zeros there when it uses them again (it expects nothing else).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::worker::{Poker, QueueCtx, QueueHandler, WorkerSlot, POKE_TOKEN, RESUME_TOKEN};
use crate::{types, ActivateContext, DeviceState, Result, VirtioDevice};

pub mod feature {
    /// The guest tells before it uses deflated pages again.
    pub const MUST_TELL_HOST: u64 = 1 << 0;
    pub const STATS_VQ: u64 = 1 << 1;
    /// The guest may take pages back when it runs out of memory.
    pub const DEFLATE_ON_OOM: u64 = 1 << 2;
    pub const REPORTING: u64 = 1 << 5;
}

/// Frees `len` bytes of guest RAM at guest physical `gpa`, both multiples
/// of the host page size; false if it can't now (nothing changed then).
pub type Release = Arc<dyn Fn(u64, u64) -> bool + Send + Sync>;

/// Balloon page numbers count 4 KiB, whatever either side's page size.
const PFN_SHIFT: u32 = 12;
const QUEUE_SIZE: u16 = 256;
const INFLATE: usize = 0;
const DEFLATE: usize = 1;

/// What the guest last said it has, in bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub total: u64,
    pub free: u64,
    /// What it could hand out without swapping (Linux: MemAvailable).
    pub available: u64,
    pub caches: u64,
    pub swap_in: u64,
    pub swap_out: u64,
}

impl Stats {
    fn parse(b: &[u8]) -> Self {
        let mut s = Stats::default();
        for e in b.chunks_exact(10) {
            let tag = u16::from_le_bytes([e[0], e[1]]);
            let mut v = [0u8; 8];
            v.copy_from_slice(&e[2..10]);
            let v = u64::from_le_bytes(v);
            match tag {
                0 => s.swap_in = v,
                1 => s.swap_out = v,
                4 => s.free = v,
                5 => s.total = v,
                6 => s.available = v,
                7 => s.caches = v,
                _ => {}
            }
        }
        s
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// Pages the host asks for (config `num_pages`).
    target: AtomicU32,
    /// Pages the guest says it holds (config `actual`).
    actual: AtomicU32,
    stats: Mutex<Option<(Stats, Instant)>>,
    /// Bytes freed on the host since start.
    released: AtomicU64,
    /// Whether the guest's driver runs (and which parts of it).
    features: AtomicU64,
    running: AtomicBool,
    poker: Mutex<Option<Poker>>,
    config_changed: AtomicBool,
    want_stats: AtomicBool,
}

impl Shared {
    fn poke(&self) {
        if let Some(p) = self.poker.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            p.poke();
        }
    }
}

/// Steers a [`Balloon`] from outside (the host's memory policy).
#[derive(Clone)]
pub struct Control(Arc<Shared>);

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").field("target", &self.target()).field("actual", &self.actual()).finish()
    }
}

/// A [`Control`] that doesn't keep the device's state alive.
#[derive(Debug, Clone)]
pub struct WeakControl(std::sync::Weak<Shared>);

impl WeakControl {
    pub fn upgrade(&self) -> Option<Control> {
        self.0.upgrade().map(Control)
    }
}

impl Control {
    pub fn downgrade(&self) -> WeakControl {
        WeakControl(Arc::downgrade(&self.0))
    }

    /// Ask the guest to hold `bytes` in the balloon.
    pub fn set_target(&self, bytes: u64) {
        let pages = u32::try_from(bytes >> PFN_SHIFT).unwrap_or(u32::MAX);
        if self.0.target.swap(pages, Ordering::AcqRel) != pages {
            self.0.config_changed.store(true, Ordering::Release);
            self.0.poke();
        }
    }

    pub fn target(&self) -> u64 {
        u64::from(self.0.target.load(Ordering::Acquire)) << PFN_SHIFT
    }

    /// What the guest holds in the balloon now.
    pub fn actual(&self) -> u64 {
        u64::from(self.0.actual.load(Ordering::Acquire)) << PFN_SHIFT
    }

    /// Bytes the host got back so far (reported and inflated).
    pub fn released(&self) -> u64 {
        self.0.released.load(Ordering::Acquire)
    }

    /// The driver runs and can inflate.
    pub fn running(&self) -> bool {
        self.0.running.load(Ordering::Acquire)
    }

    pub fn has_stats(&self) -> bool {
        self.running() && self.0.features.load(Ordering::Acquire) & feature::STATS_VQ != 0
    }

    pub fn reporting(&self) -> bool {
        self.running() && self.0.features.load(Ordering::Acquire) & feature::REPORTING != 0
    }

    /// The guest's latest statistics and how old they are.
    pub fn stats(&self) -> Option<(Stats, Duration)> {
        self.0.stats.lock().unwrap_or_else(|e| e.into_inner()).map(|(s, t)| (s, t.elapsed()))
    }

    /// Ask the guest for fresh statistics (they come in a moment).
    pub fn request_stats(&self) {
        self.0.want_stats.store(true, Ordering::Release);
        self.0.poke();
    }
}

/// Which 4 KiB pages of guest RAM are in the balloon. A host page goes
/// back once all of its 4 KiB pages are in.
#[derive(Debug, Clone)]
struct Inflated {
    /// (first pfn, bitmap) per RAM region.
    regions: Vec<(u64, Vec<u64>)>,
    /// 4 KiB pages per host page.
    per_host: u64,
}

impl Inflated {
    fn new(ram: &[(u64, u64)], host_page: u64) -> Self {
        let regions =
            ram.iter().map(|&(gpa, len)| (gpa >> PFN_SHIFT, vec![0u64; ((len >> PFN_SHIFT) as usize).div_ceil(64)])).collect();
        Inflated { regions, per_host: (host_page >> PFN_SHIFT).max(1) }
    }

    fn find(&mut self, pfn: u64) -> Option<(&mut Vec<u64>, usize)> {
        self.regions.iter_mut().find_map(|(first, b)| {
            let i = pfn.checked_sub(*first)? as usize;
            (i < b.len() * 64).then_some((b, i))
        })
    }

    fn set(&mut self, pfn: u64, on: bool) -> bool {
        let Some((b, i)) = self.find(pfn) else { return false };
        if on {
            b[i / 64] |= 1 << (i % 64);
        } else {
            b[i / 64] &= !(1 << (i % 64));
        }
        true
    }

    fn get(&mut self, pfn: u64) -> bool {
        self.find(pfn).is_some_and(|(b, i)| b[i / 64] & (1 << (i % 64)) != 0)
    }

    /// The host page holding `pfn`, if all of it is in the balloon: its
    /// first pfn.
    fn whole_host_page(&mut self, pfn: u64) -> Option<u64> {
        let first = pfn & !(self.per_host - 1);
        (first..first + self.per_host).all(|p| self.get(p)).then_some(first)
    }

    /// Runs of set pages (first pfn, count), for a snapshot.
    fn runs(&self) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::new();
        for (first, b) in &self.regions {
            for (w, &word) in b.iter().enumerate() {
                if word == 0 {
                    continue;
                }
                for bit in 0..64 {
                    if word & (1 << bit) != 0 {
                        let pfn = first + (w * 64 + bit) as u64;
                        match out.last_mut() {
                            Some((s, n)) if *s + *n == pfn => *n += 1,
                            _ => out.push((pfn, 1)),
                        }
                    }
                }
            }
        }
        out
    }
}

/// Merge sorted host-page starts into (gpa, len) runs.
fn coalesce(mut pfns: Vec<u64>, host_page: u64) -> Vec<(u64, u64)> {
    pfns.sort_unstable();
    pfns.dedup();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for p in pfns {
        let gpa = p << PFN_SHIFT;
        match out.last_mut() {
            Some((s, l)) if *s + *l == gpa => *l += host_page,
            _ => out.push((gpa, host_page)),
        }
    }
    out
}

#[derive(Serialize, Deserialize)]
struct Saved {
    actual: u32,
    stats_head: Option<u16>,
    inflated: Vec<(u64, u64)>,
}

pub struct Balloon {
    shared: Arc<Shared>,
    release: Release,
    host_page: u64,
    ram: Vec<(u64, u64)>,
    worker: WorkerSlot<Handler>,
}

impl std::fmt::Debug for Balloon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Balloon").field("ram", &self.ram).finish_non_exhaustive()
    }
}

impl Balloon {
    /// `ram`: guest RAM as (gpa, len); `host_page`: what `release` frees
    /// at the least (a power of two, 4 KiB or more).
    pub fn new(ram: Vec<(u64, u64)>, host_page: u64, release: Release) -> (Self, Control) {
        let shared = Arc::new(Shared::default());
        let host_page = host_page.max(1 << PFN_SHIFT).next_power_of_two();
        (Balloon { shared: shared.clone(), release, host_page, ram, worker: WorkerSlot::default() }, Control(shared))
    }

    fn handler(&self, features: u64) -> Handler {
        let stats = (features & feature::STATS_VQ != 0).then_some(2);
        let report = (features & feature::REPORTING != 0).then_some(2 + usize::from(stats.is_some()));
        Handler {
            shared: self.shared.clone(),
            release: self.release.clone(),
            host_page: self.host_page,
            inflated: Inflated::new(&self.ram, self.host_page),
            stats,
            report,
            stats_head: None,
        }
    }

    fn start(&mut self, ctx: ActivateContext, saved: Option<Saved>) -> Result<()> {
        let mut h = self.handler(ctx.features);
        self.shared.features.store(ctx.features, Ordering::Release);
        // Restored: paused until the VM resumes.
        let paused = saved.is_some();
        if let Some(s) = saved {
            let size = h.stats.and_then(|q| ctx.queues.get(q)).map_or(0, |q| q.size);
            h.stats_head = s.stats_head.filter(|&head| head < size);
            for (pfn, n) in s.inflated.into_iter().take(1 << 24) {
                for p in pfn..pfn.saturating_add(n.min(1 << 24)) {
                    h.inflated.set(p, true);
                }
            }
            self.shared.actual.store(s.actual, Ordering::Release);
        }
        self.worker.start("balloon", ctx, h, paused)?;
        self.shared.running.store(true, Ordering::Release);
        Ok(())
    }
}

struct Handler {
    shared: Arc<Shared>,
    release: Release,
    host_page: u64,
    inflated: Inflated,
    stats: Option<usize>,
    report: Option<usize>,
    /// The statistics buffer the guest left with us: handing it back asks
    /// for fresh ones.
    stats_head: Option<u16>,
}

impl Handler {
    fn release(&self, runs: Vec<(u64, u64)>) {
        for (gpa, len) in runs {
            if (self.release)(gpa, len) {
                self.shared.released.fetch_add(len, Ordering::AcqRel);
            }
        }
    }

    fn pfns(ctx: &mut QueueCtx, q: usize, mut f: impl FnMut(u64)) {
        loop {
            let chain = match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("balloon q{q}: {e}");
                    break;
                }
            };
            let head = chain.head();
            if let Ok((mut r, _)) = chain.into_rw() {
                if let Ok(b) = r.read_to_vec() {
                    for p in b.chunks_exact(4) {
                        f(u64::from(u32::from_le_bytes([p[0], p[1], p[2], p[3]])));
                    }
                }
            }
            ctx.add_used(q, head, 0);
        }
    }

    fn inflate(&mut self, ctx: &mut QueueCtx) {
        let mut whole = Vec::new();
        let inflated = &mut self.inflated;
        Self::pfns(ctx, INFLATE, |pfn| {
            if inflated.set(pfn, true) {
                if let Some(first) = inflated.whole_host_page(pfn) {
                    whole.push(first);
                }
            }
        });
        self.release(coalesce(whole, self.host_page));
    }

    fn deflate(&mut self, ctx: &mut QueueCtx) {
        let inflated = &mut self.inflated;
        Self::pfns(ctx, DEFLATE, |pfn| {
            inflated.set(pfn, false);
        });
    }

    fn stats(&mut self, ctx: &mut QueueCtx, q: usize) {
        loop {
            let chain = match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("balloon stats: {e}");
                    break;
                }
            };
            let head = chain.head();
            if let Ok((mut r, _)) = chain.into_rw() {
                if let Ok(b) = r.read_to_vec() {
                    *self.shared.stats.lock().unwrap_or_else(|e| e.into_inner()) = Some((Stats::parse(&b), Instant::now()));
                }
            }
            // Kept until the next request; an older one goes back now.
            if let Some(old) = self.stats_head.replace(head) {
                ctx.add_used(q, old, 0);
            }
        }
    }

    /// Free page reporting: each buffer is a run of free guest memory.
    fn report(&mut self, ctx: &mut QueueCtx, q: usize) {
        let page = self.host_page;
        loop {
            let chain = match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("balloon reporting: {e}");
                    break;
                }
            };
            let head = chain.head();
            let mut runs = Vec::new();
            for d in chain {
                let Ok(d) = d else { break };
                // Only whole host pages inside it.
                let start = d.addr.next_multiple_of(page);
                let end = (d.addr + u64::from(d.len)) & !(page - 1);
                if end > start && ctx.mem.check_range(vroom_memory::GuestAddress(start), end - start) {
                    runs.push((start, end - start));
                }
            }
            self.release(runs);
            ctx.add_used(q, head, 0);
        }
    }

    fn poked(&mut self, ctx: &mut QueueCtx) {
        if self.shared.config_changed.swap(false, Ordering::AcqRel) {
            ctx.irq.signal_config_changed();
        }
        if self.shared.want_stats.swap(false, Ordering::AcqRel) {
            // Linux puts its first statistics up before the device runs:
            // that kick went nowhere.
            if let Some(q) = self.stats.filter(|&q| self.stats_head.is_none() && ctx.queues[q].ready) {
                self.stats(ctx, q);
            }
            if let (Some(q), Some(head)) = (self.stats, self.stats_head.take()) {
                ctx.add_used(q, head, 0);
                ctx.flush();
            }
        }
    }
}

impl QueueHandler for Handler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        if !ctx.queues.get(q).is_some_and(|x| x.ready) {
            return;
        }
        match q {
            INFLATE => self.inflate(ctx),
            DEFLATE => self.deflate(ctx),
            q if Some(q) == self.stats => self.stats(ctx, q),
            q if Some(q) == self.report => self.report(ctx, q),
            _ => {}
        }
        ctx.flush();
    }

    fn attach(&mut self, poker: Poker) {
        *self.shared.poker.lock().unwrap_or_else(|e| e.into_inner()) = Some(poker);
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, token: usize) {
        if token == POKE_TOKEN || token == RESUME_TOKEN {
            self.poked(ctx);
        }
    }
}

impl VirtioDevice for Balloon {
    fn device_type(&self) -> u32 {
        types::BALLOON
    }
    fn name(&self) -> String {
        "balloon".into()
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        // inflate, deflate, stats, reporting (stats absent: reporting is 2).
        vec![QUEUE_SIZE; 4]
    }
    fn device_features(&self) -> u64 {
        feature::MUST_TELL_HOST | feature::STATS_VQ | feature::DEFLATE_ON_OOM | feature::REPORTING
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 16];
        cfg[0..4].copy_from_slice(&self.shared.target.load(Ordering::Acquire).to_le_bytes());
        cfg[4..8].copy_from_slice(&self.shared.actual.load(Ordering::Acquire).to_le_bytes());
        data.fill(0);
        if let Some(src) = cfg.get(offset as usize..) {
            let n = src.len().min(data.len());
            data[..n].copy_from_slice(&src[..n]);
        }
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // Only `actual` is the driver's to write.
        if offset == 4 && data.len() == 4 {
            self.shared.actual.store(u32::from_le_bytes([data[0], data[1], data[2], data[3]]), Ordering::Release);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        self.start(ctx, None)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.worker.stop();
        self.shared.running.store(false, Ordering::Release);
        self.shared.features.store(0, Ordering::Release);
        // The guest has all its memory again, and starts from there (a
        // reboot): the host asks anew.
        self.shared.actual.store(0, Ordering::Release);
        self.shared.target.store(0, Ordering::Release);
        *self.shared.stats.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.shared.poker.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        let (stats_head, inflated) = self.worker.with(|_, h| (h.stats_head, h.inflated.runs())).unwrap_or_default();
        let s = Saved { actual: self.shared.actual.load(Ordering::Acquire), stats_head, inflated };
        Ok(DeviceState { queues: self.worker.queues(), data: vroom_snapshot::encode(&s)? })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        let s: Saved = vroom_snapshot::decode(data)?;
        self.start(ctx, Some(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_page_goes_once_all_of_it_is_in() {
        let mut b = Inflated::new(&[(0x4000_0000, 1 << 20)], 16384);
        let first = 0x4000_0000 >> PFN_SHIFT;
        for p in first..first + 3 {
            b.set(p, true);
            assert_eq!(b.whole_host_page(p), None);
        }
        b.set(first + 3, true);
        assert_eq!(b.whole_host_page(first + 1), Some(first));
        b.set(first + 2, false);
        assert_eq!(b.whole_host_page(first), None);
        assert!(!b.set(0, true), "outside RAM");
        assert_eq!(b.runs(), vec![(first, 2), (first + 3, 1)]);
    }

    #[test]
    fn runs_merge() {
        let p = 0x4000_0000 >> PFN_SHIFT;
        assert_eq!(coalesce(vec![p + 8, p, p + 4, p + 16], 16384), vec![(0x4000_0000, 3 * 16384), (0x4001_0000, 16384)]);
    }

    #[test]
    fn stats_parse() {
        let mut b = Vec::new();
        for (t, v) in [(5u16, 100u64), (6, 40), (7, 30), (4, 10), (99, 1)] {
            b.extend_from_slice(&t.to_le_bytes());
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(Stats::parse(&b), Stats { total: 100, available: 40, caches: 30, free: 10, ..Default::default() });
    }
}
