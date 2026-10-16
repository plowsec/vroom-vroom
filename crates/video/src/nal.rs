//! H.264 and HEVC bitstreams as a guest sends them (Annex B: NAL units
//! after `00 00 01` start codes), taken apart before VideoToolbox sees
//! them: NAL units, access units (one picture each), and the parameter
//! sets (SPS, PPS, and VPS for HEVC) a decoder needs up front.
//!
//! All of it is guest data: lengths, ids and counts are checked, nothing
//! here can panic on any input.

use std::collections::BTreeMap;

use crate::Codec;

/// Most NAL units we take from one buffer.
pub const MAX_NALS: usize = 4096;
/// Biggest parameter set we keep.
pub const MAX_PARAM_SET: usize = 4096;

/// The NAL units of an Annex B buffer, start codes and trailing zeros
/// removed. Bytes before the first start code are ignored.
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                push_nal(&mut out, &data[s..i]);
            }
            i += 3;
            start = Some(i);
            if out.len() >= MAX_NALS {
                return out;
            }
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        push_nal(&mut out, &data[s..]);
    }
    out
}

fn push_nal<'a>(out: &mut Vec<&'a [u8]>, nal: &'a [u8]) {
    // Trailing zero bytes belong to the next start code (00 00 00 01) or
    // are padding.
    let end = nal.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
    if end > 0 && out.len() < MAX_NALS {
        out.push(&nal[..end]);
    }
}

/// The payload of a NAL unit without its emulation prevention bytes
/// (`00 00 03` -> `00 00`), at most `max` bytes of it.
pub fn rbsp(nal: &[u8], max: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len().min(max));
    let mut zeros = 0;
    for &b in nal {
        if out.len() >= max {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Reads bits, most significant first. Past the end reads fail.
#[derive(Debug)]
pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }

    pub fn bits(&mut self, n: u32) -> Option<u32> {
        if n > 32 {
            return None;
        }
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        u32::try_from(v).ok()
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        let to = self.pos.checked_add(n)?;
        if to > self.data.len() * 8 {
            return None;
        }
        self.pos = to;
        Some(())
    }

    /// Exp-Golomb `ue(v)`, up to 31 leading zeros.
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)? as u64;
        u32::try_from((1u64 << zeros) - 1 + rest).ok()
    }
}

/// What a NAL unit is, as far as splitting the stream goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A slice of a picture; `first`: the picture's first slice.
    Slice { first: bool },
    /// A parameter set: its kind and id.
    Param(Param, u32),
    /// Access unit delimiter.
    Delimiter,
    /// SEI that goes before its picture's slices (starts an access unit
    /// when it follows slices).
    PrefixSei,
    /// Anything else (suffix SEI, end of sequence, filler...).
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Param {
    Vps,
    Sps,
    Pps,
}

/// Classify one NAL unit (`None`: too short or not for the base layer).
pub fn classify(codec: Codec, nal: &[u8]) -> Option<Kind> {
    match codec {
        Codec::H264 => {
            let h = *nal.first()?;
            if h & 0x80 != 0 {
                return None;
            }
            Some(match h & 0x1f {
                1 | 5 => Kind::Slice { first: nal.get(1)? & 0x80 != 0 },
                6 => Kind::PrefixSei,
                7 => Kind::Param(Param::Sps, h264_sps_id(nal)?),
                8 => Kind::Param(Param::Pps, h264_pps_id(nal)?),
                9 => Kind::Delimiter,
                _ => Kind::Other,
            })
        }
        Codec::Hevc => {
            let (h0, h1) = (*nal.first()?, *nal.get(1)?);
            if h0 & 0x80 != 0 {
                return None;
            }
            let layer = ((h0 & 1) << 5) | (h1 >> 3);
            if layer != 0 {
                return None;
            }
            Some(match (h0 >> 1) & 0x3f {
                0..=9 | 16..=21 => Kind::Slice { first: nal.get(2)? & 0x80 != 0 },
                32 => Kind::Param(Param::Vps, hevc_vps_id(nal)?),
                33 => Kind::Param(Param::Sps, hevc_sps_id(nal)?),
                34 => Kind::Param(Param::Pps, hevc_pps_id(nal)?),
                35 => Kind::Delimiter,
                39 => Kind::PrefixSei,
                _ => Kind::Other,
            })
        }
        // Not NAL unit streams.
        Codec::Vp9 | Codec::Av1 => None,
    }
}

