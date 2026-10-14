//! Per-device worker thread.
//!
//! vCPU threads only flip a bit and wake the worker; all ring processing
//! and backend I/O happens here. The worker holds its lock while touching
//! guest memory, so `pause()` returning means "no DMA in flight".

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use bulwark_memory::GuestMemory;
use mio::{Events, Poll, Registry, Token, Waker};

use crate::{ActivateContext, Queue, Result, VirtioInterrupt};

const WAKE: Token = Token(usize::MAX);
/// Tokens below this are reserved; backends use `BACKEND_TOKEN_BASE + n`.
pub const BACKEND_TOKEN_BASE: usize = 64;
/// Passed to `backend_ready` after a resume: re-check every backend source.
pub const RESUME_TOKEN: usize = usize::MAX - 1;

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
}

struct Inner<H> {
    paused: bool,
    ctx: QueueCtx,
    handler: H,
}

struct Shared<H> {
    inner: Mutex<Inner<H>>,
    kicks: AtomicU64,
    stop: AtomicBool,
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
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner { paused, ctx, handler }),
            // Process everything once at start: the driver may have queued
            // buffers before we came up (or before a snapshot).
            kicks: AtomicU64::new(if nq >= 64 { u64::MAX } else { (1u64 << nq) - 1 }),
            stop: AtomicBool::new(false),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("virtio-{name}"))
            .spawn(move || run(poll, s))?;
        waker.wake()?;
        Ok(Worker { shared, waker, thread: Some(thread) })
    }

    pub fn kick(&self, q: u16) {
        if q < 64 {
            self.shared.kicks.fetch_or(1 << q, Ordering::AcqRel);
            let _ = self.waker.wake();
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<H>> {
        self.shared.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// When this returns, the worker is idle and won't touch guest memory.
    pub fn pause(&self) {
        self.lock().paused = true;
    }

    pub fn resume(&self) {
        self.lock().paused = false;
        self.shared.kicks.fetch_or(u64::MAX, Ordering::AcqRel);
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
        self.shared.stop.store(true, Ordering::Release);
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
        if s.stop.load(Ordering::Acquire) {
            let mut g = s.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.handler.deregister(poll.registry());
            return;
        }
        let mut g = s.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *g;
        if inner.paused {
            was_paused = true;
            continue;
        }
        let kicks = s.kicks.swap(0, Ordering::AcqRel);
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

    pub fn queues(&self) -> Vec<Queue> {
        self.worker.as_ref().map(|w| w.with(|c, _| c.queues.clone())).unwrap_or_default()
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut QueueCtx, &mut H) -> R) -> Option<R> {
        self.worker.as_ref().map(|w| w.with(f))
    }
}
