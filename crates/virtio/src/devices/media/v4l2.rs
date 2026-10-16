//! The V4L2 structures virtio-media carries, as bytes. Layouts are those
//! of a 64-bit Linux guest (virtio-media always uses them). Only the
//! fields a decoder needs are read or written; the rest is left as the
//! guest sent it, or zero.

/// ioctl numbers (`_IOC_NR` of the `VIDIOC_*` codes).
pub mod ioc {
    pub const ENUM_FMT: u32 = 2;
    pub const G_FMT: u32 = 4;
    pub const S_FMT: u32 = 5;
    pub const REQBUFS: u32 = 8;
    pub const QUERYBUF: u32 = 9;
    pub const QBUF: u32 = 15;
    pub const STREAMON: u32 = 18;
    pub const STREAMOFF: u32 = 19;
    pub const G_CTRL: u32 = 27;
    pub const QUERYCTRL: u32 = 36;
    pub const QUERYMENU: u32 = 37;
    pub const TRY_FMT: u32 = 64;
    pub const ENUM_FRAMESIZES: u32 = 74;
    pub const SUBSCRIBE_EVENT: u32 = 90;
    pub const UNSUBSCRIBE_EVENT: u32 = 91;
    pub const G_SELECTION: u32 = 94;
    pub const S_SELECTION: u32 = 95;
    pub const DECODER_CMD: u32 = 96;
    pub const TRY_DECODER_CMD: u32 = 97;
}

pub const CAPTURE_MPLANE: u32 = 9;
pub const OUTPUT_MPLANE: u32 = 10;
/// Selections name the queue without "multi-plane" (the guest kernel
/// rewrites it), but take both.
pub const CAPTURE: u32 = 1;

pub const MEMORY_MMAP: u32 = 1;
pub const FIELD_NONE: u32 = 1;

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}
pub const PIX_H264: u32 = fourcc(b"H264");
pub const PIX_HEVC: u32 = fourcc(b"HEVC");
pub const PIX_NV12: u32 = fourcc(b"NV12");

pub const CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
pub const CAP_STREAMING: u32 = 0x0400_0000;
pub const CAP_EXT_PIX_FORMAT: u32 = 0x0020_0000;

pub const FMT_FLAG_COMPRESSED: u32 = 0x1;
pub const FMT_FLAG_DYN_RESOLUTION: u32 = 0x8;

pub const BUF_CAP_SUPPORTS_MMAP: u32 = 0x1;

pub const BUF_FLAG_QUEUED: u32 = 0x2;
pub const BUF_FLAG_ERROR: u32 = 0x40;
pub const BUF_FLAG_TIMESTAMP_COPY: u32 = 0x4000;
pub const BUF_FLAG_LAST: u32 = 0x0010_0000;

pub const EVENT_ALL: u32 = 0;
pub const EVENT_EOS: u32 = 2;
pub const EVENT_SOURCE_CHANGE: u32 = 5;
pub const SRC_CH_RESOLUTION: u32 = 1;

pub const DEC_CMD_START: u32 = 0;
pub const DEC_CMD_STOP: u32 = 1;

pub const CID_MIN_BUFFERS_FOR_CAPTURE: u32 = 0x0098_0927;
pub const CID_H264_LEVEL: u32 = 0x0099_0a67;
pub const CID_H264_PROFILE: u32 = 0x0099_0a6b;
pub const CID_HEVC_PROFILE: u32 = 0x0099_0b67;
pub const CID_HEVC_LEVEL: u32 = 0x0099_0b68;
pub const CTRL_TYPE_INTEGER: u32 = 1;
pub const CTRL_TYPE_MENU: u32 = 3;
pub const CTRL_FLAG_NEXT_CTRL: u32 = 0x8000_0000;
pub const CTRL_FLAG_NEXT_COMPOUND: u32 = 0x4000_0000;
pub const CTRL_FLAG_READ_ONLY: u32 = 0x4;
pub const CTRL_FLAG_VOLATILE: u32 = 0x80;

