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
    pub const S_CTRL: u32 = 28;
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
    // Encoders.
    pub const G_PARM: u32 = 21;
    pub const S_PARM: u32 = 22;
    pub const G_EXT_CTRLS: u32 = 71;
    pub const S_EXT_CTRLS: u32 = 72;
    pub const TRY_EXT_CTRLS: u32 = 73;
    pub const ENUM_FRAMEINTERVALS: u32 = 75;
    pub const ENCODER_CMD: u32 = 77;
    pub const TRY_ENCODER_CMD: u32 = 78;
    pub const QUERY_EXT_CTRL: u32 = 103;
}

pub const CAPTURE_MPLANE: u32 = 9;
pub const OUTPUT_MPLANE: u32 = 10;
/// Selections and stream parameters name the queue without "multi-plane"
/// (the guest kernel rewrites it), but take both.
pub const CAPTURE: u32 = 1;
pub const OUTPUT: u32 = 2;

pub const MEMORY_MMAP: u32 = 1;
pub const FIELD_NONE: u32 = 1;

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}
pub const PIX_H264: u32 = fourcc(b"H264");
pub const PIX_HEVC: u32 = fourcc(b"HEVC");
pub const PIX_VP9: u32 = fourcc(b"VP90");
/// Not in Linux 6.8's headers: what later kernels and GStreamer call
/// `V4L2_PIX_FMT_AV1` (a stateful decoder's AV1 stream).
pub const PIX_AV1: u32 = fourcc(b"AV01");
pub const PIX_NV12: u32 = fourcc(b"NV12");
pub const PIX_P010: u32 = fourcc(b"P010");

pub const CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
pub const CAP_STREAMING: u32 = 0x0400_0000;
pub const CAP_EXT_PIX_FORMAT: u32 = 0x0020_0000;

pub const FMT_FLAG_COMPRESSED: u32 = 0x1;
pub const FMT_FLAG_DYN_RESOLUTION: u32 = 0x8;

pub const BUF_CAP_SUPPORTS_MMAP: u32 = 0x1;

pub const BUF_FLAG_QUEUED: u32 = 0x2;
pub const BUF_FLAG_KEYFRAME: u32 = 0x8;
pub const BUF_FLAG_PFRAME: u32 = 0x10;
pub const BUF_FLAG_ERROR: u32 = 0x40;
pub const BUF_FLAG_TIMESTAMP_COPY: u32 = 0x4000;
pub const BUF_FLAG_LAST: u32 = 0x0010_0000;

pub const EVENT_ALL: u32 = 0;
pub const EVENT_EOS: u32 = 2;
pub const EVENT_SOURCE_CHANGE: u32 = 5;
pub const SRC_CH_RESOLUTION: u32 = 1;

pub const DEC_CMD_START: u32 = 0;
pub const DEC_CMD_STOP: u32 = 1;
pub const ENC_CMD_START: u32 = 0;
pub const ENC_CMD_STOP: u32 = 1;

/// `v4l2_outputparm.capability`: frame intervals can be set.
pub const CAP_TIMEPERFRAME: u32 = 0x1000;

/// `v4l2_ext_controls.which`: current values, defaults; or a class.
pub const CTRL_WHICH_CUR_VAL: u32 = 0;
pub const CTRL_WHICH_DEF_VAL: u32 = 0x0f00_0000;
pub const CTRL_CLASS_USER: u32 = 0x0098_0000;
pub const CTRL_CLASS_CODEC: u32 = 0x0099_0000;

pub const CID_MIN_BUFFERS_FOR_CAPTURE: u32 = 0x0098_0927;
pub const CID_H264_LEVEL: u32 = 0x0099_0a67;
pub const CID_H264_PROFILE: u32 = 0x0099_0a6b;
pub const CID_HEVC_PROFILE: u32 = 0x0099_0b67;
pub const CID_HEVC_LEVEL: u32 = 0x0099_0b68;
pub const CID_VP9_PROFILE: u32 = 0x0099_0b00;
pub const CID_VP9_LEVEL: u32 = 0x0099_0b01;
pub const CID_DEC_DISPLAY_DELAY: u32 = 0x0099_0b8d;
pub const CID_DEC_DISPLAY_DELAY_ENABLE: u32 = 0x0099_0b8e;
pub const CID_AV1_PROFILE: u32 = 0x0099_0b8f;
pub const CID_AV1_LEVEL: u32 = 0x0099_0b90;
// Encoder controls.
pub const CID_MIN_BUFFERS_FOR_OUTPUT: u32 = 0x0098_0928;
pub const CID_B_FRAMES: u32 = 0x0099_09ca;
pub const CID_GOP_SIZE: u32 = 0x0099_09cb;
pub const CID_BITRATE_MODE: u32 = 0x0099_09ce;
pub const CID_BITRATE: u32 = 0x0099_09cf;
pub const CID_BITRATE_PEAK: u32 = 0x0099_09d0;
pub const CID_FRAME_RC_ENABLE: u32 = 0x0099_09d7;
pub const CID_HEADER_MODE: u32 = 0x0099_09d8;
pub const CID_REPEAT_SEQ_HEADER: u32 = 0x0099_09e2;
pub const CID_FORCE_KEY_FRAME: u32 = 0x0099_09e5;
pub const CID_H264_ENTROPY_MODE: u32 = 0x0099_0a65;
pub const CID_H264_I_PERIOD: u32 = 0x0099_0a66;
pub const CID_PREPEND_SPSPPS_TO_IDR: u32 = 0x0099_0b84;
pub const CID_CONSTANT_QUALITY: u32 = 0x0099_0b85;
pub const CTRL_TYPE_INTEGER: u32 = 1;
pub const CTRL_TYPE_BOOLEAN: u32 = 2;
pub const CTRL_TYPE_MENU: u32 = 3;
pub const CTRL_TYPE_BUTTON: u32 = 4;
pub const CTRL_FLAG_NEXT_CTRL: u32 = 0x8000_0000;
pub const CTRL_FLAG_NEXT_COMPOUND: u32 = 0x4000_0000;
pub const CTRL_FLAG_READ_ONLY: u32 = 0x4;
pub const CTRL_FLAG_WRITE_ONLY: u32 = 0x40;
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
pub const QUERY_EXT_CTRL_SIZE: usize = 232;
/// `struct v4l2_ext_controls`, and each (packed) `v4l2_ext_control`.
pub const EXT_CONTROLS_SIZE: usize = 32;
pub const EXT_CONTROL_SIZE: usize = 20;
pub const STREAMPARM_SIZE: usize = 204;
pub const FRMIVALENUM_SIZE: usize = 52;
pub const ENCODER_CMD_SIZE: usize = 40;

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

pub fn put_u64(b: &mut [u8], at: usize, v: u64) {
    if let Some(s) = b.get_mut(at..at + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
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
        assert_eq!(PIX_P010, 0x3031_3050);
        assert_eq!(PIX_VP9, 0x3039_5056);
    }
}
