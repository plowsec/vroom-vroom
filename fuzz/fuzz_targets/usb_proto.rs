//! USB passthrough protocol, both directions, and the helper's checks:
//! - a hostile VMM's byte stream, decoded and fed to the helper's session
//!   (in front of the test device, as in `VROOM_USB_FAKE=acm`);
//! - a hostile helper's byte stream, decoded as events;
//! - device-supplied configuration descriptors.
#![no_main]
use std::sync::mpsc;

use libfuzzer_sys::fuzz_target;
use vroom_usb::desc::Config;
use vroom_usb::fake::Acm;
use vroom_usb::helper::Session;
use vroom_usb::proto::{read_event, read_request, Request, Submit};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 3 {
        0 => {
            let (tx, rx) = mpsc::channel();
            let mut dev = Acm::new(tx.clone(), 0x1209, 0x000a);
            let mut s = Session::new(tx);
            // Configured first, so bulk and interrupt endpoints exist.
            s.request(Some(&mut dev), Request::Submit(Submit::control(0, [0, 9, 1, 0, 0, 0, 0, 0], vec![])));
            let mut r = rest;
            while let Ok(Some(req)) = read_request(&mut r) {
                s.request(Some(&mut dev), req);
                while rx.try_recv().is_ok() {}
            }
        }
        1 => {
            let mut r = rest;
            while let Ok(Some(ev)) = read_event(&mut r) {
                // Whatever decodes must encode back to the same thing.
                let b = ev.encode();
                assert_eq!(read_event(&mut &b[..]).ok().flatten(), Some(ev));
            }
        }
        _ => {
            if let Some(c) = Config::parse(rest) {
                for i in c.interfaces() {
                    let _ = c.alt(i, 0);
                }
            }
        }
    }
});
