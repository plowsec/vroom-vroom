//! Per-device worker thread.
//!
//! vCPU threads only flip a bit and wake the worker; all ring processing
//! and backend I/O happens here. The worker holds its lock while touching
//! guest memory, so `pause()` returning means "no DMA in flight".

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use vroom_memory::GuestMemory;
use mio::{Events, Poll, Registry, Token, Waker};

use crate::{ActivateContext, Queue, Result, VirtioInterrupt};

const WAKE: Token = Token(usize::MAX);
/// Tokens below this are reserved; backends use `BACKEND_TOKEN_BASE + n`.
pub const BACKEND_TOKEN_BASE: usize = 64;
/// Passed to `backend_ready` after a resume: re-check every backend source.
pub const RESUME_TOKEN: usize = usize::MAX - 1;
/// Passed to `backend_ready` after a [`Poker::poke`].
pub const POKE_TOKEN: usize = usize::MAX - 2;

/// Queues + memory + interrupt, as seen by a handler.
pub struct QueueCtx {
    pub mem: GuestMemory,
    pub queues: Vec<Queue>,
    pub irq: Arc<dyn VirtioInterrupt>,
    pub features: u64,
    /// Queues with used entries not yet signalled.
    pending_irq: u64,
}

impl std::fmt::Debug for QueueCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueCtx").field("queues", &self.queues).finish_non_exhaustive()
    }
}

impl QueueCtx {
    pub fn new(ctx: ActivateContext) -> Self {
        QueueCtx { mem: ctx.mem, queues: ctx.queues, irq: ctx.irq, features: ctx.features, pending_irq: 0 }
    }

    /// Return a buffer to the driver. Interrupts are batched until `flush`.
    pub fn add_used(&mut self, q: usize, head: u16, len: u32) {
        let Some(queue) = self.queues.get_mut(q) else { return };
        if let Err(e) = queue.add_used(&self.mem, head, len) {
            log::warn!("add_used q{q}: {e}");
            return;
        }
        self.pending_irq |= 1 << q;
    }

    /// Send interrupts for queues that got used entries (respecting
    /// the driver's suppression flags).
    pub fn flush(&mut self) {
        let mut p = std::mem::take(&mut self.pending_irq);
        while p != 0 {
            let q = p.trailing_zeros() as usize;
            p &= p - 1;
            let Some(queue) = self.queues.get_mut(q) else { continue };
            match queue.needs_notification(&self.mem) {
                Ok(true) => self.irq.signal_used_queue(q as u16),
                Ok(false) => {}
                Err(e) => log::warn!("notify q{q}: {e}"),
            }
        }
    }
}

/// Device-specific processing, run on the worker thread.
pub trait QueueHandler: Send + 'static {
    /// The driver kicked queue `q` (or we're resuming).
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize);
    /// A backend source registered in `register` is ready.
    fn backend_ready(&mut self, _ctx: &mut QueueCtx, _token: usize) {}
    /// Register backend sources (tokens >= BACKEND_TOKEN_BASE).
    fn register(&mut self, _reg: &Registry) -> io::Result<()> {
        Ok(())
    }
    /// The worker stops (device reset, VM teardown): undo `register`, or
    /// the next activation can't register the same sources again.
    fn deregister(&mut self, _reg: &Registry) {}
    /// Optional periodic tick (e.g. rate limiting, retries).
    fn tick_interval(&mut self) -> Option<Duration> {
        None
    }
    fn tick(&mut self, _ctx: &mut QueueCtx) {}
    /// Called once before the worker starts: keep the poker to wake the
    /// worker from other threads (`backend_ready(POKE_TOKEN)` follows).
    fn attach(&mut self, _poker: Poker) {}
    /// Run queue `q` on the thread that kicked it (a vCPU), instead of
    /// waking the worker. False if the handler doesn't do that.
    fn process_queue_inline(&mut self, _ctx: &mut QueueCtx, _q: usize) -> bool {
        false
    }
    /// The worker is being paused: finish whatever runs on other threads
    /// and hand back its buffers, so nothing touches guest memory until
    /// the resume. Runs on the pausing thread, with the worker idle.
    fn quiesce(&mut self, _ctx: &mut QueueCtx) {}
    /// On the worker thread, asked by [`WorkerSlot::release_thread`]:
    /// let go of what is bound to this thread (a GL context), so another
    /// thread can use it while the worker is paused.
    fn release_thread(&mut self) {}
}

struct Inner<H> {
    paused: bool,
    ctx: QueueCtx,
    handler: H,
}

/// Notifications and pokes no worker of this process has handled yet.
static OUTSTANDING: AtomicUsize = AtomicUsize::new(0);

/// A device has guest notifications or I/O completions it hasn't handled
/// yet: the guest is waiting on the host, not on time (the idle skip).
pub fn busy() -> bool {
    OUTSTANDING.load(Ordering::Acquire) != 0
}

