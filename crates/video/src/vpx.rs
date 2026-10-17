//! VP9 and AV1 as a guest sends them, taken apart before VideoToolbox
//! sees them.
//!
//! VP9: one buffer is a frame or a superframe (frames with an index at
//! the end). We read each frame's uncompressed header up to its size,
//! which tells the profile, bit depth and chroma layout VideoToolbox
//! needs to start a session, and whether the frame is shown.
//!
//! AV1: one buffer is a temporal unit, a run of OBUs. We read the OBU
//! headers, keep the sequence header (VideoToolbox gets it up front) and
//! drop temporal delimiters and padding.
//!
//! All of it is guest data: every read is checked, nothing here can panic
//! on any input.

use crate::nal::Bits;

/// Frames in one VP9 superframe (the format's own limit).
pub const MAX_SUPERFRAME: usize = 8;
/// Most OBUs we take from one AV1 buffer.
pub const MAX_OBUS: usize = 1024;
/// Biggest AV1 sequence header we keep.
pub const MAX_SEQUENCE_HEADER: usize = 1024;

/// What a stream needs from VideoToolbox: a session made for one of
/// these decodes until the next one differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub profile: u8,
    /// Level as the codec numbers it (VP9: 10 x level; AV1: seq_level_idx).
    pub level: u8,
    pub tier: u8,
    pub depth: u8,
    pub mono: bool,
    pub subsampling: (bool, bool),
    pub full_range: bool,
    /// Colour primaries, transfer, matrix (ISO/IEC 23091-2 numbers).
    pub colour: (u8, u8, u8),
    pub width: u32,
    pub height: u32,
}

/// One VP9 frame's header, as far as we read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vp9Frame {
    /// Shows an earlier frame again (`show_existing_frame`): no decoding.
    pub show_existing: Option<u8>,
    pub key: bool,
    pub intra_only: bool,
    pub show: bool,
    pub refresh: u8,
    /// Set on key frames and intra-only frames (which carry it).
    pub config: Option<Config>,
    /// The coded size, when the header says it (not for frames that take
    /// a reference's size).
    pub size: Option<(u32, u32)>,
}

/// The frames of a VP9 buffer: one, or those a superframe index lists.
/// A malformed index makes the whole buffer one frame.
pub fn vp9_frames(data: &[u8]) -> Vec<&[u8]> {
    let Some(&last) = data.last() else { return Vec::new() };
    if last & 0xe0 == 0xc0 {
        let n = (last & 7) as usize + 1;
        let size_bytes = ((last >> 3) & 3) as usize + 1;
        let index = 2 + size_bytes * n;
        if data.len() >= index && data[data.len() - index] == last {
            let mut out = Vec::with_capacity(n);
            let mut at = 0usize;
            let body = data.len() - index;
            for i in 0..n {
                let p = data.len() - index + 1 + i * size_bytes;
                let mut len = 0usize;
                for (k, &b) in data[p..p + size_bytes].iter().enumerate() {
                    len |= (b as usize) << (8 * k);
                }
                let Some(end) = at.checked_add(len).filter(|&e| e <= body) else { return vec![data] };
                if len > 0 {
                    out.push(&data[at..end]);
                }
                at = end;
            }
            return out;
        }
    }
    vec![data]
}

/// color_config(): a [`Config`] without its size.
fn vp9_color(b: &mut Bits, profile: u8) -> Option<Config> {
    let depth = if profile >= 2 { if b.bit()? == 1 { 12 } else { 10 } } else { 8 };
    let space = b.bits(3)?;
    let full;
    let mut sub = (true, true);
    if space != 7 {
        full = b.bit()? == 1;
        if profile == 1 || profile == 3 {
            sub = (b.bit()? == 1, b.bit()? == 1);
            b.skip(1)?;
        }
    } else {
        // sRGB: 4:4:4, full range.
        full = true;
        sub = (false, false);
        if profile == 1 || profile == 3 {
            b.skip(1)?;
        }
    }
    // VP9 colour spaces as primaries / transfer / matrix.
    let colour = match space {
        1 => (6, 6, 6),   // BT.601
        2 => (1, 1, 1),   // BT.709
        3 => (6, 6, 6),   // SMPTE-170
        4 => (7, 7, 7),   // SMPTE-240
        5 => (9, 14, 9),  // BT.2020
        7 => (1, 13, 0),  // sRGB
        _ => (2, 2, 2),   // unknown
    };
    Some(Config { profile, level: 0, tier: 0, depth, mono: false, subsampling: sub, full_range: full, colour, width: 0, height: 0 })
}

