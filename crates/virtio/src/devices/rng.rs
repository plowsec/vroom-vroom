//! virtio-rng: host CSPRNG for the guest.

use crate::worker::{QueueCtx, QueueHandler, WorkerSlot};
use crate::{types, ActivateContext, DeviceState, Result, VirtioDevice};

/// Per-request cap so one request can't make us generate gigabytes.
const MAX_REQUEST: usize = 64 << 10;

#[derive(Debug, Default)]
pub struct Rng {
    worker: WorkerSlot<RngHandler>,
}

impl Rng {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug)]
pub struct RngHandler;

impl QueueHandler for RngHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        loop {
            let chain = match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("rng: {e}");
                    break;
                }
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((_, mut w)) = chain.into_rw() {
                let n = w.available().min(MAX_REQUEST);
                let mut buf = vec![0u8; n];
                if getrandom::getrandom(&mut buf).is_ok() && w.write_all(&buf).is_ok() {
                    len = n as u32;
                }
            }
            ctx.add_used(q, head, len);
        }
    }
}

impl VirtioDevice for Rng {
    fn device_type(&self) -> u32 {
        types::RNG
    }
    fn name(&self) -> String {
        "rng".into()
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64]
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        self.worker.start("rng", ctx, RngHandler, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.worker.stop();
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        Ok(DeviceState { queues: self.worker.queues(), data: Vec::new() })
    }
    fn restore_active(&mut self, ctx: ActivateContext, _data: &[u8]) -> Result<()> {
        self.worker.start("rng", ctx, RngHandler, true)
    }
}