/// What other threads set to get the worker's attention.
struct Signals {
    /// This worker's share of `OUTSTANDING`.
    outstanding: AtomicUsize,
    kicks: AtomicU64,
    poked: AtomicBool,
    stop: AtomicBool,
    /// [`Worker::release_thread`] asks; the worker counts the times it did.
    release: AtomicBool,
    released: (Mutex<u64>, std::sync::Condvar),
}

impl Signals {
    fn pending(&self) {
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        OUTSTANDING.fetch_add(1, Ordering::AcqRel);
    }

    /// `n` handled (or dropped with the worker).
    fn handled(&self, n: usize) {
        if n != 0 {
            OUTSTANDING.fetch_sub(n, Ordering::AcqRel);
        }
    }
}

struct Shared<H> {
    inner: Mutex<Inner<H>>,
    sig: Arc<Signals>,
}

/// Wakes a worker from another thread (a device's I/O helpers).
#[derive(Clone)]
pub struct Poker {
    sig: Arc<Signals>,
    waker: Arc<Waker>,
}

impl std::fmt::Debug for Poker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Poker")
    }
}

impl Poker {
    pub fn poke(&self) {
        self.sig.pending();
        self.sig.poked.store(true, Ordering::Release);
        let _ = self.waker.wake();
    }
}

/// Handle to a running worker.
pub struct Worker<H: QueueHandler> {
    shared: Arc<Shared<H>>,
    waker: Arc<Waker>,
    thread: Option<JoinHandle<()>>,
}

impl<H: QueueHandler> std::fmt::Debug for Worker<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

impl<H: QueueHandler> Worker<H> {
    pub fn spawn(name: &str, ctx: QueueCtx, mut handler: H, paused: bool) -> Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        handler.register(poll.registry())?;
        let nq = ctx.queues.len();
        let sig = Arc::new(Signals {
            // Process everything once at start: the driver may have queued
            // buffers before we came up (or before a snapshot).
            outstanding: AtomicUsize::new(0),
            kicks: AtomicU64::new(if nq >= 64 { u64::MAX } else { (1u64 << nq) - 1 }),
            poked: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            release: AtomicBool::new(false),
            released: (Mutex::new(0), std::sync::Condvar::new()),
        });
        handler.attach(Poker { sig: sig.clone(), waker: waker.clone() });
        let shared = Arc::new(Shared { inner: Mutex::new(Inner { paused, ctx, handler }), sig });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("virtio-{name}"))
            .spawn(move || run(poll, s))?;
        waker.wake()?;
        Ok(Worker { shared, waker, thread: Some(thread) })
    }

    pub fn kick(&self, q: u16) {
        if q < 64 {
            self.shared.sig.pending();
            self.shared.sig.kicks.fetch_or(1 << q, Ordering::AcqRel);
            let _ = self.waker.wake();
        }
    }

    /// Let the handler run queue `q` here and now if the worker is idle
    /// (not busy, not paused). False: kick it instead.
    pub fn try_inline(&self, q: u16) -> bool {
        let Ok(mut g) = self.shared.inner.try_lock() else { return false };
        let inner = &mut *g;
        if inner.paused || (q as usize) >= inner.ctx.queues.len() {
            return false;
        }
        if !inner.handler.process_queue_inline(&mut inner.ctx, q as usize) {
            return false;
        }
        inner.ctx.flush();
        true
    }

    fn lock(&self) -> MutexGuard<'_, Inner<H>> {
        self.shared.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Have the worker thread run `release_thread` (paused or not), and
    /// wait for it. False if it didn't within a few seconds.
    pub fn release_thread(&self) -> bool {
        let (count, cv) = &self.shared.sig.released;
        let before = *count.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.sig.release.store(true, Ordering::Release);
        let _ = self.waker.wake();
        let g = count.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = cv.wait_timeout_while(g, Duration::from_secs(5), |n| *n == before).unwrap_or_else(|e| e.into_inner());
        *g != before
    }

    /// When this returns, the worker is idle and won't touch guest memory.
    pub fn pause(&self) {
        let mut g = self.lock();
        g.paused = true;
        let inner = &mut *g;
        inner.handler.quiesce(&mut inner.ctx);
        inner.ctx.flush();
    }

    pub fn resume(&self) {
        self.lock().paused = false;
        self.shared.sig.kicks.fetch_or(u64::MAX, Ordering::AcqRel);
        let _ = self.waker.wake();
    }

    /// Inspect state (snapshots). Pause first for a consistent view.
    pub fn with<R>(&self, f: impl FnOnce(&mut QueueCtx, &mut H) -> R) -> R {
        let mut g = self.lock();
        let inner = &mut *g;
        f(&mut inner.ctx, &mut inner.handler)
    }

    /// Stop the thread; returns final queue state.
    pub fn stop(mut self) -> Vec<Queue> {
        self.shutdown();
        self.lock().ctx.queues.clone()
    }

    /// Stop the thread, then let `f` take what it needs from the handler.
    pub fn stop_with<R>(mut self, f: impl FnOnce(&mut H) -> R) -> R {
        self.shutdown();
        f(&mut self.lock().handler)
    }

    fn shutdown(&mut self) {
        self.shared.sig.stop.store(true, Ordering::Release);
        let _ = self.waker.wake();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl<H: QueueHandler> Drop for Worker<H> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run<H: QueueHandler>(mut poll: Poll, s: Arc<Shared<H>>) {
    let mut events = Events::with_capacity(64);
    let mut was_paused = false;
    loop {
        let interval = s.inner.lock().unwrap_or_else(|e| e.into_inner()).handler.tick_interval();
        if let Err(e) = poll.poll(&mut events, interval) {
            if e.kind() != io::ErrorKind::Interrupted {
                log::error!("worker poll: {e}");
                return;
            }
        }
        if s.sig.stop.load(Ordering::Acquire) {
            let mut g = s.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.handler.deregister(poll.registry());
            s.sig.handled(s.sig.outstanding.swap(0, Ordering::AcqRel));
            return;
        }
        let mut g = s.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *g;
        if s.sig.release.swap(false, Ordering::AcqRel) {
            inner.handler.release_thread();
            let (count, cv) = &s.sig.released;
            *count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            cv.notify_all();
        }
        if inner.paused {
            was_paused = true;
            continue;
        }
        let taken = s.sig.outstanding.swap(0, Ordering::AcqRel);
        if s.sig.poked.swap(false, Ordering::AcqRel) {
            inner.handler.backend_ready(&mut inner.ctx, POKE_TOKEN);
        }
        let kicks = s.sig.kicks.swap(0, Ordering::AcqRel);
        let nq = inner.ctx.queues.len();
        for q in 0..nq.min(64) {
            if kicks & (1 << q) != 0 {
                inner.handler.process_queue(&mut inner.ctx, q);
            }
        }
        if was_paused {
            was_paused = false;
            inner.handler.backend_ready(&mut inner.ctx, RESUME_TOKEN);
        }
        for ev in events.iter() {
            if ev.token() != WAKE {
                inner.handler.backend_ready(&mut inner.ctx, ev.token().0);
            }
        }
        if interval.is_some() {
            inner.handler.tick(&mut inner.ctx);
        }
        inner.ctx.flush();
        s.sig.handled(taken);
    }
}

/// Optional running worker, with the boilerplate every device front-end needs.
pub struct WorkerSlot<H: QueueHandler> {
    worker: Option<Worker<H>>,
}

impl<H: QueueHandler> Default for WorkerSlot<H> {
    fn default() -> Self {
        WorkerSlot { worker: None }
    }
}

impl<H: QueueHandler> std::fmt::Debug for WorkerSlot<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerSlot").field("running", &self.worker.is_some()).finish()
    }
}