pub const SEL_TGT_CROP: u32 = 0;
pub const SEL_TGT_CROP_DEFAULT: u32 = 1;
pub const SEL_TGT_CROP_BOUNDS: u32 = 2;
pub const SEL_TGT_NATIVE_SIZE: u32 = 3;
pub const SEL_TGT_COMPOSE: u32 = 0x100;
pub const SEL_TGT_COMPOSE_DEFAULT: u32 = 0x101;
pub const SEL_TGT_COMPOSE_BOUNDS: u32 = 0x102;
pub const SEL_TGT_COMPOSE_PADDED: u32 = 0x103;

pub const COLORSPACE_REC709: u32 = 3;
pub const YCBCR_ENC_709: u8 = 2;
pub const QUANTIZATION_LIM_RANGE: u8 = 2;
pub const XFER_FUNC_709: u8 = 1;

/// Struct sizes.
pub const FORMAT_SIZE: usize = 208;
pub const FMTDESC_SIZE: usize = 64;
pub const FRMSIZEENUM_SIZE: usize = 44;
pub const REQBUFS_SIZE: usize = 20;
pub const BUFFER_SIZE: usize = 88;
pub const PLANE_SIZE: usize = 64;
pub const MAX_PLANES: usize = 8;
pub const EVENT_SIZE: usize = 136;
pub const SUBSCRIPTION_SIZE: usize = 32;
pub const SELECTION_SIZE: usize = 64;
pub const DECODER_CMD_SIZE: usize = 72;
pub const CONTROL_SIZE: usize = 8;
pub const QUERYCTRL_SIZE: usize = 68;
pub const QUERYMENU_SIZE: usize = 44;

pub fn get_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub fn put_u32(b: &mut [u8], at: usize, v: u32) {
    if let Some(s) = b.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

pub fn put_i32(b: &mut [u8], at: usize, v: i32) {
    put_u32(b, at, v as u32);
}

pub fn put_u8(b: &mut [u8], at: usize, v: u8) {
    if let Some(x) = b.get_mut(at) {
        *x = v;
    }
}

pub fn put_str(b: &mut [u8], at: usize, len: usize, s: &str) {
    if let Some(d) = b.get_mut(at..at + len) {
        d.fill(0);
        let n = s.len().min(len - 1);
        d[..n].copy_from_slice(&s.as_bytes()[..n]);
    }
}

/// `struct v4l2_format` with a `v4l2_pix_format_mplane` inside.
pub mod fmt {
    pub const TYPE: usize = 0;
    pub const WIDTH: usize = 8;
    pub const HEIGHT: usize = 12;
    pub const PIXELFORMAT: usize = 16;
    pub const FIELD: usize = 20;
    pub const COLORSPACE: usize = 24;
    pub const SIZEIMAGE: usize = 28;
    pub const BYTESPERLINE: usize = 32;
    pub const NUM_PLANES: usize = 188;
    pub const FLAGS: usize = 189;
    pub const YCBCR_ENC: usize = 190;
    pub const QUANTIZATION: usize = 191;
    pub const XFER_FUNC: usize = 192;
}

/// `struct v4l2_buffer`.
pub mod buf {
    pub const INDEX: usize = 0;
    pub const TYPE: usize = 4;
    pub const BYTESUSED: usize = 8;
    pub const FLAGS: usize = 12;
    pub const FIELD: usize = 16;
    pub const TIMESTAMP: usize = 24;
    pub const SEQUENCE: usize = 56;
    pub const MEMORY: usize = 60;
    pub const M: usize = 64;
    pub const LENGTH: usize = 72;
}

/// `struct v4l2_plane`.
pub mod plane {
    pub const BYTESUSED: usize = 0;
    pub const LENGTH: usize = 4;
    pub const MEM_OFFSET: usize = 8;
    pub const DATA_OFFSET: usize = 16;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourccs() {
        assert_eq!(PIX_NV12, 0x3231_564e);
        assert_eq!(PIX_H264, 0x3436_3248);
        assert_eq!(PIX_HEVC, 0x4356_4548);
    }
}
