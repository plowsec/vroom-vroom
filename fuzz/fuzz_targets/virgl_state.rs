//! The GL state tracker on a hostile command stream: what a guest
//! submits to a GL context, followed, then turned back into commands.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vroom_virtio::devices::virgl_state::GlState;

fuzz_target!(|data: &[u8]| {
    let words: Vec<u32> = data.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let mut s = GlState::default();
    // In two submits, like a guest flushing halfway.
    let (a, b) = words.split_at(words.len() / 2);
    s.follow(a);
    s.follow(b);
    let fetch = s.fetch_query_results();
    let out = s.replay(&|r| r % 2 == 0);
    // What comes out is whole commands.
    for stream in [&fetch, &out] {
        let mut i = 0;
        while i < stream.len() {
            i += 1 + (stream[i] >> 16) as usize;
        }
        assert_eq!(i, stream.len());
    }
});