impl<H: QueueHandler> WorkerSlot<H> {
    pub fn start(&mut self, name: &str, ctx: ActivateContext, handler: H, paused: bool) -> Result<()> {
        self.stop();
        self.worker = Some(Worker::spawn(name, QueueCtx::new(ctx), handler, paused)?);
        Ok(())
    }

    pub fn notify(&self, q: u16) {
        if let Some(w) = &self.worker {
            w.kick(q);
        }
    }

    /// Run queue `q` on the calling thread if the handler can, else kick.
    pub fn notify_inline(&self, q: u16) {
        if let Some(w) = &self.worker {
            if !w.try_inline(q) {
                w.kick(q);
            }
        }
    }

    pub fn stop(&mut self) -> Vec<Queue> {
        self.worker.take().map(|w| w.stop()).unwrap_or_default()
    }

    pub fn stop_with<R>(&mut self, f: impl FnOnce(&mut H) -> R) -> Option<R> {
        self.worker.take().map(|w| w.stop_with(f))
    }

    pub fn pause(&self) {
        if let Some(w) = &self.worker {
            w.pause();
        }
    }

    pub fn resume(&self) {
        if let Some(w) = &self.worker {
            w.resume();
        }
    }

    /// See [`Worker::release_thread`]. True with no worker.
    pub fn release_thread(&self) -> bool {
        self.worker.as_ref().is_none_or(|w| w.release_thread())
    }

    pub fn queues(&self) -> Vec<Queue> {
        self.worker.as_ref().map(|w| w.with(|c, _| c.queues.clone())).unwrap_or_default()
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut QueueCtx, &mut H) -> R) -> Option<R> {
        self.worker.as_ref().map(|w| w.with(f))
    }
}
