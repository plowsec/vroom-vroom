//! virtio-snd from a hostile driver: control requests (params, state
//! changes in any order) and PCM transfers both ways (with or without a
//! capture stream), all from guest memory.
#![no_main]
use std::sync::Arc;

use vroom_memory::{GuestAddress, GuestMemory};
use vroom_virtio::devices::sound::{AudioSink, AudioSource, Sound};
use vroom_virtio::{ActivateContext, Queue, VirtioInterrupt};
use libfuzzer_sys::fuzz_target;

struct Null;
impl AudioSink for Null {
    fn start(&mut self, _r: u32, _c: u8) {}
    fn write(&mut self, pcm: &[u8]) {
        std::hint::black_box(pcm.len());
    }
    fn stop(&mut self) {}
}
struct Mic;
impl AudioSource for Mic {
    fn start(&mut self, _r: u32, _c: u8) {}
    fn read(&mut self, out: &mut [u8]) {
        out.fill(1);
    }
    fn stop(&mut self) {}
}
struct NoIrq;
impl VirtioInterrupt for NoIrq {
    fn signal_used_queue(&self, _q: u16) {}
    fn signal_config_changed(&self) {}
}

const MEM: u64 = 1 << 20;

fn queue(base: u64, size: u16) -> Queue {
    let mut q = Queue::new(16);
    q.size = size;
    q.avail_ring = base;
    q.desc_table = base + 0x40;
    q.used_ring = 0xf_0000 + base;
    q.ready = true;
    q
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let mem = GuestMemory::new(&[(GuestAddress(0), MEM)]).unwrap();
    let n = (data.len() - 1).min(MEM as usize);
    mem.write(GuestAddress(0), &data[1..1 + n]).unwrap();
    let size = 1 << (data[0] % 5);
    let mut s = Sound::new(Box::new(|| Box::new(Null) as Box<dyn AudioSink>));
    if data[0] & 0x80 != 0 {
        s = s.with_capture(Box::new(|| Box::new(Mic) as Box<dyn AudioSource>));
    }
    let ctx = ActivateContext {
        mem,
        queues: (0..4).map(|i| queue(i * 0x200, size)).collect(),
        irq: Arc::new(NoIrq),
        features: 0,
    };
    s.fuzz_queues(ctx);
});
