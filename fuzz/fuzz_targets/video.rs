//! Video decoding: what a guest's bitstream does to the helper's parsing
//! (NAL units, access units, parameter sets, display order) before
//! VideoToolbox sees it, and what either side of the helper protocol can
//! send the other.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vroom_video::nal::{self, ParamSets};
use vroom_video::poc::{Poc, Reorder};
use vroom_video::proto::{FromHelper, ToHelper};
use vroom_video::Codec;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 3 {
        0 | 1 => {
            let codec = if sel % 3 == 0 { Codec::H264 } else { Codec::Hevc };
            let mut sets = ParamSets::new(codec);
            let mut poc = Poc::new(codec);
            let mut order = Reorder::default();
            // A stream in a few buffers.
            for chunk in rest.chunks(512) {
                let p = nal::parse(codec, chunk);
                for &(_, kind, id, n) in &p.params {
                    sets.update(kind, id, n);
                    poc.param(kind, n);
                }
                for au in &p.units {
                    let _ = nal::length_prefixed(au);
                    if let Some(pic) = poc.picture(au) {
                        if pic.reset {
                            order.flush();
                        }
                        order.push(pic.poc, pic.reorder, ());
                    }
                }
                let _ = sets.complete();
                let _ = sets.list();
            }
            order.flush();
        }
        _ => {
            let mut r = rest;
            while let Ok(m) = ToHelper::read_from(&mut r) {
                assert_eq!(ToHelper::read_from(&mut &m.encode()[..]).ok(), Some(m));
            }
            let mut r = rest;
            while let Ok(m) = FromHelper::read_from(&mut r) {
                assert_eq!(FromHelper::read_from(&mut &m.encode()[..]).ok(), Some(m));
            }
        }
    }
});
