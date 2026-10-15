//! The viewer protocol in both directions. Viewer side matters most: the
//! unsandboxed GUI decodes what the (guest-exposed) VMM sends.
#![no_main]
use vroom_display::Decoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (mut a, mut b) = (Decoder::default(), Decoder::default());
    for part in data.chunks(13) {
        a.push(part);
        b.push(part);
        while let Ok(Some(m)) = a.next_to_viewer() {
            let mut d = Decoder::default();
            d.push(&m.encode());
            assert!(matches!(d.next_to_viewer(), Ok(Some(_))));
        }
        while let Ok(Some(m)) = b.next_to_vmm() {
            let mut d = Decoder::default();
            d.push(&m.encode());
            assert!(matches!(d.next_to_vmm(), Ok(Some(_))));
        }
    }
});
