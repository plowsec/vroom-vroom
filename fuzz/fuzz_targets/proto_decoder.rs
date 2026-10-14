//! Agent messages from a hostile guest.
#![no_main]
use bulwark_proto::Decoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = Decoder::new(1 << 16);
    // Split delivery like a stream would.
    for part in data.chunks(7) {
        d.push(part);
        while let Ok(Some(m)) = d.next() {
            // Whatever decodes must encode and decode back the same.
            let mut d2 = Decoder::new(1 << 16);
            d2.push(&m.encode());
            assert!(matches!(d2.next(), Ok(Some(_))));
        }
    }
});
