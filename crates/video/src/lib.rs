//! Hardware video decoding for guests.
//!
//! The guest's video decoder is a virtio-media device in the VMM (a V4L2
//! stateful decoder, see `vroom_virtio::devices::media`). The decoding
//! itself happens in `vroom video-helper`, a sandboxed process using the
//! Mac's VideoToolbox. This crate is what both sides share:
//!
//! - [`proto`]: the messages between the VMM and the helper, checked on
//!   both ends;
//! - [`nal`]: splitting a guest's H.264 / HEVC stream into NAL units,
//!   access units and parameter sets, before VideoToolbox sees any of it;
//! - [`poc`]: display order for H.264 / HEVC;
//! - [`vpx`]: VP9 frames and AV1 temporal units, the headers VideoToolbox
//!   needs from them.
//!
//! Everything a guest sends is hostile: sizes and counts are capped here.
#![forbid(unsafe_code)]

pub mod nal;
pub mod poc;
pub mod proto;
pub mod vpx;

/// Coded formats the decoder takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vp9,
    Av1,
}

impl Codec {
    pub const ALL: [Codec; 4] = [Codec::H264, Codec::Hevc, Codec::Vp9, Codec::Av1];

    pub fn to_u8(self) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::Hevc => 2,
            Codec::Vp9 => 3,
            Codec::Av1 => 4,
        }
    }

    pub fn from_u8(v: u8) -> Option<Codec> {
        match v {
            1 => Some(Codec::H264),
            2 => Some(Codec::Hevc),
            3 => Some(Codec::Vp9),
            4 => Some(Codec::Av1),
            _ => None,
        }
    }

    /// Its bit in a set of codecs ([`proto::FromHelper::Caps`]).
    pub fn bit(self) -> u32 {
        1 << self.to_u8()
    }
}

/// How a decoded picture is laid out in the guest's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixFmt {
    /// 8 bits: the luma plane, then Cb and Cr interleaved at half height.
    Nv12,
    /// 10 bits in the top of 16-bit samples, planes as NV12.
    P010,
}

impl PixFmt {
    pub fn to_u8(self) -> u8 {
        match self {
            PixFmt::Nv12 => 1,
            PixFmt::P010 => 2,
        }
    }

    pub fn from_u8(v: u8) -> Option<PixFmt> {
        match v {
            1 => Some(PixFmt::Nv12),
            2 => Some(PixFmt::P010),
            _ => None,
        }
    }

    /// Bytes per sample.
    pub fn bytes(self) -> u32 {
        match self {
            PixFmt::Nv12 => 1,
            PixFmt::P010 => 2,
        }
    }

    /// The format that keeps a stream's bit depth.
    pub fn for_depth(depth: u32) -> PixFmt {
        if depth > 8 {
            PixFmt::P010
        } else {
            PixFmt::Nv12
        }
    }
}

/// Biggest picture: 4096 wide or high, and no more pixels than 4096x2304.
pub const MAX_SIDE: u32 = 4096;
pub const MAX_PIXELS: u32 = 4096 * 2304;
/// Smallest picture side we report.
pub const MIN_SIDE: u32 = 16;
/// Biggest picture in bytes (4096x2304 P010).
pub const MAX_PICTURE: u32 = (MAX_PIXELS + 16 * MAX_SIDE) * 3;
/// Biggest bitstream buffer (one guest OUTPUT buffer).
pub const MAX_BITSTREAM: u32 = 8 << 20;

/// Sizes of decoded pictures are multiples of this (H.264 macroblocks).
pub const ALIGN: u32 = 16;

/// A decoded picture's size in the guest's buffers: one plane, the luma
/// rows (`width` samples each, `height` of them) then the interleaved
/// chroma rows (`height / 2`), in a [`PixFmt`]. `width` and `height` are
/// multiples of [`ALIGN`]; the picture itself (`visible`) sits in the top
/// left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
}

impl Layout {
    /// The layout for a picture of `w` x `h`, if it is within limits.
    pub fn for_picture(w: u32, h: u32) -> Option<Layout> {
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return None;
        }
        let l = Layout { width: w.div_ceil(ALIGN) * ALIGN, height: h.div_ceil(ALIGN) * ALIGN };
        l.valid().then_some(l)
    }

    /// An encoder's raw frames: the guest's own size, any even one within
    /// limits, rows of `width` samples with nothing rounded up (players
    /// write frames packed, and FFmpeg takes the format's size for its
    /// frames' size).
    pub fn for_frame(w: u32, h: u32) -> Option<Layout> {
        let l = Layout { width: w, height: h };
        l.valid_frame().then_some(l)
    }

    /// Within limits and even (a frame layout from the other side).
    pub fn valid_frame(&self) -> bool {
        let (w, h) = (self.width, self.height);
        (MIN_SIDE..=MAX_SIDE).contains(&w) && (MIN_SIDE..=MAX_SIDE).contains(&h) && w % 2 == 0 && h % 2 == 0 && w * h <= MAX_PIXELS + 16 * MAX_SIDE
    }

    /// Within limits and aligned (a layout from the other side).
    pub fn valid(&self) -> bool {
        let (w, h) = (self.width, self.height);
        (MIN_SIDE..=MAX_SIDE).contains(&w)
            && (MIN_SIDE..=MAX_SIDE).contains(&h)
            && w % ALIGN == 0
            && h % ALIGN == 0
            && w * h <= MAX_PIXELS + 16 * MAX_SIDE
    }

    /// Bytes per luma row.
    pub fn stride(&self, f: PixFmt) -> u32 {
        self.width * f.bytes()
    }

    /// Bytes of one picture.
    pub fn size(&self, f: PixFmt) -> u32 {
        self.width * self.height * 3 / 2 * f.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts() {
        let l = Layout::for_picture(1920, 1080).unwrap();
        assert_eq!((l.width, l.height, l.size(PixFmt::Nv12)), (1920, 1088, 1920 * 1088 * 3 / 2));
        assert_eq!((l.stride(PixFmt::P010), l.size(PixFmt::P010)), (3840, 1920 * 1088 * 3));
        assert!(Layout::for_picture(4096, 2304).is_some());
        assert!(Layout::for_picture(4096, 4096).is_none());
        assert!(Layout::for_picture(0, 10).is_none());
        assert!(Layout::for_picture(5000, 10).is_none());
        assert_eq!(Layout::for_picture(1, 1), Some(Layout { width: 16, height: 16 }));
        assert!(!Layout { width: 17, height: 16 }.valid());
    }
}