fn h264_sps_id(nal: &[u8]) -> Option<u32> {
    let r = rbsp(nal.get(1..)?, 64);
    let mut b = Bits::new(&r);
    b.skip(24)?; // profile_idc, constraint flags, level_idc
    b.ue().filter(|&id| id <= 31)
}

fn h264_pps_id(nal: &[u8]) -> Option<u32> {
    let r = rbsp(nal.get(1..)?, 64);
    Bits::new(&r).ue().filter(|&id| id <= 255)
}

fn hevc_vps_id(nal: &[u8]) -> Option<u32> {
    Some((*nal.get(2)? >> 4) as u32)
}

fn hevc_sps_id(nal: &[u8]) -> Option<u32> {
    let r = rbsp(nal.get(2..)?, 128);
    let mut b = Bits::new(&r);
    b.skip(4)?; // sps_video_parameter_set_id
    let sub_layers = b.bits(3)? as usize; // sps_max_sub_layers_minus1
    if sub_layers > 6 {
        return None;
    }
    b.skip(1)?; // sps_temporal_id_nesting_flag
    // profile_tier_level(1, sub_layers): general part, 88 bits + level.
    b.skip(88 + 8)?;
    let mut present = [(false, false); 6];
    for p in present.iter_mut().take(sub_layers) {
        *p = (b.bit()? == 1, b.bit()? == 1);
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers))?;
    }
    for &(profile, level) in present.iter().take(sub_layers) {
        if profile {
            b.skip(88)?;
        }
        if level {
            b.skip(8)?;
        }
    }
    b.ue().filter(|&id| id <= 15)
}

/// Bits per luma sample an SPS asks for.
pub fn sps_depth(codec: Codec, nal: &[u8]) -> Option<u32> {
    match codec {
        Codec::H264 => {
            let r = rbsp(nal.get(1..)?, 64);
            let mut b = Bits::new(&r);
            let profile = b.bits(8)?;
            b.skip(16)?;
            b.ue()?;
            if !matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
                return Some(8);
            }
            if b.ue()? == 3 {
                b.skip(1)?;
            }
            Some(b.ue().filter(|&d| d <= 6)? + 8)
        }
        Codec::Hevc => {
            let r = rbsp(nal.get(2..)?, 128);
            let mut b = Bits::new(&r);
            b.skip(4)?;
            let sub_layers = b.bits(3)? as usize;
            if sub_layers > 6 {
                return None;
            }
            b.skip(1 + 88 + 8)?;
            let mut present = [(false, false); 6];
            for p in present.iter_mut().take(sub_layers) {
                *p = (b.bit()? == 1, b.bit()? == 1);
            }
            if sub_layers > 0 {
                b.skip(2 * (8 - sub_layers))?;
            }
            for &(profile, level) in present.iter().take(sub_layers) {
                b.skip(if profile { 88 } else { 0 } + if level { 8 } else { 0 })?;
            }
            b.ue()?; // id
            if b.ue()? == 3 {
                b.skip(1)?;
            }
            b.ue()?;
            b.ue()?;
            if b.bit()? == 1 {
                for _ in 0..4 {
                    b.ue()?;
                }
            }
            Some(b.ue().filter(|&d| d <= 8)? + 8)
        }
        _ => None,
    }
}

