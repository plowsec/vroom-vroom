//! virtio-net. One RX/TX pair, no offloads (keeps the frame path simple:
//! every frame is a plain, fully checksummed Ethernet frame).

use vroom_net::NetBackend;

use crate::worker::{QueueCtx, QueueHandler, WorkerSlot};
use crate::{types, ActivateContext, DeviceState, Result, VirtioDevice};

const F_MTU: u64 = 1 << 3;
const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;

const RX: usize = 0;
const TX: usize = 1;
const HDR_LEN: usize = 12;
const MTU: u16 = 1500;
/// Biggest frame we accept from the guest (no TSO, so MTU + header).
const MAX_FRAME: usize = 1514 + 4;

#[derive(Debug)]
pub struct Net {
    id: String,
    mac: [u8; 6],
    /// Backend lives here while inactive, in the worker while active.
    backend: Option<Box<dyn NetBackend>>,
    worker: WorkerSlot<NetHandler>,
}

impl Net {
    pub fn new(id: &str, mac: [u8; 6], backend: Box<dyn NetBackend>) -> Self {
        Net { id: id.to_string(), mac, backend: Some(backend), worker: WorkerSlot::default() }
    }

    fn start(&mut self, ctx: ActivateContext, paused: bool) -> Result<()> {
        let backend = match self.backend.take() {
            Some(b) => b,
            None => return Err(crate::Error::Device("net backend already in use".into())),
        };
        self.worker.start(&format!("net-{}", self.id), ctx, NetHandler { backend: Some(backend), pending: None }, paused)
    }

    /// Get the backend back from a stopped worker. The worker deregisters
    /// the backend's sources on its way out, so take it only afterwards.
    fn stop(&mut self) {
        let b = self.worker.stop_with(|h| h.backend.take()).flatten();
        if b.is_some() {
            self.backend = b;
        }
    }
}

#[derive(Debug)]
pub struct NetHandler {
    backend: Option<Box<dyn NetBackend>>,
    /// Frame waiting for an RX buffer.
    pending: Option<Vec<u8>>,
}

impl NetHandler {
    fn tx(&mut self, ctx: &mut QueueCtx) {
        let Some(be) = self.backend.as_mut() else { return };
        // Everything the guest queued goes to the backend as one batch.
        let mut batch = Vec::new();
        loop {
            let chain = match ctx.queues[TX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("net tx: {e}");
                    break;
                }
            };
            let head = chain.head();
            if let Ok((mut r, _)) = chain.into_rw() {
                let n = r.available();
                if n > HDR_LEN && n <= HDR_LEN + MAX_FRAME {
                    let mut hdr = [0u8; HDR_LEN];
                    let mut frame = vec![0u8; n - HDR_LEN];
                    if r.read_exact(&mut hdr).is_ok() && r.read_exact(&mut frame).is_ok() {
                        batch.push(frame);
                    }
                }
            }
            ctx.add_used(TX, head, 0);
        }
        if !batch.is_empty() {
            be.send_frames(&batch);
        }
    }

    fn rx(&mut self, ctx: &mut QueueCtx) {
        let Some(be) = self.backend.as_mut() else { return };
        while let Some(frame) = self.pending.take().or_else(|| be.recv_frame()) {
            let chain = match ctx.queues[RX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    // No buffers: hold the frame until the driver adds some.
                    self.pending = Some(frame);
                    break;
                }
                Err(e) => {
                    log::warn!("net rx: {e}");
                    break;
                }
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((_, mut w)) = chain.into_rw() {
                if w.available() >= HDR_LEN + frame.len() {
                    let mut hdr = [0u8; HDR_LEN];
                    hdr[10] = 1; // num_buffers
                    if w.write_all(&hdr).is_ok() && w.write_all(&frame).is_ok() {
                        len = w.written() as u32;
                    }
                }
            }
            ctx.add_used(RX, head, len);
        }
    }
}

impl QueueHandler for NetHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        if q == TX {
            self.tx(ctx);
        }
        self.rx(ctx);
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, token: usize) {
        if let Some(be) = self.backend.as_mut() {
            be.ready(token);
        }
        self.rx(ctx);
    }

    fn register(&mut self, reg: &mio::Registry) -> std::io::Result<()> {
        match self.backend.as_mut() {
            Some(be) => be.register(reg),
            None => Ok(()),
        }
    }

    fn deregister(&mut self, reg: &mio::Registry) {
        if let Some(be) = self.backend.as_mut() {
            be.deregister(reg);
        }
    }

    fn tick_interval(&mut self) -> Option<std::time::Duration> {
        self.backend.as_mut().and_then(|b| b.poll_delay())
    }

    fn tick(&mut self, ctx: &mut QueueCtx) {
        if let Some(be) = self.backend.as_mut() {
            be.tick();
        }
        self.rx(ctx);
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        types::NET
    }
    fn name(&self) -> String {
        format!("net-{}", self.id)
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 256]
    }
    fn device_features(&self) -> u64 {
        F_MAC | F_STATUS | F_MTU | crate::features::RING_EVENT_IDX | crate::features::RING_INDIRECT_DESC
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut c = [0u8; 12];
        c[0..6].copy_from_slice(&self.mac);
        c[6..8].copy_from_slice(&1u16.to_le_bytes()); // LINK_UP
        c[8..10].copy_from_slice(&1u16.to_le_bytes());
        c[10..12].copy_from_slice(&MTU.to_le_bytes());
        let o = offset as usize;
        data.fill(0);
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        self.start(ctx, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.stop();
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        // Connection state (NAT tables, host sockets) can't be snapshotted;
        // TCP sessions reset after restore, like after a cable unplug.
        Ok(DeviceState { queues: self.worker.queues(), data: Vec::new() })
    }
    fn restore_active(&mut self, ctx: ActivateContext, _data: &[u8]) -> Result<()> {
        self.start(ctx, true)
    }
}