/// Read a VP9 frame header. `refs`: the sizes in the 8 reference slots
/// (for frames that take their size from one); updated for the frame.
pub fn vp9_header(frame: &[u8], refs: &mut [(u32, u32); 8]) -> Option<Vp9Frame> {
    let mut b = Bits::new(frame);
    if b.bits(2)? != 2 {
        return None;
    }
    let lo = b.bit()? as u8;
    let hi = b.bit()? as u8;
    let profile = (hi << 1) | lo;
    if profile == 3 {
        b.skip(1)?;
    }
    if b.bit()? == 1 {
        let idx = b.bits(3)? as u8;
        return Some(Vp9Frame { show_existing: Some(idx), key: false, intra_only: false, show: true, refresh: 0, config: None, size: None });
    }
    let key = b.bit()? == 0;
    let show = b.bit()? == 1;
    let error_res = b.bit()? == 1;
    let sync = |b: &mut Bits| -> Option<()> { (b.bits(24)? == 0x49_83_42).then_some(()) };
    let size = |b: &mut Bits| -> Option<(u32, u32)> {
        let w = b.bits(16)? + 1;
        let h = b.bits(16)? + 1;
        // render_size(): different from the frame size, or not.
        if b.bit()? == 1 {
            b.skip(32)?;
        }
        Some((w, h))
    };
    let mut f = Vp9Frame { show_existing: None, key, intra_only: false, show, refresh: 0xff, config: None, size: None };
    let mut colour = None;
    if key {
        sync(&mut b)?;
        colour = Some(vp9_color(&mut b, profile)?);
        f.size = Some(size(&mut b)?);
    } else {
        f.intra_only = if show { false } else { b.bit()? == 1 };
        if !error_res {
            b.skip(2)?; // reset_frame_context
        }
        if f.intra_only {
            sync(&mut b)?;
            colour = Some(if profile > 0 {
                vp9_color(&mut b, profile)?
            } else {
                Config { profile, level: 0, tier: 0, depth: 8, mono: false, subsampling: (true, true), full_range: false, colour: (2, 2, 2), width: 0, height: 0 }
            });
            f.refresh = b.bits(8)? as u8;
            f.size = Some(size(&mut b)?);
        } else {
            f.refresh = b.bits(8)? as u8;
            let mut idx = [0usize; 3];
            for i in &mut idx {
                *i = b.bits(3)? as usize;
                b.skip(1)?; // sign bias
            }
            for &i in &idx {
                if b.bit()? == 1 {
                    f.size = Some(refs[i]);
                    break;
                }
            }
            if f.size.is_none() {
                f.size = Some(size(&mut b)?);
            }
        }
    }
    if let (Some(c), Some((width, height))) = (colour, f.size) {
        f.config = Some(Config { width, height, ..c });
    }
    if let Some(s) = f.size {
        for (i, r) in refs.iter_mut().enumerate() {
            if f.refresh & (1 << i) != 0 {
                *r = s;
            }
        }
    }
    Some(f)
}

/// AV1 OBU types we act on.
pub mod obu {
    pub const SEQUENCE_HEADER: u8 = 1;
    pub const TEMPORAL_DELIMITER: u8 = 2;
    pub const FRAME_HEADER: u8 = 3;
    pub const FRAME: u8 = 6;
    pub const PADDING: u8 = 15;
}

/// One OBU: its type and the whole of it (header, size and payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    pub kind: u8,
    pub data: &'a [u8],
    /// Where its payload starts in `data`.
    pub payload: usize,
}

fn leb128(d: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *d.get(*at)?;
        *at += 1;
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The OBUs of a temporal unit. Every OBU must carry its size (as in
/// files and on the web); a buffer that breaks off yields what came
/// before.
pub fn av1_obus(data: &[u8]) -> Vec<Obu<'_>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < data.len() && out.len() < MAX_OBUS {
        let start = at;
        let h = data[at];
        at += 1;
        if h & 0x80 != 0 || h & 2 == 0 {
            break;
        }
        if h & 4 != 0 {
            at += 1;
        }
        let Some(len) = leb128(data, &mut at) else { break };
        let Some(end) = usize::try_from(len).ok().and_then(|l| at.checked_add(l)).filter(|&e| e <= data.len()) else { break };
        out.push(Obu { kind: (h >> 3) & 0xf, data: &data[start..end], payload: at - start });
        at = end;
    }
    out
}

fn uvlc(b: &mut Bits) -> Option<u32> {
    let mut zeros = 0;
    while b.bit()? == 0 {
        zeros += 1;
        if zeros >= 32 {
            return None;
        }
    }
    Some(((1u64 << zeros) - 1 + b.bits(zeros)? as u64).min(u32::MAX as u64) as u32)
}