fn hevc_pps_id(nal: &[u8]) -> Option<u32> {
    let r = rbsp(nal.get(2..)?, 64);
    Bits::new(&r).ue().filter(|&id| id <= 63)
}

/// One picture's NAL units, parameter sets left out (see [`ParamSets`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AccessUnit<'a> {
    pub nals: Vec<&'a [u8]>,
}

/// What one guest buffer holds: parameter sets in the order they came,
/// each with the number of access units before it, and the access units.
#[derive(Debug, Default)]
pub struct Parsed<'a> {
    pub params: Vec<(usize, Param, u32, &'a [u8])>,
    pub units: Vec<AccessUnit<'a>>,
}

/// Split a buffer into access units. A new one starts at a picture's
/// first slice, a delimiter, a parameter set or a prefix SEI, once the
/// current one has slices. NAL units we can't classify are dropped.
pub fn parse(codec: Codec, data: &[u8]) -> Parsed<'_> {
    let mut p = Parsed::default();
    let mut cur = AccessUnit::default();
    let mut has_slice = false;
    for nal in split_annexb(data) {
        let Some(kind) = classify(codec, nal) else { continue };
        let starts = match kind {
            Kind::Slice { first } => first,
            Kind::Param(..) | Kind::Delimiter | Kind::PrefixSei => true,
            Kind::Other => false,
        };
        if starts && has_slice {
            p.units.push(std::mem::take(&mut cur));
            has_slice = false;
        }
        match kind {
            Kind::Param(param, id) => p.params.push((p.units.len(), param, id, nal)),
            Kind::Delimiter => {}
            Kind::Slice { .. } => {
                has_slice = true;
                cur.nals.push(nal);
            }
            Kind::PrefixSei | Kind::Other => cur.nals.push(nal),
        }
    }
    if has_slice {
        p.units.push(cur);
    }
    p
}

/// The parameter sets seen so far, by kind and id.
#[derive(Debug)]
pub struct ParamSets {
    codec: Codec,
    sets: BTreeMap<(Param, u32), Vec<u8>>,
}

impl ParamSets {
    pub fn new(codec: Codec) -> Self {
        ParamSets { codec, sets: BTreeMap::new() }
    }

    /// Keep `nal`; true if that changed anything.
    pub fn update(&mut self, param: Param, id: u32, nal: &[u8]) -> bool {
        if nal.len() > MAX_PARAM_SET {
            return false;
        }
        match self.sets.get(&(param, id)) {
            Some(old) if old == nal => false,
            _ => {
                self.sets.insert((param, id), nal.to_vec());
                true
            }
        }
    }

    /// Enough to start decoding (an SPS and a PPS, and a VPS for HEVC).
    pub fn complete(&self) -> bool {
        let has = |p| self.sets.keys().any(|&(k, _)| k == p);
        has(Param::Sps) && has(Param::Pps) && (self.codec == Codec::H264 || has(Param::Vps))
    }

    /// All of them, VPS then SPS then PPS, by id.
    pub fn list(&self) -> Vec<&[u8]> {
        self.sets.values().map(|v| v.as_slice()).collect()
    }

    /// The most bits per luma sample any SPS asks for (8 if none says).
    pub fn depth(&self) -> u32 {
        self.sets.iter().filter(|(k, _)| k.0 == Param::Sps).filter_map(|(_, n)| sps_depth(self.codec, n)).max().unwrap_or(8)
    }

    pub fn clear(&mut self) {
        self.sets.clear();
    }
}

