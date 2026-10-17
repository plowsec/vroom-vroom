//! virtio-media from a hostile driver: V4L2 ioctls on the command queue
//! (formats, buffers, queues, controls, decoder and encoder commands) for
//! the decoder or the encoder, then what a hostile video helper sends
//! back, then a snapshot of the result restored in a new device.
#![no_main]
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use vroom_memory::{GuestAddress, GuestMemory};
use vroom_video::proto::{FromHelper, ToHelper};
use vroom_virtio::devices::media::{HelperLink, Media, Role, WindowPlacer, WINDOW};
use vroom_virtio::worker::Poker;
use vroom_virtio::{ActivateContext, Queue, VirtioInterrupt};

struct Link;
impl HelperLink for Link {
    fn send(&self, m: &ToHelper) -> bool {
        // What we would send must parse on the other side.
        assert!(ToHelper::read_from(&mut &m.encode()[..]).is_ok(), "{m:?}");
        true
    }
    fn try_recv(&self) -> Option<FromHelper> {
        None
    }
    fn set_poker(&self, _: Poker) {}
    fn alive(&self) -> bool {
        true
    }
}

struct Nowhere;
impl WindowPlacer for Nowhere {
    fn place(&self, _: u64) {}
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
    let [sel, len, rest @ ..] = data else { return };
    // Guest memory (up to 64 KiB of it), then the helper's messages.
    let split = rest.len().min((*len as usize + 1) * 256);
    let (guest, helper) = rest.split_at(split);
    let sel = *sel;
    let mem = GuestMemory::new(&[(GuestAddress(0), MEM)]).unwrap();
    mem.write(GuestAddress(0), guest).unwrap();
    let window = GuestMemory::new(&[(GuestAddress(0), WINDOW)]).unwrap();
    let role = if sel & 1 == 0 { Role::Decoder } else { Role::Encoder };
    let m = Media::new(Arc::new(Link), u32::MAX, window, Arc::new(Nowhere), role);
    let size = 1 << (sel % 5);
    let ctx = ActivateContext { mem, queues: (0..2).map(|i| queue(i * 0x400, size)).collect(), irq: Arc::new(NoIrq), features: 0 };
    m.fuzz_queues(ctx, helper);
});