/// Read an AV1 sequence header OBU's payload.
pub fn av1_sequence(payload: &[u8]) -> Option<Config> {
    let mut b = Bits::new(payload);
    let profile = b.bits(3)? as u8;
    if profile > 2 {
        return None;
    }
    b.skip(1)?; // still_picture
    let reduced = b.bit()? == 1;
    let (mut level, mut tier) = (0u8, 0u8);
    if reduced {
        level = b.bits(5)? as u8;
    } else {
        let timing = b.bit()? == 1;
        let mut decoder_model = false;
        let mut delay_len = 0;
        if timing {
            b.skip(64)?;
            if b.bit()? == 1 {
                uvlc(&mut b)?;
            }
            decoder_model = b.bit()? == 1;
            if decoder_model {
                delay_len = b.bits(5)? + 1;
                b.skip(32 + 5 + 5)?;
            }
        }
        let display_delay = b.bit()? == 1;
        let points = b.bits(5)? + 1;
        for i in 0..points {
            b.skip(12)?; // operating_point_idc
            let l = b.bits(5)? as u8;
            let t = if l > 7 { b.bit()? as u8 } else { 0 };
            if i == 0 {
                (level, tier) = (l, t);
            }
            if decoder_model && b.bit()? == 1 {
                b.skip(2 * delay_len as usize + 1)?;
            }
            if display_delay && b.bit()? == 1 {
                b.skip(4)?;
            }
        }
    }
    let wbits = b.bits(4)? + 1;
    let hbits = b.bits(4)? + 1;
    let width = b.bits(wbits)?.checked_add(1)?;
    let height = b.bits(hbits)?.checked_add(1)?;
    if !reduced && b.bit()? == 1 {
        b.skip(4 + 3)?; // frame id lengths
    }
    b.skip(3)?; // 128x128 superblocks, filter intra, intra edge
    if !reduced {
        b.skip(4)?; // interintra, masked, warped, dual filter
        let order_hint = b.bit()? == 1;
        if order_hint {
            b.skip(2)?; // jnt_comp, ref_frame_mvs
        }
        let screen = if b.bit()? == 1 { 2 } else { b.bit()? };
        if screen > 0 && b.bit()? == 0 {
            b.skip(1)?; // seq_force_integer_mv
        }
        if order_hint {
            b.skip(3)?;
        }
    }
    b.skip(3)?; // superres, cdef, restoration
    // color_config()
    let high = b.bit()? == 1;
    let depth = if profile == 2 && high { if b.bit()? == 1 { 12 } else { 10 } } else if high { 10 } else { 8 };
    let mono = if profile == 1 { false } else { b.bit()? == 1 };
    let mut colour = (2u8, 2u8, 2u8);
    if b.bit()? == 1 {
        colour = (b.bits(8)? as u8, b.bits(8)? as u8, b.bits(8)? as u8);
    }
    let full_range;
    let mut sub = (true, true);
    if mono {
        full_range = b.bit()? == 1;
    } else if colour == (1, 13, 0) {
        full_range = true;
        sub = (false, false);
    } else {
        full_range = b.bit()? == 1;
        sub = match profile {
            0 => (true, true),
            1 => (false, false),
            _ if depth == 12 => {
                let x = b.bit()? == 1;
                (x, x && b.bit()? == 1)
            }
            _ => (true, false),
        };
    }
    Some(Config { profile, level, tier, depth, mono, subsampling: sub, full_range, colour, width, height })
}

/// The `vpcC` box VideoToolbox describes a VP9 stream with (VP Codec
/// ISO Media File Format Binding, version 1).
pub fn vpcc(c: &Config) -> Vec<u8> {
    let sub = match c.subsampling {
        (true, true) => 1, // 4:2:0 colocated with luma
        (true, false) => 2,
        _ => 3,
    };
    vec![1, 0, 0, 0, c.profile, c.level, (c.depth << 4) | (sub << 1) | c.full_range as u8, c.colour.0, c.colour.1, c.colour.2, 0, 0]
}

