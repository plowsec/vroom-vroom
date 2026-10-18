//! USB passthrough protocol, both directions, and the helper's checks:
//! - a hostile VMM's byte stream, decoded and fed to the helper's session
//!   (in front of a test device, as with `VROOM_USB_FAKE=acm` or `audio`,
//!   or a USB stick on a small RAM disk: Bulk-Only and SCSI, as "Use as a
//!   disk" serves a Mac disk);
//! - a hostile helper's byte stream, decoded as events;
//! - device-supplied configuration descriptors.
#![no_main]
use std::sync::mpsc;

use libfuzzer_sys::fuzz_target;
use vroom_usb::desc::Config;
use vroom_usb::fake::{Acm, Audio};
use vroom_usb::helper::Device;
use vroom_usb::helper::Session;
use vroom_usb::msd::{Disk, Ids, Ram};
use vroom_usb::proto::{read_event, read_request, Request, Submit};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 5 {
        0 | 3 | 4 => {
            let (tx, rx) = mpsc::channel();
            let mut dev: Box<dyn Device> =
                match sel % 5 {
                    0 => Box::new(Acm::new(tx.clone(), 0x1209, 0x000a)),
                    3 => Box::new(Audio::new(tx.clone(), 0x1209, 0x0002, false)),
                    _ => Box::new(Disk::with_store(tx.clone(), Ids::test(0x1209, 0x00ab), Box::new(Ram(vec![0; 64 << 10])))),
                };
            let mut s = Session::new(tx);
            // Configured first, so bulk and interrupt endpoints exist.
            s.request(Some(&mut *dev), Request::Submit(Submit::control(0, [0, 9, 1, 0, 0, 0, 0, 0], vec![])));
            let mut r = rest;
            while let Ok(Some(req)) = read_request(&mut r) {
                s.request(Some(&mut *dev), req);
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
