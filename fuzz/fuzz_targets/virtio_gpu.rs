//! virtio-gpu 2D commands from a hostile driver: control and cursor
//! queues, descriptors, resources and backing pages all from the input.
#![no_main]
use std::sync::Arc;

use vroom_memory::{GuestAddress, GuestMemory};
use vroom_virtio::devices::gpu::{DisplaySink, Gpu};
use vroom_virtio::{ActivateContext, Queue, VirtioInterrupt};
use libfuzzer_sys::fuzz_target;

struct Null;
impl DisplaySink for Null {
    fn set_mode(&self, _w: u32, _h: u32) {}
    fn flush(&self, _x: u32, _y: u32, _w: u32, _h: u32, rows: &[u8], stride: usize) {
        // Touch what we were given, like a real sink copying it.
        std::hint::black_box(rows.iter().step_by(stride.max(1)).count());
    }
}
struct NoIrq;
impl VirtioInterrupt for NoIrq {
    fn signal_used_queue(&self, _q: u16) {}
    fn signal_config_changed(&self) {}
}

const MEM: u64 = 1 << 20;

/// Avail ring first and descriptors right after, so the fuzzer reaches
/// them with short inputs; the used ring lies outside the input.
fn queue(base: u64, size: u16, next_avail: u16) -> Queue {
    let mut q = Queue::new(16);
    q.size = size;
    q.avail_ring = base;
    q.desc_table = base + 0x40;
    q.used_ring = 0xf_0000 + base;
    q.ready = true;
    q.next_avail = next_avail;
    q
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let mem = GuestMemory::new(&[(GuestAddress(0), MEM)]).unwrap();
    let n = (data.len() - 2).min(MEM as usize);
    mem.write(GuestAddress(0), &data[2..2 + n]).unwrap();
    let size = 1 << (data[0] % 5); // 1..16
    // Two scanouts: the multi-monitor paths too.
    let (gpu, _ctl) = Gpu::with_heads(&[(64, 64), (32, 32)], vec![Arc::new(Null), Arc::new(Null)]);
    let ctx = ActivateContext {
        mem,
        queues: vec![queue(0, size, data[1] as u16 % 4), queue(0x200, size, 0)],
        irq: Arc::new(NoIrq),
        features: 0,
    };
    gpu.fuzz_queues(ctx);
});