/// The `av1C` box VideoToolbox describes an AV1 stream with, its
/// sequence header OBU inside.
pub fn av1c(c: &Config, sequence_obu: &[u8]) -> Vec<u8> {
    let mut v = vec![
        0x81,
        (c.profile << 5) | (c.level & 0x1f),
        (c.tier << 7)
            | ((c.depth > 8) as u8) << 6
            | ((c.depth == 12) as u8) << 5
            | (c.mono as u8) << 4
            | (c.subsampling.0 as u8) << 3
            | (c.subsampling.1 as u8) << 2,
        0,
    ];
    v.extend_from_slice(sequence_obu);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superframes() {
        // Two frames of 3 and 2 bytes, 1-byte sizes: marker 0b11000_001.
        let d = [1, 2, 3, 4, 5, 0xc1, 3, 2, 0xc1];
        assert_eq!(vp9_frames(&d), vec![&[1, 2, 3][..], &[4, 5][..]]);
        // Sizes past the end: one frame.
        let d = [1, 2, 0xc1, 9, 9, 0xc1];
        assert_eq!(vp9_frames(&d).len(), 1);
        assert_eq!(vp9_frames(&[0x82]).len(), 1);
        assert!(vp9_frames(&[]).is_empty());
    }

    #[test]
    fn vp9_key_frame() {
        // Profile 0 key frame, shown, 352x288, BT.709.
        // 10 0 0 0 1 0 | sync | cs=2 range=0 | w-1 h-1 | render 0
        let mut bits = String::from("10000010");
        bits += "010010011000001101000010";
        bits += "0100";
        bits += &format!("{:016b}{:016b}0", 351, 287);
        let mut d: Vec<u8> = bits.as_bytes().chunks(8).map(|c| u8::from_str_radix(&format!("{:0<8}", std::str::from_utf8(c).unwrap()), 2).unwrap()).collect();
        d.extend_from_slice(&[0; 4]);
        let mut refs = [(0, 0); 8];
        let f = vp9_header(&d, &mut refs).unwrap();
        assert!(f.key && f.show && f.show_existing.is_none());
        let c = f.config.unwrap();
        assert_eq!((c.profile, c.depth, c.width, c.height, c.colour), (0, 8, 352, 288, (1, 1, 1)));
        assert_eq!(refs, [(352, 288); 8]);
        assert_eq!(vpcc(&c)[4..7], [0, 0, 0x82]);
        // show_existing_frame of slot 5: 10 0 0 1 101
        assert_eq!(vp9_header(&[0b1000_1101], &mut refs).unwrap().show_existing, Some(5));
        assert!(vp9_header(&[0x40], &mut refs).is_none());
    }

    #[test]
    fn av1_obus_and_sequence_header() {
        // A temporal delimiter, then a sequence header: profile 0,
        // reduced still picture header off, no timing, one operating
        // point at level 8, 1920x1080 with 11/11-bit sizes.
        let mut bits = String::from("000"); // profile
        bits += "0"; // still
        bits += "0"; // reduced
        bits += "0"; // timing
        bits += "0"; // initial display delay
        bits += "00000"; // one operating point
        bits += "000000000000"; // idc
        bits += "01000"; // level 8 (4.0), tier follows (>7)
        bits += "0";
        bits += "1010"; // width bits - 1 = 10
        bits += "1010";
        bits += &format!("{:011b}{:011b}", 1919, 1079);
        bits += "0"; // frame ids
        bits += "000"; // sb, filter intra, edge
        bits += "0000"; // interintra.. dual filter
        bits += "0"; // order hint
        bits += "1"; // seq_choose_screen_content_tools -> 2
        bits += "1"; // seq_choose_integer_mv
        bits += "000"; // superres cdef restoration
        bits += "1"; // high bitdepth -> 10
        bits += "0"; // mono
        bits += "0"; // no colour description
        bits += "0"; // range
        bits += "00"; // chroma sample position
        bits += "0"; // separate uv delta q
        bits += "0"; // film grain
        let payload: Vec<u8> = bits.as_bytes().chunks(8).map(|c| u8::from_str_radix(&format!("{:0<8}", std::str::from_utf8(c).unwrap()), 2).unwrap()).collect();
        let c = av1_sequence(&payload).unwrap();
        assert_eq!((c.profile, c.level, c.tier, c.depth, c.width, c.height, c.mono), (0, 8, 0, 10, 1920, 1080, false));
        let mut tu = vec![0x12, 0x00, 0x0a, payload.len() as u8];
        tu.extend_from_slice(&payload);
        let o = av1_obus(&tu);
        assert_eq!(o.len(), 2);
        assert_eq!((o[0].kind, o[1].kind), (obu::TEMPORAL_DELIMITER, obu::SEQUENCE_HEADER));
        assert_eq!(&o[1].data[o[1].payload..], &payload[..]);
        assert_eq!(av1c(&c, o[1].data)[..3], [0x81, 0x08, 0x4c]);
        // An OBU without a size field, or longer than the buffer.
        assert!(av1_obus(&[0x10]).is_empty());
        assert!(av1_obus(&[0x12, 0x05, 1]).is_empty());
    }

    #[test]
    fn junk_never_panics() {
        let mut seed = 0x9e37_79b9u32;
        let mut refs = [(1, 1); 8];
        for len in 0..400 {
            let d: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            for f in vp9_frames(&d) {
                let _ = vp9_header(f, &mut refs);
            }
            for o in av1_obus(&d) {
                let _ = av1_sequence(&o.data[o.payload..]);
            }
            let _ = av1_sequence(&d);
        }
    }
}
