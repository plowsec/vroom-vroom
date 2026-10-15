//! A guest scribbling over its rings: walk every chain the avail ring
//! offers, read and write through it, complete it.
#![no_main]
use vroom_memory::{GuestAddress, GuestMemory};
use vroom_virtio::Queue;
use libfuzzer_sys::fuzz_target;

const MEM: u64 = 64 << 10;

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let mem = GuestMemory::new(&[(GuestAddress(0), MEM)]).unwrap();
    let n = (data.len() - 4).min(MEM as usize);
    mem.write(GuestAddress(0), &data[4..4 + n]).unwrap();
    // Avail ring first, descriptors after: short inputs reach both.
    let mut q = Queue::new(64);
    q.size = 1 << (data[0] % 7); // 1..64
    q.avail_ring = 0;
    q.desc_table = 0x100;
    q.used_ring = 0xc000;
    q.ready = true;
    q.indirect = data[1] & 1 != 0;
    q.event_idx = data[1] & 2 != 0;
    q.next_avail = u16::from_le_bytes([data[2], data[3]]);
    if !q.is_valid(&mem) {
        return;
    }
    for _ in 0..512 {
        let Ok(Some(chain)) = q.pop(&mem) else { break };
        let head = chain.head();
        if let Ok((mut r, mut w)) = chain.into_rw() {
            let _ = r.read_to_vec();
            let _ = w.write_all(&[0xa5; 300]);
        }
        let _ = q.add_used(&mem, head, 0);
        let _ = q.needs_notification(&mem);
    }
});
