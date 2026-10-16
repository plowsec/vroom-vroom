//! Display order. VideoToolbox hands pictures out in decoding order; a
//! V4L2 decoder must hand them out in display order. So we work out each
//! picture's order count (POC) from the slice headers, as the standards
//! say, and how many pictures a stream may hold back (its reorder depth),
//! from the SPS.
//!
//! Only what that takes is parsed. Frames only: field pictures
//! (interlaced H.264) get an order count but aren't paired up.

use std::collections::BTreeMap;

use crate::nal::{rbsp, AccessUnit, Bits, Param};
use crate::Codec;

/// The most pictures any stream may hold back.
pub const MAX_REORDER: u32 = 16;

/// What the slice headers say about one picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    /// Order count, within its sequence.
    pub poc: i64,
    /// Starts a new sequence: every picture held back goes out first.
    pub reset: bool,
    /// How many pictures may come before this one in display order.
    pub reorder: u32,
}

#[derive(Debug, Clone, Default)]
struct H264Sps {
    log2_max_frame_num: u32,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    delta_always_zero: bool,
    offset_non_ref: i64,
    offset_top_bottom: i64,
    cycle: Vec<i64>,
    frame_mbs_only: bool,
    separate_planes: bool,
    reorder: u32,
}

#[derive(Debug, Clone, Default)]
struct H264Pps {
    sps: u32,
    bottom_field_poc: bool,
}

#[derive(Debug, Clone, Default)]
struct HevcSps {
    separate_planes: bool,
    log2_max_poc_lsb: u32,
    reorder: u32,
}

#[derive(Debug, Clone, Default)]
struct HevcPps {
    sps: u32,
    dependent_slices: bool,
    output_flag: bool,
    extra_bits: u32,
}

/// Order counts across a stream.
#[derive(Debug)]
pub struct Poc {
    codec: Codec,
    h264_sps: BTreeMap<u32, H264Sps>,
    h264_pps: BTreeMap<u32, H264Pps>,
    hevc_sps: BTreeMap<u32, HevcSps>,
    hevc_pps: BTreeMap<u32, HevcPps>,
    // H.264 POC type 0 / HEVC: the previous reference picture's msb, lsb.
    prev_msb: i64,
    prev_lsb: i64,
    // H.264 POC types 1 and 2.
    prev_frame_num: i64,
    prev_frame_num_offset: i64,
    // HEVC: the next IRAP starts a new sequence (start, end of sequence).
    first: bool,
}

fn se(b: &mut Bits) -> Option<i64> {
    let k = b.ue()? as i64;
    Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
}

fn flag(b: &mut Bits) -> Option<bool> {
    Some(b.bit()? == 1)
}

