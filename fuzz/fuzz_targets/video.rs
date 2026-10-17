//! Video decoding: what a guest's bitstream does to the helper's parsing
//! (NAL units, access units, parameter sets, display order; VP9 frame
//! headers and superframes; AV1 OBUs and sequence headers) before
//! VideoToolbox sees it, the encoder's output turned into Annex B, and
//! what either side of the helper protocol (decoder and encoder messages)
//! can send the other.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vroom_video::nal::{self, ParamSets};
use vroom_video::poc::{Poc, Reorder};
use vroom_video::proto::{FromHelper, ToHelper};
use vroom_video::vpx;
use vroom_video::Codec;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 5 {
        0 | 1 => {
            let codec = if sel % 5 == 0 { Codec::H264 } else { Codec::Hevc };
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
                let _ = sets.depth();
            }
            order.flush();
        }
        2 => {
            let mut refs = [(0, 0); 8];
            for chunk in rest.chunks(512) {
                for f in vpx::vp9_frames(chunk) {
                    if let Some(c) = vpx::vp9_header(f, &mut refs).and_then(|h| h.config) {
                        let _ = vpx::vpcc(&c);
                    }
                }
            }
        }
        3 => {
            for o in vpx::av1_obus(rest) {
                assert!(o.payload <= o.data.len());
                if let Some(c) = vpx::av1_sequence(&o.data[o.payload..]) {
                    let _ = vpx::av1c(&c, o.data);
                }
            }
            // The encoder's output: length-prefixed NAL units to Annex B.
            // What comes out splits back into the same units.
            let len_size = 1 + (sel as usize / 5) % 4;
            if let Some(out) = nal::annexb(&[], rest, len_size, 1 << 20) {
                assert!(out.len() <= 1 << 20);
                let _ = nal::split_annexb(&out);
            }
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
