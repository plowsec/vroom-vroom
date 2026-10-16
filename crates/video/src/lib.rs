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
//!   access units and parameter sets, before VideoToolbox sees any of it.
//!
//! Everything a guest sends is hostile: sizes and counts are capped here.
#![forbid(unsafe_code)]

pub mod nal;
pub mod poc;
pub mod proto;

/// Coded formats the decoder takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub fn to_u8(self) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::Hevc => 2,
        }
    }

    pub fn from_u8(v: u8) -> Option<Codec> {
        match v {
            1 => Some(Codec::H264),
            2 => Some(Codec::Hevc),
            _ => None,
        }
    }
}

/// Biggest picture: 4096 wide or high, and no more pixels than 4096x2304.
pub const MAX_SIDE: u32 = 4096;
pub const MAX_PIXELS: u32 = 4096 * 2304;
/// Smallest picture side we report.
pub const MIN_SIDE: u32 = 16;
/// Biggest bitstream buffer (one guest OUTPUT buffer).
pub const MAX_BITSTREAM: u32 = 8 << 20;

/// Sizes of decoded pictures are multiples of this (H.264 macroblocks).
pub const ALIGN: u32 = 16;

/// A decoded picture's size in the guest's buffers: NV12, one plane, the
/// luma rows (`width` bytes each, `height` of them) then the interleaved
/// chroma rows (`height / 2`). `width` and `height` are multiples of
/// [`ALIGN`]; the picture itself (`visible`) sits in the top left.
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
    pub fn stride(&self) -> u32 {
        self.width
    }

    /// Bytes of one picture.
    pub fn size(&self) -> u32 {
        self.width * self.height * 3 / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts() {
        let l = Layout::for_picture(1920, 1080).unwrap();
        assert_eq!((l.width, l.height, l.size()), (1920, 1088, 1920 * 1088 * 3 / 2));
        assert!(Layout::for_picture(4096, 2304).is_some());
        assert!(Layout::for_picture(4096, 4096).is_none());
        assert!(Layout::for_picture(0, 10).is_none());
        assert!(Layout::for_picture(5000, 10).is_none());
        assert_eq!(Layout::for_picture(1, 1), Some(Layout { width: 16, height: 16 }));
        assert!(!Layout { width: 17, height: 16 }.valid());
    }
}