/// An access unit as VideoToolbox takes it: each NAL unit after its
/// length (4 bytes, big endian).
pub fn length_prefixed(au: &AccessUnit) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.nals.iter().map(|n| n.len() + 4).sum());
    for n in &au.nals {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_start_codes() {
        let d = [9, 9, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 0x88, 0, 0];
        let n = split_annexb(&d);
        assert_eq!(n, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 0x88][..]]);
        assert!(split_annexb(&[0, 0, 1]).is_empty());
        assert!(split_annexb(&[]).is_empty());
        assert!(split_annexb(&[0, 0]).is_empty());
    }

    #[test]
    fn removes_emulation_prevention() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3], 100), vec![0, 0, 1, 0, 0]);
        assert_eq!(rbsp(&[1, 2, 3], 2), vec![1, 2]);
    }

    #[test]
    fn exp_golomb() {
        // 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3
        let d = [0b1010_0110, 0b0100_0000];
        let mut b = Bits::new(&d);
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
        assert_eq!(Bits::new(&[0, 0, 0, 0, 0]).ue(), None);
        assert_eq!(Bits::new(&[]).ue(), None);
    }

    #[test]
    fn h264_access_units() {
        // SPS (id 0), PPS (id 0), IDR first slice, IDR second slice, then
        // a new picture's first slice.
        let sps = [0x67, 0x64, 0x00, 0x1f, 0x80];
        let pps = [0x68, 0x80];
        let mut d = Vec::new();
        for n in [&sps[..], &pps, &[0x65, 0x88, 1], &[0x65, 0x40, 2], &[0x41, 0x9a, 3]] {
            d.extend_from_slice(&[0, 0, 0, 1]);
            d.extend_from_slice(n);
        }
        let p = parse(Codec::H264, &d);
        assert_eq!(p.params.len(), 2);
        assert_eq!((p.params[0].1, p.params[0].2), (Param::Sps, 0));
        assert_eq!((p.params[1].1, p.params[1].2), (Param::Pps, 0));
        assert_eq!(p.units.len(), 2);
        assert_eq!(p.units[0].nals.len(), 2);
        assert_eq!(p.units[1].nals, vec![&[0x41, 0x9a, 3][..]]);
        let mut ps = ParamSets::new(Codec::H264);
        assert!(!ps.complete());
        assert!(ps.update(Param::Sps, 0, &sps));
        assert!(!ps.update(Param::Sps, 0, &sps));
        assert!(ps.update(Param::Pps, 0, &pps));
        assert!(ps.complete());
        assert_eq!(ps.list().len(), 2);
        let lp = length_prefixed(&p.units[1]);
        assert_eq!(lp, vec![0, 0, 0, 3, 0x41, 0x9a, 3]);
    }

    #[test]
    fn hevc_ids() {
        // VPS id 0; SPS with one sub layer, id 0 (ue "1" after PTL);
        // PPS id 2 (ue 011).
        let vps = [0x40, 0x01, 0x0c, 0x01];
        let mut sps = vec![0x42, 0x01, 0x01];
        sps.extend_from_slice(&[0; 12]);
        sps.push(0x80);
        let pps = [0x44, 0x01, 0x60];
        assert_eq!(classify(Codec::Hevc, &vps), Some(Kind::Param(Param::Vps, 0)));
        assert_eq!(classify(Codec::Hevc, &sps), Some(Kind::Param(Param::Sps, 0)));
        assert_eq!(classify(Codec::Hevc, &pps), Some(Kind::Param(Param::Pps, 2)));
        // IDR_W_RADL (19), first slice.
        assert_eq!(classify(Codec::Hevc, &[0x26, 0x01, 0xaf]), Some(Kind::Slice { first: true }));
        // Another layer: ignored.
        assert_eq!(classify(Codec::Hevc, &[0x26, 0x09, 0xaf]), None);
    }

    #[test]
    fn junk_never_panics() {
        let mut seed = 0x1234_5678u32;
        for len in 0..300 {
            let d: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    let b = (seed >> 16) as u8;
                    if b < 96 { 0 } else if b < 128 { 1 } else { b }
                })
                .collect();
            for c in [Codec::H264, Codec::Hevc] {
                let p = parse(c, &d);
                for u in &p.units {
                    length_prefixed(u);
                }
            }
        }
    }
}