fn h264_scaling_list(b: &mut Bits, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + se(b)? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn h264_hrd(b: &mut Bits) -> Option<()> {
    let n = b.ue()?;
    if n > 31 {
        return None;
    }
    b.skip(8)?;
    for _ in 0..=n {
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
    }
    b.skip(20)
}

/// MaxDpbMbs by level_idc (H.264 table A-1).
fn h264_max_dpb_mbs(level: u32, constraint3: bool) -> u32 {
    match level {
        9 => 396,
        10 => 396,
        11 if constraint3 => 396,
        11 => 900,
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110_400,
        51 | 52 => 184_320,
        _ => 696_320,
    }
}

fn parse_h264_sps(nal: &[u8]) -> Option<(u32, H264Sps)> {
    let r = rbsp(nal.get(1..)?, 4096);
    let mut b = Bits::new(&r);
    let profile = b.bits(8)?;
    let constraints = b.bits(8)?;
    let level = b.bits(8)?;
    let id = b.ue().filter(|&i| i <= 31)?;
    let mut s = H264Sps::default();
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = b.ue()?;
        if chroma == 3 {
            s.separate_planes = flag(&mut b)?;
        }
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
        if flag(&mut b)? {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if flag(&mut b)? {
                    h264_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    s.log2_max_frame_num = b.ue().filter(|&v| v <= 12)? + 4;
    s.poc_type = b.ue().filter(|&v| v <= 2)?;
    if s.poc_type == 0 {
        s.log2_max_poc_lsb = b.ue().filter(|&v| v <= 12)? + 4;
    } else if s.poc_type == 1 {
        s.delta_always_zero = flag(&mut b)?;
        s.offset_non_ref = se(&mut b)?;
        s.offset_top_bottom = se(&mut b)?;
        let n = b.ue().filter(|&v| v <= 255)?;
        for _ in 0..n {
            s.cycle.push(se(&mut b)?);
        }
    }
    let max_ref = b.ue()?;
    b.skip(1)?;
    let w = b.ue()?.checked_add(1)?;
    let h_units = b.ue()?.checked_add(1)?;
    s.frame_mbs_only = flag(&mut b)?;
    if !s.frame_mbs_only {
        b.skip(1)?;
    }
    b.skip(1)?;
    if flag(&mut b)? {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    // Without the VUI's bitstream restriction, the level's DPB size.
    let frame_mbs = (w as u64) * (h_units as u64) * if s.frame_mbs_only { 1 } else { 2 };
    let dpb = (h264_max_dpb_mbs(level, constraints & 0x10 != 0) as u64 / frame_mbs.max(1)).min(16) as u32;
    s.reorder = if profile == 66 || (matches!(profile, 44 | 86 | 100 | 110 | 122 | 244) && constraints & 0x10 != 0) { 0 } else { dpb.max(max_ref.min(16)) };
    if flag(&mut b)? {
        if let Some(r) = h264_vui_reorder(&mut b) {
            s.reorder = r;
        }
    }
    s.reorder = s.reorder.min(MAX_REORDER);
    Some((id, s))
}

/// max_num_reorder_frames from the VUI, if it has a bitstream restriction.
fn h264_vui_reorder(b: &mut Bits) -> Option<u32> {
    if flag(b)? && b.bits(8)? == 255 {
        b.skip(32)?;
    }
    if flag(b)? {
        b.skip(1)?;
    }
    if flag(b)? {
        b.skip(4)?;
        if flag(b)? {
            b.skip(24)?;
        }
    }
    if flag(b)? {
        b.ue()?;
        b.ue()?;
    }
    if flag(b)? {
        b.skip(65)?;
    }
    let nal_hrd = flag(b)?;
    if nal_hrd {
        h264_hrd(b)?;
    }
    let vcl_hrd = flag(b)?;
    if vcl_hrd {
        h264_hrd(b)?;
    }
    if nal_hrd || vcl_hrd {
        b.skip(1)?;
    }
    b.skip(1)?;
    if !flag(b)? {
        return None;
    }
    b.skip(1)?;
    for _ in 0..4 {
        b.ue()?;
    }
    b.ue()
}

fn parse_h264_pps(nal: &[u8]) -> Option<(u32, H264Pps)> {
    let r = rbsp(nal.get(1..)?, 64);
    let mut b = Bits::new(&r);
    let id = b.ue().filter(|&i| i <= 255)?;
    let sps = b.ue().filter(|&i| i <= 31)?;
    b.skip(1)?;
    Some((id, H264Pps { sps, bottom_field_poc: flag(&mut b)? }))
}

fn parse_hevc_sps(nal: &[u8]) -> Option<(u32, HevcSps)> {
    let r = rbsp(nal.get(2..)?, 4096);
    let mut b = Bits::new(&r);
    b.skip(4)?;
    let sub_layers = b.bits(3)? as usize;
    if sub_layers > 6 {
        return None;
    }
    b.skip(1 + 96)?;
    let mut present = [(false, false); 6];
    for p in present.iter_mut().take(sub_layers) {
        *p = (flag(&mut b)?, flag(&mut b)?);
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers))?;
    }
    for &(profile, level) in present.iter().take(sub_layers) {
        b.skip(if profile { 88 } else { 0 } + if level { 8 } else { 0 })?;
    }
    let id = b.ue().filter(|&i| i <= 15)?;
    let mut s = HevcSps::default();
    if b.ue()? == 3 {
        s.separate_planes = flag(&mut b)?;
    }
    b.ue()?;
    b.ue()?;
    if flag(&mut b)? {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    b.ue()?;
    b.ue()?;
    s.log2_max_poc_lsb = b.ue().filter(|&v| v <= 12)? + 4;
    let all = flag(&mut b)?;
    for _ in if all { 0 } else { sub_layers }..=sub_layers {
        b.ue()?;
        s.reorder = b.ue()?.min(MAX_REORDER);
        b.ue()?;
    }
    Some((id, s))
}

fn parse_hevc_pps(nal: &[u8]) -> Option<(u32, HevcPps)> {
    let r = rbsp(nal.get(2..)?, 64);
    let mut b = Bits::new(&r);
    let id = b.ue().filter(|&i| i <= 63)?;
    let sps = b.ue().filter(|&i| i <= 15)?;
    Some((id, HevcPps { sps, dependent_slices: flag(&mut b)?, output_flag: flag(&mut b)?, extra_bits: b.bits(3)? }))
}

impl Poc {
    pub fn new(codec: Codec) -> Self {
        Poc {
            codec,
            h264_sps: BTreeMap::new(),
            h264_pps: BTreeMap::new(),
            hevc_sps: BTreeMap::new(),
            hevc_pps: BTreeMap::new(),
            prev_msb: 0,
            prev_lsb: 0,
            prev_frame_num: 0,
            prev_frame_num_offset: 0,
            first: true,
        }
    }

    /// A parameter set went by (`nal` from [`crate::nal::parse`]).
    pub fn param(&mut self, param: Param, nal: &[u8]) {
        match (self.codec, param) {
            (Codec::H264, Param::Sps) => {
                if let Some((id, s)) = parse_h264_sps(nal) {
                    self.h264_sps.insert(id, s);
                }
            }
            (Codec::H264, Param::Pps) => {
                if let Some((id, p)) = parse_h264_pps(nal) {
                    self.h264_pps.insert(id, p);
                }
            }
            (Codec::Hevc, Param::Sps) => {
                if let Some((id, s)) = parse_hevc_sps(nal) {
                    self.hevc_sps.insert(id, s);
                }
            }
            (Codec::Hevc, Param::Pps) => {
                if let Some((id, p)) = parse_hevc_pps(nal) {
                    self.hevc_pps.insert(id, p);
                }
            }
            _ => {}
        }
    }

    /// The order of the picture in `au`, if its first slice says.
    pub fn picture(&mut self, au: &AccessUnit) -> Option<Picture> {
        match self.codec {
            Codec::H264 => self.h264(au),
            Codec::Hevc => self.hevc(au),
        }
    }

    fn h264(&mut self, au: &AccessUnit) -> Option<Picture> {
        let nal = au.nals.iter().find(|n| matches!(n.first().map(|h| h & 0x1f), Some(1 | 5)))?;
        let idr = nal[0] & 0x1f == 5;
        let is_ref = nal[0] & 0x60 != 0;
        let r = rbsp(&nal[1..], 64);
        let mut b = Bits::new(&r);
        b.ue()?;
        b.ue()?;
        let pps = self.h264_pps.get(&b.ue()?)?.clone();
        let sps = self.h264_sps.get(&pps.sps)?.clone();
        if sps.separate_planes {
            b.skip(2)?;
        }
        let frame_num = b.bits(sps.log2_max_frame_num)? as i64;
        let mut field = false;
        if !sps.frame_mbs_only {
            field = flag(&mut b)?;
            if field {
                b.skip(1)?;
            }
        }
        if idr {
            b.ue()?;
        }
        let max_frame_num = 1i64 << sps.log2_max_frame_num;
        let poc = match sps.poc_type {
            0 => {
                let lsb = b.bits(sps.log2_max_poc_lsb)? as i64;
                let bottom = if pps.bottom_field_poc && !field { se(&mut b)? } else { 0 };
                let max = 1i64 << sps.log2_max_poc_lsb;
                if idr {
                    self.prev_msb = 0;
                    self.prev_lsb = 0;
                }
                let msb = if lsb < self.prev_lsb && self.prev_lsb - lsb >= max / 2 {
                    self.prev_msb + max
                } else if lsb > self.prev_lsb && lsb - self.prev_lsb > max / 2 {
                    self.prev_msb - max
                } else {
                    self.prev_msb
                };
                if is_ref {
                    self.prev_msb = msb;
                    self.prev_lsb = lsb;
                }
                let top = msb + lsb;
                top.min(top + bottom)
            }
            t => {
                let offset = if idr {
                    0
                } else if self.prev_frame_num > frame_num {
                    self.prev_frame_num_offset + max_frame_num
                } else {
                    self.prev_frame_num_offset
                };
                self.prev_frame_num = frame_num;
                self.prev_frame_num_offset = offset;
                if t == 2 {
                    if idr {
                        0
                    } else if is_ref {
                        2 * (offset + frame_num)
                    } else {
                        2 * (offset + frame_num) - 1
                    }
                } else {
                    let (d0, d1) = if sps.delta_always_zero {
                        (0, 0)
                    } else {
                        let d0 = se(&mut b)?;
                        (d0, if pps.bottom_field_poc && !field { se(&mut b)? } else { 0 })
                    };
                    let n = sps.cycle.len() as i64;
                    let mut abs = if n != 0 { offset + frame_num } else { 0 };
                    if !is_ref && abs > 0 {
                        abs -= 1;
                    }
                    let mut expected = 0i64;
                    if abs > 0 {
                        let per_cycle: i64 = sps.cycle.iter().sum();
                        let (cycles, within) = ((abs - 1) / n, (abs - 1) % n);
                        expected = cycles.saturating_mul(per_cycle) + sps.cycle[..=within as usize].iter().sum::<i64>();
                    }
                    if !is_ref {
                        expected += sps.offset_non_ref;
                    }
                    let top = expected + d0;
                    top.min(top + sps.offset_top_bottom + d1)
                }
            }
        };
        Some(Picture { poc, reset: idr, reorder: if sps.poc_type == 2 { 0 } else { sps.reorder } })
    }

    fn hevc(&mut self, au: &AccessUnit) -> Option<Picture> {
        let nal = au.nals.iter().find(|n| n.len() > 2 && matches!((n[0] >> 1) & 0x3f, 0..=9 | 16..=21))?;
        let t = (nal[0] >> 1) & 0x3f;
        let tid = (nal[1] & 7).checked_sub(1)?;
        let r = rbsp(&nal[2..], 64);
        let mut b = Bits::new(&r);
        if !flag(&mut b)? {
            return None;
        }
        let irap = (16..=23).contains(&t);
        if irap {
            b.skip(1)?;
        }
        let pps = self.hevc_pps.get(&b.ue()?)?.clone();
        let sps = self.hevc_sps.get(&pps.sps)?.clone();
        let _ = pps.dependent_slices;
        b.skip(pps.extra_bits as usize)?;
        b.ue()?;
        if pps.output_flag {
            b.skip(1)?;
        }
        if sps.separate_planes {
            b.skip(2)?;
        }
        let idr = t == 19 || t == 20;
        // IDR, BLA, or a CRA that starts the stream: a new sequence.
        let reset = idr || (16..=18).contains(&t) || (t == 21 && self.first);
        self.first = false;
        let lsb = if idr { 0 } else { b.bits(sps.log2_max_poc_lsb)? as i64 };
        let max = 1i64 << sps.log2_max_poc_lsb;
        let msb = if reset {
            0
        } else if lsb < self.prev_lsb && self.prev_lsb - lsb >= max / 2 {
            self.prev_msb + max
        } else if lsb > self.prev_lsb && lsb - self.prev_lsb > max / 2 {
            self.prev_msb - max
        } else {
            self.prev_msb
        };
        // Pictures later ones count from: temporal layer 0, not RASL,
        // RADL or a sub-layer non-reference picture.
        let sub_layer_non_ref = t <= 14 && t % 2 == 0;
        if tid == 0 && !(6..=9).contains(&t) && !sub_layer_non_ref {
            self.prev_msb = msb;
            self.prev_lsb = lsb;
        }
        Some(Picture { poc: msb + lsb, reset, reorder: sps.reorder })
    }

    /// End of a sequence (a drain or a seek): the next IRAP starts anew.
    pub fn restart(&mut self) {
        self.first = true;
        self.prev_msb = 0;
        self.prev_lsb = 0;
        self.prev_frame_num = 0;
        self.prev_frame_num_offset = 0;
    }
}

/// Pictures waiting to go out in display order.
#[derive(Debug)]
pub struct Reorder<T> {
    held: Vec<(i64, u64, T)>,
    seq: u64,
}

impl<T> Default for Reorder<T> {
    fn default() -> Self {
        Reorder { held: Vec::new(), seq: 0 }
    }
}

impl<T> Reorder<T> {
    /// Take a decoded picture; returns those now due, in order.
    pub fn push(&mut self, poc: i64, depth: u32, item: T) -> Vec<T> {
        self.seq += 1;
        self.held.push((poc, self.seq, item));
        let mut out = Vec::new();
        while self.held.len() > depth.min(MAX_REORDER) as usize {
            out.push(self.pop_first());
        }
        out
    }

    fn pop_first(&mut self) -> T {
        let i = (0..self.held.len()).min_by_key(|&i| (self.held[i].0, self.held[i].1)).unwrap_or(0);
        self.held.remove(i).2
    }

    /// Everything held, in order.
    pub fn flush(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        while !self.held.is_empty() {
            out.push(self.pop_first());
        }
        out
    }

    pub fn clear(&mut self) {
        self.held.clear();
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorders() {
        let mut r = Reorder::default();
        // Decode order I0 P6 B2 B4 with depth 2: I0 goes once 3 are held.
        assert!(r.push(0, 2, "I0").is_empty());
        assert!(r.push(6, 2, "P6").is_empty());
        assert_eq!(r.push(2, 2, "B2"), vec!["I0"]);
        assert_eq!(r.push(4, 2, "B4"), vec!["B2"]);
        assert_eq!(r.flush(), vec!["B4", "P6"]);
        assert!(r.is_empty());
    }

    #[test]
    fn signed_golomb() {
        // ue 1 (010) -> +1, ue 2 (011) -> -1, ue 0 (1) -> 0
        let d = [0b0100_1110];
        let mut b = Bits::new(&d);
        assert_eq!(se(&mut b), Some(1));
        assert_eq!(se(&mut b), Some(-1));
        assert_eq!(se(&mut b), Some(0));
    }

    #[test]
    fn junk_never_panics() {
        let mut seed = 0x8765_4321u32;
        let mut p264 = Poc::new(Codec::H264);
        let mut p265 = Poc::new(Codec::Hevc);
        for len in 0..400 {
            let d: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (seed >> 16) as u8
                })
                .collect();
            for (p, h) in [(Param::Sps, 0x67u8), (Param::Pps, 0x68)] {
                let mut n = vec![h];
                n.extend_from_slice(&d);
                p264.param(p, &n);
            }
            for (p, h) in [(Param::Sps, 0x42u8), (Param::Pps, 0x44)] {
                let mut n = vec![h, 1];
                n.extend_from_slice(&d);
                p265.param(p, &n);
            }
            let mut n = vec![0x65];
            n.extend_from_slice(&d);
            p264.picture(&AccessUnit { nals: vec![&n] });
            let mut n = vec![0x02, 0x01];
            n.extend_from_slice(&d);
            p265.picture(&AccessUnit { nals: vec![&n] });
        }
    }
}
