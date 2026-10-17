// SPDX-License-Identifier: MIT
//
// vroom_drv_video.so: a VA-API driver for Linux guests of vroom. It
// decodes on the Mac (VideoToolbox) through the VM's V4L2 video decoder
// (virtio-media, /dev/videoN "Video decoder").
//
// VA-API is the interface browsers use (Firefox, Chromium). It hands the
// driver pictures already parsed: parameters and slices. The V4L2 decoder
// takes a plain bitstream. So for each picture this driver writes the
// parameter sets back out from VA-API's parameters (H.264: SPS and PPS),
// puts the slices after them and queues that as one bitstream buffer.
// HEVC slice headers name reference picture sets VA-API doesn't pass, so
// those headers are rewritten to carry their sets. VP9 frames go as they
// are. The decoder is asked for pictures in decoding
// order (display delay 0), as soon as each is done: the player reorders.
//
// Decoded pictures land in our V4L2 buffers; the driver copies each into
// its VA surface's memory. For a player that wants the picture as a
// dma-buf (vaExportSurfaceHandle: Firefox, mpv), the surface also gets
// two GBM buffers on the virtio-gpu render node (luma, chroma), filled
// on export, which the player's OpenGL imports. Chrome exports each
// surface once, empty, and keeps the dma-buf: those are filled at each
// vaEndPicture instead.
//
// Build: make (needs libva-dev, libdrm-dev, libgbm-dev). Install with
// install-vaapi.sh; use with LIBVA_DRIVER_NAME=vroom.
//
// This runs in the guest, in the player's process. The host trusts none
// of it: the VMM checks every V4L2 request.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <linux/videodev2.h>

#include <drm/virtgpu_drm.h>
#include <drm_fourcc.h>
#include <gbm.h>
#include <va/va.h>
#include <va/va_backend.h>
#include <va/va_drmcommon.h>

#ifndef V4L2_PIX_FMT_AV1
#define V4L2_PIX_FMT_AV1 v4l2_fourcc('A', 'V', '0', '1')
#endif

#define VENDOR "vroom (VideoToolbox on the Mac, through virtio-media)"
// Biggest PPS we write (HEVC with all its scaling lists explicit).
#define PPS_MAX 2048
#define MAX_PROFILES 16
#define MAX_ENTRYPOINTS 2
#define MAX_ATTRIBUTES 8
#define MAX_IMAGE_FORMATS 2
#define OUTPUT_BUFS 8
#define MAX_CAPTURE_BUFS 24
// How long a picture may take before we give up on it.
#define SYNC_TIMEOUT_MS 3000

static int debug_level = -1;

static void dbg(const char *fmt, ...)
{
	if (debug_level < 0) {
		const char *e = getenv("VROOM_VA_DEBUG");
		debug_level = e ? atoi(e) : 0;
	}
	if (!debug_level)
		return;
	va_list ap;
	va_start(ap, fmt);
	fprintf(stderr, "vroom-va: ");
	vfprintf(stderr, fmt, ap);
	fputc('\n', stderr);
	va_end(ap);
}

// ---------------------------------------------------------------------
// Objects

enum { OBJ_NONE, OBJ_CONFIG, OBJ_CONTEXT, OBJ_SURFACE, OBJ_BUFFER, OBJ_IMAGE };

struct config {
	VAProfile profile;
	VAEntrypoint entrypoint;
	unsigned rt_format;
};

struct context;

struct surface {
	unsigned width, height;
	bool p010;
	// Our copy of the picture: luma rows, then interleaved chroma rows.
	uint8_t *data;
	unsigned pitch, rows; // bytes per row, luma rows
	size_t size;
	// Decoding: queued to the decoder as picture `seq` of `ctx`.
	bool pending;
	bool failed;
	uint64_t seq;
	struct context *ctx;
	// For export: luma and chroma planes as GBM buffers, and our
	// mapping of each (virtio-gpu: its guest pages).
	struct gbm_bo *bo[2];
	uint8_t *bo_map[2];
	size_t bo_map_len[2];
	bool bo_stale;
	// Exported before it held a picture: the player keeps the dma-buf and
	// expects it to follow the surface, as a GPU decoder's would
	// (Chromium). Each picture then goes into the buffers as it is decoded.
	bool live;
};

struct buffer {
	VABufferType type;
	unsigned size, num;
	uint8_t *data;
	bool borrowed; // data is a surface's (a derived image)
};

struct image {
	VAImage image;
	VASurfaceID derived;
};

struct obj {
	int type;
	void *p;
};

// The H.264 PPS defaults we can only learn from slice headers.
struct h264_pps_state {
	bool known_l0, known_l1;
	unsigned l0, l1;
};

struct context {
	struct driver *drv;
	VAProfile profile;
	unsigned width, height;
	bool p010;
	VASurfaceID target;
	// The picture being built: its slices (Annex B), then all of it.
	uint8_t *slices;
	size_t slices_len, slices_cap;
	bool pps_used[256];
	uint8_t *au;
	size_t au_len, au_cap;
	VAPictureParameterBufferH264 h264_pic;
	VAIQMatrixBufferH264 h264_iq;
	bool have_pic, have_iq;
	VADecPictureParameterBufferVP9 vp9_pic;
	VAPictureParameterBufferHEVC hevc_pic;
	VAIQMatrixBufferHEVC hevc_iq;
	// Parameter sets sent so far (H.264: SPS; HEVC: VPS and SPS), to send
	// them only when they change.
	uint8_t last_sps[256];
	size_t last_sps_len;
	uint8_t last_pps[256][PPS_MAX];
	uint16_t last_pps_len[256];
	struct h264_pps_state pps_state[256];
	// V4L2
	int fd;
	bool out_on, cap_on, failed;
	void *out_map[OUTPUT_BUFS];
	size_t out_len[OUTPUT_BUFS];
	bool out_busy[OUTPUT_BUFS];
	unsigned n_out;
	void *cap_map[MAX_CAPTURE_BUFS];
	size_t cap_len[MAX_CAPTURE_BUFS];
	unsigned n_cap;
	unsigned cap_w, cap_h, cap_pitch;
	bool cap_p010;
	uint64_t next_seq;
};

struct driver {
	pthread_mutex_t lock;
	int drm_fd;
	struct gbm_device *gbm;
	char video[64];
	uint32_t coded[8];
	unsigned n_coded;
	bool p010;
	struct obj *objs;
	unsigned n_objs, cap_objs;
};

static unsigned new_obj(struct driver *d, int type, void *p)
{
	for (unsigned i = 0; i < d->n_objs; i++) {
		if (d->objs[i].type == OBJ_NONE) {
			d->objs[i] = (struct obj){ type, p };
			return i + 1;
		}
	}
	if (d->n_objs == d->cap_objs) {
		unsigned n = d->cap_objs ? d->cap_objs * 2 : 256;
		struct obj *o = realloc(d->objs, n * sizeof(*o));
		if (!o)
			return VA_INVALID_ID;
		d->objs = o;
		d->cap_objs = n;
	}
	d->objs[d->n_objs] = (struct obj){ type, p };
	return ++d->n_objs;
}

static void *get_obj(struct driver *d, unsigned id, int type)
{
	if (id == 0 || id > d->n_objs || d->objs[id - 1].type != type)
		return NULL;
	return d->objs[id - 1].p;
}

static void del_obj(struct driver *d, unsigned id)
{
	if (id > 0 && id <= d->n_objs)
		d->objs[id - 1] = (struct obj){ OBJ_NONE, NULL };
}

#define DRV(ctx) ((struct driver *)(ctx)->pDriverData)

static void bo_free(struct surface *s);
static bool upload(struct driver *d, struct surface *s);

// ---------------------------------------------------------------------
// Bit writer: RBSP, then NAL units with emulation prevention.

struct bw {
	uint8_t buf[4096];
	size_t len;
	uint32_t acc;
	int n;
	bool overflow;
};

static void put_bits(struct bw *b, uint32_t v, int n)
{
	for (int i = n - 1; i >= 0; i--) {
		b->acc = (b->acc << 1) | ((v >> i) & 1);
		if (++b->n == 8) {
			if (b->len < sizeof(b->buf))
				b->buf[b->len++] = (uint8_t)b->acc;
			else
				b->overflow = true;
			b->acc = 0;
			b->n = 0;
		}
	}
}

static void put_ue(struct bw *b, uint32_t v)
{
	uint64_t x = (uint64_t)v + 1;
	int len = 0;
	while ((x >> len) > 1)
		len++;
	put_bits(b, 0, len);
	put_bits(b, (uint32_t)x, len + 1);
}

static void put_se(struct bw *b, int32_t v)
{
	put_ue(b, v > 0 ? 2 * (uint32_t)v - 1 : 2 * (uint32_t)(-(int64_t)v));
}

static void put_trailing(struct bw *b)
{
	put_bits(b, 1, 1);
	while (b->n)
		put_bits(b, 0, 1);
}

static bool grow(uint8_t **buf, size_t *cap, size_t len, size_t more)
{
	if (len + more <= *cap)
		return true;
	if (len + more > (64u << 20))
		return false;
	size_t n = *cap ? *cap : 1 << 20;
	while (n < len + more)
		n *= 2;
	uint8_t *p = realloc(*buf, n);
	if (!p)
		return false;
	*buf = p;
	*cap = n;
	return true;
}

static bool au_grow(struct context *c, size_t more)
{
	return grow(&c->au, &c->au_cap, c->au_len, more);
}

// Append a NAL unit: start code, header byte(s), escaped payload.
static bool au_nal(struct context *c, const uint8_t *hdr, size_t hlen, const uint8_t *rbsp, size_t len)
{
	if (!au_grow(c, 4 + hlen + len * 3 / 2 + 4))
		return false;
	uint8_t *o = c->au + c->au_len;
	size_t k = 0;
	o[k++] = 0, o[k++] = 0, o[k++] = 0, o[k++] = 1;
	memcpy(o + k, hdr, hlen);
	k += hlen;
	int zeros = 0;
	for (size_t i = 0; i < len; i++) {
		if (zeros >= 2 && rbsp[i] <= 3) {
			o[k++] = 3;
			zeros = 0;
		}
		o[k++] = rbsp[i];
		zeros = rbsp[i] == 0 ? zeros + 1 : 0;
	}
	c->au_len += k;
	return true;
}

// Append to the picture's slices (with a start code: a NAL unit).
static bool slice_add(struct context *c, const uint8_t *data, size_t len, bool start_code)
{
	if (!grow(&c->slices, &c->slices_cap, c->slices_len, len + 3))
		return false;
	if (start_code) {
		c->slices[c->slices_len++] = 0;
		c->slices[c->slices_len++] = 0;
		c->slices[c->slices_len++] = 1;
	}
	memcpy(c->slices + c->slices_len, data, len);
	c->slices_len += len;
	return true;
}

// Bit reader over a NAL unit's payload, skipping emulation prevention.
struct br {
	const uint8_t *p;
	size_t len, pos; // pos in bits over the unescaped stream
	uint8_t rbsp[96];
	size_t rlen;
	bool bad;
};

static void br_init(struct br *r, const uint8_t *nal, size_t len)
{
	r->rlen = 0;
	int zeros = 0;
	for (size_t i = 0; i < len && r->rlen < sizeof(r->rbsp); i++) {
		if (zeros >= 2 && nal[i] == 3) {
			zeros = 0;
			continue;
		}
		zeros = nal[i] == 0 ? zeros + 1 : 0;
		r->rbsp[r->rlen++] = nal[i];
	}
	r->pos = 0;
	r->bad = false;
}

static uint32_t get_bits(struct br *r, int n)
{
	uint32_t v = 0;
	for (int i = 0; i < n; i++) {
		if (r->pos >= r->rlen * 8) {
			r->bad = true;
			return 0;
		}
		v = (v << 1) | ((r->rbsp[r->pos / 8] >> (7 - r->pos % 8)) & 1);
		r->pos++;
	}
	return v;
}

static uint32_t get_ue(struct br *r)
{
	int zeros = 0;
	while (!r->bad && get_bits(r, 1) == 0) {
		if (++zeros > 31) {
			r->bad = true;
			return 0;
		}
	}
	return (uint32_t)((1ull << zeros) - 1 + get_bits(r, zeros));
}

// ---------------------------------------------------------------------
// H.264: parameter sets back out of VA-API's picture parameters.

static const uint8_t zigzag4[16] = { 0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15 };
static const uint8_t zigzag8[64] = {
	0,  1,  8,  16, 9,  2,  3,  10, 17, 24, 32, 25, 18, 11, 4,  5,  12, 19, 26, 33, 40, 48,
	41, 34, 27, 20, 13, 6,  7,  14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23,
	30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
};

static bool h264_high(VAProfile p)
{
	return p == VAProfileH264High;
}

static void h264_sps(struct context *c, struct bw *b)
{
	const VAPictureParameterBufferH264 *pp = &c->h264_pic;
	unsigned profile_idc = c->profile == VAProfileH264ConstrainedBaseline ? 66 : c->profile == VAProfileH264Main ? 77 : 100;
	put_bits(b, profile_idc, 8);
	put_bits(b, profile_idc == 66 ? 0x40 : 0, 8); // constraint_set1 for Constrained Baseline
	put_bits(b, 51, 8);                          // level 5.1
	put_ue(b, 0);                                // seq_parameter_set_id
	if (profile_idc == 100) {
		put_ue(b, pp->seq_fields.bits.chroma_format_idc);
		if (pp->seq_fields.bits.chroma_format_idc == 3)
			put_bits(b, pp->seq_fields.bits.residual_colour_transform_flag, 1);
		put_ue(b, pp->bit_depth_luma_minus8);
		put_ue(b, pp->bit_depth_chroma_minus8);
		put_bits(b, 0, 1); // qpprime_y_zero_transform_bypass_flag
		put_bits(b, 0, 1); // seq_scaling_matrix_present_flag (the PPS has them)
	}
	put_ue(b, pp->seq_fields.bits.log2_max_frame_num_minus4);
	put_ue(b, pp->seq_fields.bits.pic_order_cnt_type);
	if (pp->seq_fields.bits.pic_order_cnt_type == 0) {
		put_ue(b, pp->seq_fields.bits.log2_max_pic_order_cnt_lsb_minus4);
	} else if (pp->seq_fields.bits.pic_order_cnt_type == 1) {
		// VA-API doesn't pass the offsets: the order counts of streams
		// using type 1 come out wrong (rare: no common encoder uses it).
		put_bits(b, pp->seq_fields.bits.delta_pic_order_always_zero_flag, 1);
		put_se(b, 0);
		put_se(b, 0);
		put_ue(b, 0);
	}
	put_ue(b, pp->num_ref_frames);
	put_bits(b, pp->seq_fields.bits.gaps_in_frame_num_value_allowed_flag, 1);
	put_ue(b, pp->picture_width_in_mbs_minus1);
	unsigned mbs = pp->picture_height_in_mbs_minus1 + 1u;
	// Map units: frames' macroblock rows, or field rows for interlaced
	// streams (VA-API gives the field's height for field pictures).
	unsigned units = pp->seq_fields.bits.frame_mbs_only_flag || pp->pic_fields.bits.field_pic_flag ? mbs : mbs / 2;
	put_ue(b, units ? units - 1 : 0);
	put_bits(b, pp->seq_fields.bits.frame_mbs_only_flag, 1);
	if (!pp->seq_fields.bits.frame_mbs_only_flag)
		put_bits(b, pp->seq_fields.bits.mb_adaptive_frame_field_flag, 1);
	put_bits(b, pp->seq_fields.bits.direct_8x8_inference_flag, 1);
	put_bits(b, 0, 1); // frame_cropping_flag: the player crops
	// VUI with only a bitstream restriction: no reordering. The decoder
	// then hands each picture out as soon as it is decoded, in decoding
	// order, which is what VA-API wants (the player reorders). It changes
	// when pictures are output, not how they decode.
	put_bits(b, 1, 1); // vui_parameters_present_flag
	put_bits(b, 0, 8); // aspect ratio, overscan, signal type, chroma loc, timing, HRDs, pic_struct
	put_bits(b, 1, 1); // bitstream_restriction_flag
	put_bits(b, 1, 1); // motion_vectors_over_pic_boundaries_flag
	put_ue(b, 0);      // max_bytes_per_pic_denom
	put_ue(b, 0);      // max_bits_per_mb_denom
	put_ue(b, 16);     // log2_max_mv_length_horizontal
	put_ue(b, 16);     // log2_max_mv_length_vertical
	put_ue(b, 0);      // max_num_reorder_frames
	put_ue(b, pp->num_ref_frames ? pp->num_ref_frames : 1); // max_dec_frame_buffering
	put_trailing(b);
}

static bool iq_flat(const VAIQMatrixBufferH264 *iq)
{
	for (int i = 0; i < 6; i++)
		for (int j = 0; j < 16; j++)
			if (iq->ScalingList4x4[i][j] != 16)
				return false;
	for (int i = 0; i < 2; i++)
		for (int j = 0; j < 64; j++)
			if (iq->ScalingList8x8[i][j] != 16)
				return false;
	return true;
}

// A scaling list, from VA-API's raster order to the bitstream's zigzag.
static void put_scaling_list(struct bw *b, const uint8_t *raster, const uint8_t *zigzag, int n)
{
	int last = 8;
	for (int j = 0; j < n; j++) {
		int v = raster[zigzag[j]];
		int delta = v - last;
		if (delta > 127)
			delta -= 256;
		if (delta < -128)
			delta += 256;
		put_se(b, delta);
		last = v;
	}
}

static void h264_pps(struct context *c, struct bw *b, unsigned pps_id, unsigned l0, unsigned l1)
{
	const VAPictureParameterBufferH264 *pp = &c->h264_pic;
	put_ue(b, pps_id);
	put_ue(b, 0); // seq_parameter_set_id
	put_bits(b, pp->pic_fields.bits.entropy_coding_mode_flag, 1);
	put_bits(b, pp->pic_fields.bits.pic_order_present_flag, 1);
	put_ue(b, 0); // num_slice_groups_minus1 (no FMO in VA-API)
	put_ue(b, l0);
	put_ue(b, l1);
	put_bits(b, pp->pic_fields.bits.weighted_pred_flag, 1);
	put_bits(b, pp->pic_fields.bits.weighted_bipred_idc, 2);
	put_se(b, pp->pic_init_qp_minus26);
	put_se(b, pp->pic_init_qs_minus26);
	put_se(b, pp->chroma_qp_index_offset);
	put_bits(b, pp->pic_fields.bits.deblocking_filter_control_present_flag, 1);
	put_bits(b, pp->pic_fields.bits.constrained_intra_pred_flag, 1);
	put_bits(b, pp->pic_fields.bits.redundant_pic_cnt_present_flag, 1);
	bool scaling = c->have_iq && !iq_flat(&c->h264_iq);
	if (h264_high(c->profile) &&
	    (pp->pic_fields.bits.transform_8x8_mode_flag || scaling || pp->second_chroma_qp_index_offset != pp->chroma_qp_index_offset)) {
		bool t8 = pp->pic_fields.bits.transform_8x8_mode_flag;
		put_bits(b, t8, 1);
		put_bits(b, scaling, 1);
		if (scaling) {
			for (int i = 0; i < 6; i++) {
				put_bits(b, 1, 1);
				put_scaling_list(b, c->h264_iq.ScalingList4x4[i], zigzag4, 16);
			}
			for (int i = 0; t8 && i < 2; i++) {
				put_bits(b, 1, 1);
				put_scaling_list(b, c->h264_iq.ScalingList8x8[i], zigzag8, 64);
			}
		}
		put_se(b, pp->second_chroma_qp_index_offset);
	}
	put_trailing(b);
}

// What the slice header says: its PPS, and whether it overrides the
// PPS's reference counts. -1: couldn't read it.
static int h264_slice_info(struct context *c, const uint8_t *nal, size_t len, bool *uses_l0, bool *uses_l1)
{
	const VAPictureParameterBufferH264 *pp = &c->h264_pic;
	*uses_l0 = *uses_l1 = false;
	if (len < 2)
		return -1;
	unsigned type = nal[0] & 0x1f;
	struct br r;
	br_init(&r, nal + 1, len - 1);
	get_ue(&r); // first_mb_in_slice
	unsigned slice_type = get_ue(&r) % 5;
	unsigned pps_id = get_ue(&r);
	if (r.bad || pps_id > 255)
		return -1;
	if (pp->seq_fields.bits.chroma_format_idc == 3 && pp->seq_fields.bits.residual_colour_transform_flag)
		get_bits(&r, 2);
	get_bits(&r, pp->seq_fields.bits.log2_max_frame_num_minus4 + 4);
	bool field = false;
	if (!pp->seq_fields.bits.frame_mbs_only_flag) {
		field = get_bits(&r, 1);
		if (field)
			get_bits(&r, 1);
	}
	if (type == 5)
		get_ue(&r);
	if (pp->seq_fields.bits.pic_order_cnt_type == 0) {
		get_bits(&r, pp->seq_fields.bits.log2_max_pic_order_cnt_lsb_minus4 + 4);
		if (pp->pic_fields.bits.pic_order_present_flag && !field)
			get_ue(&r);
	} else if (pp->seq_fields.bits.pic_order_cnt_type == 1 && !pp->seq_fields.bits.delta_pic_order_always_zero_flag) {
		get_ue(&r);
		if (pp->pic_fields.bits.pic_order_present_flag && !field)
			get_ue(&r);
	}
	if (pp->pic_fields.bits.redundant_pic_cnt_present_flag)
		get_ue(&r);
	if (slice_type == 1)
		get_bits(&r, 1); // direct_spatial_mv_pred_flag
	if (slice_type == 0 || slice_type == 1 || slice_type == 3) {
		bool override = get_bits(&r, 1);
		*uses_l0 = !override;
		*uses_l1 = !override && slice_type == 1;
	}
	return r.bad ? -1 : (int)pps_id;
}

// ---------------------------------------------------------------------
// V4L2

static int xioctl(int fd, unsigned long req, void *arg)
{
	int r;
	do
		r = ioctl(fd, req, arg);
	while (r < 0 && errno == EINTR);
	return r;
}

// Find our decoder: an M2M device whose card is "Video decoder" on
// virtio-media. VROOM_VIDEO_DEVICE overrides.
static bool find_device(struct driver *d)
{
	const char *env = getenv("VROOM_VIDEO_DEVICE");
	for (int i = 0; i < 64; i++) {
		char path[64];
		if (env)
			snprintf(path, sizeof(path), "%s", env);
		else
			snprintf(path, sizeof(path), "/dev/video%d", i);
		int fd = open(path, O_RDWR | O_CLOEXEC);
		if (fd >= 0) {
			struct v4l2_capability cap = { 0 };
			bool ok = xioctl(fd, VIDIOC_QUERYCAP, &cap) == 0 && (cap.device_caps & V4L2_CAP_VIDEO_M2M_MPLANE) &&
				  strstr((const char *)cap.bus_info, "virtio-media") && !strcmp((const char *)cap.card, "Video decoder");
			if (ok) {
				snprintf(d->video, sizeof(d->video), "%s", path);
				d->n_coded = 0;
				for (unsigned k = 0; k < 8; k++) {
					struct v4l2_fmtdesc f = { .index = k, .type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE };
					if (xioctl(fd, VIDIOC_ENUM_FMT, &f))
						break;
					d->coded[d->n_coded++] = f.pixelformat;
				}
				for (unsigned k = 0; k < 4; k++) {
					struct v4l2_fmtdesc f = { .index = k, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE };
					if (xioctl(fd, VIDIOC_ENUM_FMT, &f))
						break;
					if (f.pixelformat == V4L2_PIX_FMT_P010)
						d->p010 = true;
				}
				close(fd);
				dbg("decoder %s, %u coded formats", path, d->n_coded);
				return true;
			}
			close(fd);
		}
		if (env)
			break;
	}
	return false;
}

static bool has_coded(struct driver *d, uint32_t fourcc)
{
	for (unsigned i = 0; i < d->n_coded; i++)
		if (d->coded[i] == fourcc)
			return true;
	return false;
}

static uint32_t profile_fourcc(VAProfile p)
{
	switch (p) {
	case VAProfileH264ConstrainedBaseline:
	case VAProfileH264Main:
	case VAProfileH264High:
		return V4L2_PIX_FMT_H264;
	case VAProfileVP9Profile0:
	case VAProfileVP9Profile2:
		return V4L2_PIX_FMT_VP9;
	case VAProfileHEVCMain:
	case VAProfileHEVCMain10:
		return V4L2_PIX_FMT_HEVC;
	default:
		return 0;
	}
}

static bool profile_10bit(VAProfile p)
{
	return p == VAProfileVP9Profile2 || p == VAProfileHEVCMain10;
}

static void cap_release(struct context *c)
{
	for (unsigned i = 0; i < c->n_cap; i++)
		if (c->cap_map[i])
			munmap(c->cap_map[i], c->cap_len[i]);
	c->n_cap = 0;
}

// The decoder found the stream's size: make the picture buffers.
static bool cap_setup(struct context *c)
{
	if (c->cap_on) {
		int t = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
		xioctl(c->fd, VIDIOC_STREAMOFF, &t);
		cap_release(c);
		struct v4l2_requestbuffers rb = { .count = 0, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, .memory = V4L2_MEMORY_MMAP };
		xioctl(c->fd, VIDIOC_REQBUFS, &rb);
		c->cap_on = false;
	}
	struct v4l2_format f = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE };
	if (xioctl(c->fd, VIDIOC_G_FMT, &f))
		return false;
	// 10-bit streams in P010 (the surfaces are), else NV12.
	uint32_t want = c->p010 ? V4L2_PIX_FMT_P010 : V4L2_PIX_FMT_NV12;
	if (f.fmt.pix_mp.pixelformat != want) {
		f.fmt.pix_mp.pixelformat = want;
		if (xioctl(c->fd, VIDIOC_S_FMT, &f))
			return false;
	}
	c->cap_w = f.fmt.pix_mp.width;
	c->cap_h = f.fmt.pix_mp.height;
	c->cap_pitch = f.fmt.pix_mp.plane_fmt[0].bytesperline;
	c->cap_p010 = f.fmt.pix_mp.pixelformat == V4L2_PIX_FMT_P010;
	struct v4l2_control ctl = { .id = V4L2_CID_MIN_BUFFERS_FOR_CAPTURE };
	unsigned min = xioctl(c->fd, VIDIOC_G_CTRL, &ctl) ? 4 : (unsigned)ctl.value;
	struct v4l2_requestbuffers rb = { .count = min + 6, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, .memory = V4L2_MEMORY_MMAP };
	if (rb.count > MAX_CAPTURE_BUFS)
		rb.count = MAX_CAPTURE_BUFS;
	if (xioctl(c->fd, VIDIOC_REQBUFS, &rb) || rb.count == 0)
		return false;
	c->n_cap = rb.count;
	for (unsigned i = 0; i < c->n_cap; i++) {
		struct v4l2_plane pl[1] = { 0 };
		struct v4l2_buffer b = { .index = i, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, .memory = V4L2_MEMORY_MMAP, .m.planes = pl, .length = 1 };
		if (xioctl(c->fd, VIDIOC_QUERYBUF, &b))
			return false;
		c->cap_len[i] = pl[0].length;
		c->cap_map[i] = mmap(NULL, pl[0].length, PROT_READ, MAP_SHARED, c->fd, pl[0].m.mem_offset);
		if (c->cap_map[i] == MAP_FAILED) {
			c->cap_map[i] = NULL;
			return false;
		}
		if (xioctl(c->fd, VIDIOC_QBUF, &b))
			return false;
	}
	int t = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
	if (xioctl(c->fd, VIDIOC_STREAMON, &t))
		return false;
	c->cap_on = true;
	dbg("pictures %ux%u pitch %u, %u buffers", c->cap_w, c->cap_h, c->cap_pitch, c->n_cap);
	return true;
}

static bool v4l2_open(struct context *c)
{
	struct driver *d = c->drv;
	c->fd = open(d->video, O_RDWR | O_NONBLOCK | O_CLOEXEC);
	if (c->fd < 0)
		return false;
	// Pictures in decoding order, each as soon as it is done.
	struct v4l2_control on = { .id = V4L2_CID_MPEG_VIDEO_DEC_DISPLAY_DELAY_ENABLE, .value = 1 };
	struct v4l2_control zero = { .id = V4L2_CID_MPEG_VIDEO_DEC_DISPLAY_DELAY, .value = 0 };
	if (xioctl(c->fd, VIDIOC_S_CTRL, &on) || xioctl(c->fd, VIDIOC_S_CTRL, &zero)) {
		dbg("the decoder can't give pictures in decoding order (an older vroom?)");
		return false;
	}
	struct v4l2_format f = { .type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE };
	f.fmt.pix_mp.pixelformat = profile_fourcc(c->profile);
	f.fmt.pix_mp.width = c->width;
	f.fmt.pix_mp.height = c->height;
	f.fmt.pix_mp.num_planes = 1;
	f.fmt.pix_mp.plane_fmt[0].sizeimage = c->width * c->height * 3 / 2 + (1 << 20);
	if (xioctl(c->fd, VIDIOC_S_FMT, &f))
		return false;
	struct v4l2_format cf = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE };
	cf.fmt.pix_mp.pixelformat = c->p010 ? V4L2_PIX_FMT_P010 : V4L2_PIX_FMT_NV12;
	cf.fmt.pix_mp.width = c->width;
	cf.fmt.pix_mp.height = c->height;
	xioctl(c->fd, VIDIOC_S_FMT, &cf);
	struct v4l2_event_subscription sub = { .type = V4L2_EVENT_SOURCE_CHANGE };
	xioctl(c->fd, VIDIOC_SUBSCRIBE_EVENT, &sub);
	struct v4l2_requestbuffers rb = { .count = OUTPUT_BUFS, .type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, .memory = V4L2_MEMORY_MMAP };
	if (xioctl(c->fd, VIDIOC_REQBUFS, &rb) || rb.count == 0)
		return false;
	c->n_out = rb.count > OUTPUT_BUFS ? OUTPUT_BUFS : rb.count;
	for (unsigned i = 0; i < c->n_out; i++) {
		struct v4l2_plane pl[1] = { 0 };
		struct v4l2_buffer b = { .index = i, .type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, .memory = V4L2_MEMORY_MMAP, .m.planes = pl, .length = 1 };
		if (xioctl(c->fd, VIDIOC_QUERYBUF, &b))
			return false;
		c->out_len[i] = pl[0].length;
		c->out_map[i] = mmap(NULL, pl[0].length, PROT_READ | PROT_WRITE, MAP_SHARED, c->fd, pl[0].m.mem_offset);
		if (c->out_map[i] == MAP_FAILED) {
			c->out_map[i] = NULL;
			return false;
		}
		c->out_busy[i] = false;
	}
	int t = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
	if (xioctl(c->fd, VIDIOC_STREAMON, &t))
		return false;
	c->out_on = true;
	dbg("decoder open: %c%c%c%c %ux%u", f.fmt.pix_mp.pixelformat & 0xff, (f.fmt.pix_mp.pixelformat >> 8) & 0xff,
	    (f.fmt.pix_mp.pixelformat >> 16) & 0xff, f.fmt.pix_mp.pixelformat >> 24, c->width, c->height);
	return true;
}

static void v4l2_close(struct context *c)
{
	if (c->fd < 0)
		return;
	for (unsigned i = 0; i < c->n_out; i++)
		if (c->out_map[i])
			munmap(c->out_map[i], c->out_len[i]);
	c->n_out = 0;
	cap_release(c);
	close(c->fd);
	c->fd = -1;
	c->out_on = c->cap_on = false;
}

static struct surface *pending_surface(struct driver *d, struct context *c, uint64_t seq)
{
	for (unsigned i = 0; i < d->n_objs; i++) {
		if (d->objs[i].type != OBJ_SURFACE)
			continue;
		struct surface *s = d->objs[i].p;
		if (s->pending && s->ctx == c && s->seq == seq)
			return s;
	}
	return NULL;
}

// Every picture queued before `seq` that hasn't come: it won't (pictures
// come in decoding order).
static void fail_before(struct driver *d, struct context *c, uint64_t seq)
{
	for (unsigned i = 0; i < d->n_objs; i++) {
		if (d->objs[i].type != OBJ_SURFACE)
			continue;
		struct surface *s = d->objs[i].p;
		if (s->pending && s->ctx == c && s->seq < seq) {
			dbg("picture %llu lost", (unsigned long long)s->seq);
			s->pending = false;
			s->failed = true;
		}
	}
}

static void copy_picture(struct context *c, struct surface *s, const uint8_t *src, size_t len)
{
	unsigned rows = s->rows < c->cap_h ? s->rows : c->cap_h;
	unsigned bytes = s->pitch < c->cap_pitch ? s->pitch : c->cap_pitch;
	size_t chroma_src = (size_t)c->cap_pitch * c->cap_h;
	size_t chroma_dst = (size_t)s->pitch * s->rows;
	if (chroma_src + (size_t)c->cap_pitch * (c->cap_h / 2) > len || c->cap_p010 != s->p010)
		return;
	for (unsigned y = 0; y < rows; y++)
		memcpy(s->data + (size_t)y * s->pitch, src + (size_t)y * c->cap_pitch, bytes);
	for (unsigned y = 0; y < rows / 2; y++)
		memcpy(s->data + chroma_dst + (size_t)y * s->pitch, src + chroma_src + (size_t)y * c->cap_pitch, bytes);
}

// Take what the decoder has for us. `wait_ms`: how long to wait for
// something (0: don't).
static void pump(struct context *c, int wait_ms)
{
	struct driver *d = c->drv;
	if (c->fd < 0)
		return;
	struct pollfd p = { .fd = c->fd, .events = POLLIN | POLLOUT | POLLPRI };
	if (poll(&p, 1, wait_ms) <= 0)
		return;
	if (p.revents & POLLPRI) {
		struct v4l2_event ev;
		while (xioctl(c->fd, VIDIOC_DQEVENT, &ev) == 0) {
			if (ev.type == V4L2_EVENT_SOURCE_CHANGE && !cap_setup(c)) {
				dbg("can't set up the picture buffers");
				c->failed = true;
			}
		}
	}
	for (;;) {
		struct v4l2_plane pl[1] = { 0 };
		struct v4l2_buffer b = { .type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, .memory = V4L2_MEMORY_MMAP, .m.planes = pl, .length = 1 };
		if (xioctl(c->fd, VIDIOC_DQBUF, &b))
			break;
		if (b.index < c->n_out)
			c->out_busy[b.index] = false;
	}
	while (c->cap_on) {
		struct v4l2_plane pl[1] = { 0 };
		struct v4l2_buffer b = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, .memory = V4L2_MEMORY_MMAP, .m.planes = pl, .length = 1 };
		if (xioctl(c->fd, VIDIOC_DQBUF, &b))
			break;
		uint64_t seq = (uint64_t)b.timestamp.tv_sec;
		fail_before(d, c, seq);
		struct surface *s = pending_surface(d, c, seq);
		if (s) {
			s->pending = false;
			s->failed = (b.flags & V4L2_BUF_FLAG_ERROR) || pl[0].bytesused == 0;
			if (!s->failed && b.index < c->n_cap && c->cap_map[b.index])
				copy_picture(c, s, c->cap_map[b.index], pl[0].bytesused);
			s->bo_stale = true;
		}
		if (!(b.flags & V4L2_BUF_FLAG_LAST))
			xioctl(c->fd, VIDIOC_QBUF, &b);
	}
	if (p.revents & POLLERR)
		usleep(1000);
}

static int64_t now_ms(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

// Queue the picture built in c->au, for surface s.
static VAStatus submit(struct context *c, struct surface *s)
{
	if (c->failed)
		return VA_STATUS_ERROR_DECODING_ERROR;
	if (c->fd < 0 && !v4l2_open(c)) {
		c->failed = true;
		v4l2_close(c);
		return VA_STATUS_ERROR_OPERATION_FAILED;
	}
	int64_t until = now_ms() + SYNC_TIMEOUT_MS;
	int idx = -1;
	for (;;) {
		for (unsigned i = 0; i < c->n_out; i++) {
			if (!c->out_busy[i]) {
				idx = (int)i;
				break;
			}
		}
		if (idx >= 0 || now_ms() > until)
			break;
		pump(c, 20);
	}
	if (idx < 0 || c->au_len > c->out_len[idx]) {
		dbg("no room for a picture (%zu bytes)", c->au_len);
		return VA_STATUS_ERROR_DECODING_ERROR;
	}
	memcpy(c->out_map[idx], c->au, c->au_len);
	// VROOM_VA_DUMP=file: the bitstream we send, to check with another decoder.
	const char *dump = getenv("VROOM_VA_DUMP");
	if (dump) {
		FILE *f = fopen(dump, "ab");
		if (f) {
			fwrite(c->au, 1, c->au_len, f);
			fclose(f);
		}
	}
	struct v4l2_plane pl[1] = { { .bytesused = (uint32_t)c->au_len, .length = (uint32_t)c->out_len[idx] } };
	struct v4l2_buffer b = { .index = (uint32_t)idx, .type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, .memory = V4L2_MEMORY_MMAP, .m.planes = pl, .length = 1 };
	uint64_t seq = ++c->next_seq;
	b.timestamp.tv_sec = (time_t)seq;
	if (xioctl(c->fd, VIDIOC_QBUF, &b)) {
		dbg("QBUF: %s", strerror(errno));
		return VA_STATUS_ERROR_DECODING_ERROR;
	}
	c->out_busy[idx] = true;
	// A surface being decoded again: whatever it had is gone.
	s->pending = true;
	s->failed = false;
	s->seq = seq;
	s->ctx = c;
	pump(c, 0);
	return VA_STATUS_SUCCESS;
}

// Wait until the surface's picture is in.
static VAStatus wait_surface(struct driver *d, struct surface *s)
{
	(void)d;
	int64_t until = now_ms() + SYNC_TIMEOUT_MS;
	while (s->pending && s->ctx) {
		if (now_ms() > until || s->ctx->failed) {
			dbg("picture %llu timed out", (unsigned long long)s->seq);
			s->pending = false;
			s->failed = true;
			break;
		}
		pump(s->ctx, 20);
	}
	return VA_STATUS_SUCCESS;
}

// ---------------------------------------------------------------------
// VA-API entry points

static VAStatus vr_Terminate(VADriverContextP ctx)
{
	struct driver *d = DRV(ctx);
	for (unsigned i = 0; i < d->n_objs; i++) {
		struct obj *o = &d->objs[i];
		if (o->type == OBJ_CONTEXT) {
			struct context *c = o->p;
			v4l2_close(c);
			free(c->au);
			free(c->slices);
		} else if (o->type == OBJ_SURFACE) {
			struct surface *s = o->p;
			bo_free(s);
			free(s->data);
		} else if (o->type == OBJ_BUFFER) {
			struct buffer *b = o->p;
			if (!b->borrowed)
				free(b->data);
		}
		free(o->p);
	}
	free(d->objs);
	if (d->gbm)
		gbm_device_destroy(d->gbm);
	pthread_mutex_destroy(&d->lock);
	free(d);
	ctx->pDriverData = NULL;
	return VA_STATUS_SUCCESS;
}

static int list_profiles(struct driver *d, VAProfile *out)
{
	int n = 0;
	if (has_coded(d, V4L2_PIX_FMT_H264)) {
		out[n++] = VAProfileH264ConstrainedBaseline;
		out[n++] = VAProfileH264Main;
		out[n++] = VAProfileH264High;
	}
	if (has_coded(d, V4L2_PIX_FMT_HEVC)) {
		out[n++] = VAProfileHEVCMain;
		if (d->p010)
			out[n++] = VAProfileHEVCMain10;
	}
	if (has_coded(d, V4L2_PIX_FMT_VP9)) {
		out[n++] = VAProfileVP9Profile0;
		if (d->p010)
			out[n++] = VAProfileVP9Profile2;
	}
	return n;
}

static bool profile_ok(struct driver *d, VAProfile p)
{
	VAProfile list[MAX_PROFILES];
	int n = list_profiles(d, list);
	for (int i = 0; i < n; i++)
		if (list[i] == p)
			return true;
	return false;
}

static VAStatus vr_QueryConfigProfiles(VADriverContextP ctx, VAProfile *list, int *n)
{
	*n = list_profiles(DRV(ctx), list);
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_QueryConfigEntrypoints(VADriverContextP ctx, VAProfile p, VAEntrypoint *list, int *n)
{
	if (!profile_ok(DRV(ctx), p))
		return VA_STATUS_ERROR_UNSUPPORTED_PROFILE;
	list[0] = VAEntrypointVLD;
	*n = 1;
	return VA_STATUS_SUCCESS;
}

static unsigned rt_format(VAProfile p)
{
	return profile_10bit(p) ? VA_RT_FORMAT_YUV420_10 : VA_RT_FORMAT_YUV420;
}

static VAStatus vr_GetConfigAttributes(VADriverContextP ctx, VAProfile p, VAEntrypoint e, VAConfigAttrib *a, int n)
{
	if (!profile_ok(DRV(ctx), p))
		return VA_STATUS_ERROR_UNSUPPORTED_PROFILE;
	if (e != VAEntrypointVLD)
		return VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT;
	for (int i = 0; i < n; i++) {
		switch (a[i].type) {
		case VAConfigAttribRTFormat:
			a[i].value = rt_format(p);
			break;
		case VAConfigAttribMaxPictureWidth:
		case VAConfigAttribMaxPictureHeight:
			a[i].value = 4096;
			break;
		default:
			a[i].value = VA_ATTRIB_NOT_SUPPORTED;
		}
	}
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_CreateConfig(VADriverContextP ctx, VAProfile p, VAEntrypoint e, VAConfigAttrib *a, int n, VAConfigID *id)
{
	struct driver *d = DRV(ctx);
	if (!profile_ok(d, p))
		return VA_STATUS_ERROR_UNSUPPORTED_PROFILE;
	if (e != VAEntrypointVLD)
		return VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT;
	struct config *c = calloc(1, sizeof(*c));
	if (!c)
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	c->profile = p;
	c->entrypoint = e;
	c->rt_format = rt_format(p);
	for (int i = 0; i < n; i++)
		if (a[i].type == VAConfigAttribRTFormat && !(a[i].value & c->rt_format)) {
			free(c);
			return VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT;
		}
	pthread_mutex_lock(&d->lock);
	*id = new_obj(d, OBJ_CONFIG, c);
	pthread_mutex_unlock(&d->lock);
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_DestroyConfig(VADriverContextP ctx, VAConfigID id)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct config *c = get_obj(d, id, OBJ_CONFIG);
	if (c) {
		free(c);
		del_obj(d, id);
	}
	pthread_mutex_unlock(&d->lock);
	return c ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_CONFIG;
}

static VAStatus vr_QueryConfigAttributes(VADriverContextP ctx, VAConfigID id, VAProfile *p, VAEntrypoint *e, VAConfigAttrib *a, int *n)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct config *c = get_obj(d, id, OBJ_CONFIG);
	if (c) {
		*p = c->profile;
		*e = c->entrypoint;
		a[0].type = VAConfigAttribRTFormat;
		a[0].value = c->rt_format;
		*n = 1;
	}
	pthread_mutex_unlock(&d->lock);
	return c ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_CONFIG;
}

static struct surface *surface_new(unsigned w, unsigned h, bool p010)
{
	struct surface *s = calloc(1, sizeof(*s));
	if (!s)
		return NULL;
	s->width = w;
	s->height = h;
	s->p010 = p010;
	s->pitch = ((w + 15) & ~15u) * (p010 ? 2 : 1);
	s->rows = (h + 15) & ~15u;
	s->size = (size_t)s->pitch * s->rows * 3 / 2;
	s->data = malloc(s->size);
	if (!s->data) {
		free(s);
		return NULL;
	}
	// Black.
	memset(s->data, p010 ? 0 : 16, (size_t)s->pitch * s->rows);
	if (p010)
		for (size_t i = (size_t)s->pitch * s->rows; i + 1 < s->size; i += 2)
			s->data[i] = 0, s->data[i + 1] = 0x80;
	else
		memset(s->data + (size_t)s->pitch * s->rows, 128, s->size - (size_t)s->pitch * s->rows);
	s->bo_stale = true;
	return s;
}

static VAStatus vr_CreateSurfaces2(VADriverContextP ctx, unsigned format, unsigned w, unsigned h, VASurfaceID *ids, unsigned n,
				   VASurfaceAttrib *attribs, unsigned nattr)
{
	struct driver *d = DRV(ctx);
	if (format != VA_RT_FORMAT_YUV420 && format != VA_RT_FORMAT_YUV420_10)
		return VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT;
	if (w == 0 || h == 0 || w > 4096 || h > 4096)
		return VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED;
	bool p010 = format == VA_RT_FORMAT_YUV420_10;
	for (unsigned i = 0; i < nattr; i++) {
		if (attribs[i].type == VASurfaceAttribPixelFormat && (attribs[i].flags & VA_SURFACE_ATTRIB_SETTABLE)) {
			if (attribs[i].value.value.i == VA_FOURCC_P010)
				p010 = true;
			else if (attribs[i].value.value.i == VA_FOURCC_NV12)
				p010 = false;
			else
				return VA_STATUS_ERROR_INVALID_IMAGE_FORMAT;
		}
		if (attribs[i].type == VASurfaceAttribMemoryType && (attribs[i].flags & VA_SURFACE_ATTRIB_SETTABLE) &&
		    attribs[i].value.value.i != VA_SURFACE_ATTRIB_MEM_TYPE_VA)
			return VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE;
	}
	pthread_mutex_lock(&d->lock);
	for (unsigned i = 0; i < n; i++) {
		struct surface *s = surface_new(w, h, p010);
		ids[i] = s ? new_obj(d, OBJ_SURFACE, s) : VA_INVALID_ID;
		if (!s) {
			pthread_mutex_unlock(&d->lock);
			return VA_STATUS_ERROR_ALLOCATION_FAILED;
		}
	}
	pthread_mutex_unlock(&d->lock);
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_CreateSurfaces(VADriverContextP ctx, int w, int h, int format, int n, VASurfaceID *ids)
{
	return vr_CreateSurfaces2(ctx, (unsigned)format, (unsigned)w, (unsigned)h, ids, (unsigned)n, NULL, 0);
}

static VAStatus vr_DestroySurfaces(VADriverContextP ctx, VASurfaceID *ids, int n)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	for (int i = 0; i < n; i++) {
		struct surface *s = get_obj(d, ids[i], OBJ_SURFACE);
		if (!s)
			continue;
		bo_free(s);
		free(s->data);
		free(s);
		del_obj(d, ids[i]);
	}
	pthread_mutex_unlock(&d->lock);
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_QuerySurfaceAttributes(VADriverContextP ctx, VAConfigID cfg, VASurfaceAttrib *a, unsigned *n)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct config *c = get_obj(d, cfg, OBJ_CONFIG);
	bool p010 = c && profile_10bit(c->profile);
	pthread_mutex_unlock(&d->lock);
	if (!c)
		return VA_STATUS_ERROR_INVALID_CONFIG;
	VASurfaceAttrib list[] = {
		{ VASurfaceAttribPixelFormat, VA_SURFACE_ATTRIB_GETTABLE | VA_SURFACE_ATTRIB_SETTABLE,
		  { VAGenericValueTypeInteger, { .i = p010 ? VA_FOURCC_P010 : VA_FOURCC_NV12 } } },
		{ VASurfaceAttribMemoryType, VA_SURFACE_ATTRIB_GETTABLE | VA_SURFACE_ATTRIB_SETTABLE,
		  { VAGenericValueTypeInteger, { .i = VA_SURFACE_ATTRIB_MEM_TYPE_VA | VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 } } },
		{ VASurfaceAttribMinWidth, VA_SURFACE_ATTRIB_GETTABLE, { VAGenericValueTypeInteger, { .i = 16 } } },
		{ VASurfaceAttribMinHeight, VA_SURFACE_ATTRIB_GETTABLE, { VAGenericValueTypeInteger, { .i = 16 } } },
		{ VASurfaceAttribMaxWidth, VA_SURFACE_ATTRIB_GETTABLE, { VAGenericValueTypeInteger, { .i = 4096 } } },
		{ VASurfaceAttribMaxHeight, VA_SURFACE_ATTRIB_GETTABLE, { VAGenericValueTypeInteger, { .i = 4096 } } },
	};
	unsigned count = sizeof(list) / sizeof(list[0]);
	if (!a) {
		*n = count;
		return VA_STATUS_SUCCESS;
	}
	if (*n < count) {
		*n = count;
		return VA_STATUS_ERROR_MAX_NUM_EXCEEDED;
	}
	memcpy(a, list, sizeof(list));
	*n = count;
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_CreateContext(VADriverContextP ctx, VAConfigID cfg, int w, int h, int flag, VASurfaceID *targets, int n, VAContextID *id)
{
	(void)flag, (void)targets, (void)n;
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct config *conf = get_obj(d, cfg, OBJ_CONFIG);
	if (!conf) {
		pthread_mutex_unlock(&d->lock);
		return VA_STATUS_ERROR_INVALID_CONFIG;
	}
	struct context *c = calloc(1, sizeof(*c));
	if (!c) {
		pthread_mutex_unlock(&d->lock);
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	}
	c->drv = d;
	c->profile = conf->profile;
	c->p010 = profile_10bit(conf->profile);
	c->width = (unsigned)w;
	c->height = (unsigned)h;
	c->fd = -1;
	c->target = VA_INVALID_ID;
	*id = new_obj(d, OBJ_CONTEXT, c);
	pthread_mutex_unlock(&d->lock);
	dbg("context %u: profile %d, %dx%d", *id, conf->profile, w, h);
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_DestroyContext(VADriverContextP ctx, VAContextID id)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct context *c = get_obj(d, id, OBJ_CONTEXT);
	if (c) {
		for (unsigned i = 0; i < d->n_objs; i++) {
			if (d->objs[i].type == OBJ_SURFACE) {
				struct surface *s = d->objs[i].p;
				if (s->ctx == c) {
					s->pending = false;
					s->ctx = NULL;
				}
			}
		}
		v4l2_close(c);
		free(c->au);
		free(c->slices);
		free(c);
		del_obj(d, id);
	}
	pthread_mutex_unlock(&d->lock);
	return c ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_CONTEXT;
}

static VAStatus vr_CreateBuffer(VADriverContextP ctx, VAContextID cid, VABufferType type, unsigned size, unsigned num, void *data,
				VABufferID *id)
{
	(void)cid;
	struct driver *d = DRV(ctx);
	if ((uint64_t)size * num > (64u << 20))
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	struct buffer *b = calloc(1, sizeof(*b));
	if (!b)
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	b->type = type;
	b->size = size;
	b->num = num;
	b->data = malloc((size_t)size * num + 1);
	if (!b->data) {
		free(b);
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	}
	if (data)
		memcpy(b->data, data, (size_t)size * num);
	pthread_mutex_lock(&d->lock);
	*id = new_obj(d, OBJ_BUFFER, b);
	pthread_mutex_unlock(&d->lock);
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_BufferSetNumElements(VADriverContextP ctx, VABufferID id, unsigned num)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct buffer *b = get_obj(d, id, OBJ_BUFFER);
	VAStatus st = VA_STATUS_ERROR_INVALID_BUFFER;
	if (b && num <= b->num) {
		b->num = num;
		st = VA_STATUS_SUCCESS;
	}
	pthread_mutex_unlock(&d->lock);
	return st;
}

static VAStatus vr_MapBuffer(VADriverContextP ctx, VABufferID id, void **p)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct buffer *b = get_obj(d, id, OBJ_BUFFER);
	if (b)
		*p = b->data;
	pthread_mutex_unlock(&d->lock);
	return b ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_BUFFER;
}

static VAStatus vr_UnmapBuffer(VADriverContextP ctx, VABufferID id)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct buffer *b = get_obj(d, id, OBJ_BUFFER);
	pthread_mutex_unlock(&d->lock);
	return b ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_BUFFER;
}

static VAStatus vr_DestroyBuffer(VADriverContextP ctx, VABufferID id)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct buffer *b = get_obj(d, id, OBJ_BUFFER);
	if (b) {
		if (!b->borrowed)
			free(b->data);
		free(b);
		del_obj(d, id);
	}
	pthread_mutex_unlock(&d->lock);
	return b ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_BUFFER;
}

static VAStatus vr_BeginPicture(VADriverContextP ctx, VAContextID cid, VASurfaceID target)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct context *c = get_obj(d, cid, OBJ_CONTEXT);
	struct surface *s = get_obj(d, target, OBJ_SURFACE);
	if (c && s) {
		// The surface may still be on its way from an earlier picture.
		if (s->pending && s->ctx)
			wait_surface(d, s);
		c->target = target;
		c->au_len = 0;
		c->slices_len = 0;
		memset(c->pps_used, 0, sizeof(c->pps_used));
		c->have_pic = c->have_iq = false;
	}
	pthread_mutex_unlock(&d->lock);
	if (!c)
		return VA_STATUS_ERROR_INVALID_CONTEXT;
	return s ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_SURFACE;
}

// H.264 slices of one RenderPicture call: parameters and data buffers
// come in pairs, parameters first. What the slice headers leave to the
// PPS is noted for the parameter sets (written at EndPicture).
static VAStatus h264_slices(struct context *c, struct buffer *params, struct buffer *data)
{
	if (!c->have_pic)
		return VA_STATUS_ERROR_INVALID_PARAMETER;
	if (params->size < sizeof(VASliceParameterBufferH264))
		return VA_STATUS_ERROR_INVALID_BUFFER;
	for (unsigned i = 0; i < params->num; i++) {
		VASliceParameterBufferH264 sp;
		memcpy(&sp, params->data + (size_t)i * params->size, sizeof(sp));
		if ((uint64_t)sp.slice_data_offset + sp.slice_data_size > (uint64_t)data->size * data->num)
			return VA_STATUS_ERROR_INVALID_BUFFER;
		const uint8_t *nal = data->data + sp.slice_data_offset;
		size_t len = sp.slice_data_size;
		// Some players pass a start code: skip it.
		while (len > 3 && nal[0] == 0 && nal[1] == 0 && (nal[2] == 1 || (nal[2] == 0 && nal[3] == 1))) {
			size_t sc = nal[2] == 1 ? 3 : 4;
			nal += sc, len -= sc;
		}
		bool l0, l1;
		int pps_id = h264_slice_info(c, nal, len, &l0, &l1);
		if (pps_id < 0)
			pps_id = 0;
		struct h264_pps_state *ps = &c->pps_state[pps_id];
		if (l0) {
			ps->known_l0 = true;
			ps->l0 = sp.num_ref_idx_l0_active_minus1;
		}
		if (l1) {
			ps->known_l1 = true;
			ps->l1 = sp.num_ref_idx_l1_active_minus1;
		}
		c->pps_used[pps_id] = true;
		if (!slice_add(c, nal, len, true))
			return VA_STATUS_ERROR_ALLOCATION_FAILED;
	}
	return VA_STATUS_SUCCESS;
}

// A VP9 picture: the frame as it came.
static bool frame_build(struct context *c)
{
	if (!au_grow(c, c->slices_len))
		return false;
	memcpy(c->au, c->slices, c->slices_len);
	c->au_len = c->slices_len;
	return true;
}

// The H.264 picture: parameter sets that changed, then its slices.
static bool h264_build(struct context *c)
{
	struct bw b = { 0 };
	h264_sps(c, &b);
	if (b.overflow)
		return false;
	if (b.len != c->last_sps_len || memcmp(b.buf, c->last_sps, b.len)) {
		memcpy(c->last_sps, b.buf, b.len);
		c->last_sps_len = b.len;
		memset(c->last_pps_len, 0, sizeof(c->last_pps_len));
		uint8_t h = 0x67;
		if (!au_nal(c, &h, 1, b.buf, b.len))
			return false;
	}
	for (unsigned id = 0; id < 256; id++) {
		if (!c->pps_used[id])
			continue;
		struct bw p = { 0 };
		h264_pps(c, &p, id, c->pps_state[id].l0, c->pps_state[id].l1);
		if (p.overflow || p.len > PPS_MAX)
			return false;
		if (p.len != c->last_pps_len[id] || memcmp(p.buf, c->last_pps[id], p.len)) {
			memcpy(c->last_pps[id], p.buf, p.len);
			c->last_pps_len[id] = (uint16_t)p.len;
			uint8_t h = 0x68;
			if (!au_nal(c, &h, 1, p.buf, p.len))
				return false;
		}
	}
	if (!au_grow(c, c->slices_len))
		return false;
	memcpy(c->au + c->au_len, c->slices, c->slices_len);
	c->au_len += c->slices_len;
	return true;
}

// ---------------------------------------------------------------------
// HEVC: VPS, SPS and PPS back out of VA-API's parameters. VA-API doesn't
// pass the SPS's reference picture sets (slices name them by index), so
// each slice header is rewritten to carry its set itself, built from the
// picture's references; the rest of the header and the slice data are
// copied as they are.

// Bits we read from (already unescaped) and write to, growing.
struct rbits {
	const uint8_t *p;
	size_t nbits, pos;
	bool bad;
};

static uint32_t rb_bits(struct rbits *r, int n)
{
	uint32_t v = 0;
	for (int i = 0; i < n; i++) {
		if (r->pos >= r->nbits) {
			r->bad = true;
			return 0;
		}
		v = (v << 1) | ((r->p[r->pos / 8] >> (7 - r->pos % 8)) & 1);
		r->pos++;
	}
	return v;
}

static uint32_t rb_ue(struct rbits *r)
{
	int zeros = 0;
	while (!r->bad && rb_bits(r, 1) == 0) {
		if (++zeros > 31) {
			r->bad = true;
			return 0;
		}
	}
	return (uint32_t)((1ull << zeros) - 1 + rb_bits(r, zeros));
}

struct wbits {
	uint8_t *buf;
	size_t len, cap;
	uint32_t acc;
	int n;
	bool bad;
};

static void wb_bits(struct wbits *w, uint32_t v, int n)
{
	for (int i = n - 1; i >= 0; i--) {
		w->acc = (w->acc << 1) | ((v >> i) & 1);
		if (++w->n == 8) {
			if (!grow(&w->buf, &w->cap, w->len, 1)) {
				w->bad = true;
				return;
			}
			w->buf[w->len++] = (uint8_t)w->acc;
			w->acc = 0;
			w->n = 0;
		}
	}
}

static void wb_ue(struct wbits *w, uint32_t v)
{
	uint64_t x = (uint64_t)v + 1;
	int len = 0;
	while ((x >> len) > 1)
		len++;
	wb_bits(w, 0, len);
	wb_bits(w, (uint32_t)x, len + 1);
}

// Copy bits [from, to) of r.
static void wb_copy(struct wbits *w, const struct rbits *r, size_t from, size_t to)
{
	for (size_t i = from; i < to && i < r->nbits; i++)
		wb_bits(w, (r->p[i / 8] >> (7 - i % 8)) & 1, 1);
}

static void wb_trailing(struct wbits *w)
{
	wb_bits(w, 1, 1);
	while (w->n)
		wb_bits(w, 0, 1);
}

static unsigned ceil_log2(unsigned v)
{
	unsigned n = 0;
	while ((1u << n) < v)
		n++;
	return n;
}

static void hevc_ptl(struct bw *b, unsigned profile)
{
	put_bits(b, 0, 2);          // general_profile_space
	put_bits(b, 0, 1);          // general_tier_flag
	put_bits(b, profile, 5);    // general_profile_idc
	put_bits(b, 1u << (31 - profile), 32); // compatibility flag for it
	put_bits(b, 1, 1);          // progressive_source_flag
	put_bits(b, 0, 1);          // interlaced_source_flag
	put_bits(b, 0, 1);          // non_packed_constraint_flag
	put_bits(b, 1, 1);          // frame_only_constraint_flag
	put_bits(b, 0, 32);         // reserved (43 bits) and inbld (1 bit)
	put_bits(b, 0, 12);
	put_bits(b, 153, 8);        // general_level_idc: 5.1
}

static unsigned hevc_profile(struct context *c)
{
	return c->profile == VAProfileHEVCMain10 ? 2 : 1;
}

static void hevc_vps(struct context *c, struct bw *b)
{
	const VAPictureParameterBufferHEVC *pp = &c->hevc_pic;
	put_bits(b, 0, 4);      // vps_video_parameter_set_id
	put_bits(b, 3, 2);      // base layer internal, available
	put_bits(b, 0, 6);      // vps_max_layers_minus1
	put_bits(b, 0, 3);      // vps_max_sub_layers_minus1
	put_bits(b, 1, 1);      // vps_temporal_id_nesting_flag
	put_bits(b, 0xffff, 16);
	hevc_ptl(b, hevc_profile(c));
	put_bits(b, 1, 1);      // vps_sub_layer_ordering_info_present_flag
	put_ue(b, pp->sps_max_dec_pic_buffering_minus1);
	put_ue(b, 0);           // vps_max_num_reorder_pics: see the SPS
	put_ue(b, 0);           // vps_max_latency_increase_plus1
	put_bits(b, 0, 6);      // vps_max_layer_id
	put_ue(b, 0);           // vps_num_layer_sets_minus1
	put_bits(b, 0, 1);      // vps_timing_info_present_flag
	put_bits(b, 0, 1);      // vps_extension_flag
	put_trailing(b);
}

static void hevc_sps(struct context *c, struct bw *b)
{
	const VAPictureParameterBufferHEVC *pp = &c->hevc_pic;
	put_bits(b, 0, 4);      // sps_video_parameter_set_id
	put_bits(b, 0, 3);      // sps_max_sub_layers_minus1
	put_bits(b, 1, 1);      // sps_temporal_id_nesting_flag
	hevc_ptl(b, hevc_profile(c));
	put_ue(b, 0);           // sps_seq_parameter_set_id
	put_ue(b, pp->pic_fields.bits.chroma_format_idc);
	if (pp->pic_fields.bits.chroma_format_idc == 3)
		put_bits(b, pp->pic_fields.bits.separate_colour_plane_flag, 1);
	put_ue(b, pp->pic_width_in_luma_samples);
	put_ue(b, pp->pic_height_in_luma_samples);
	put_bits(b, 0, 1);      // conformance_window_flag: the player crops
	put_ue(b, pp->bit_depth_luma_minus8);
	put_ue(b, pp->bit_depth_chroma_minus8);
	put_ue(b, pp->log2_max_pic_order_cnt_lsb_minus4);
	put_bits(b, 1, 1);      // sps_sub_layer_ordering_info_present_flag
	put_ue(b, pp->sps_max_dec_pic_buffering_minus1);
	// No reordering: pictures come out as soon as they are decoded, in
	// decoding order, which VA-API wants (the player reorders).
	put_ue(b, 0);
	put_ue(b, 0);
	put_ue(b, pp->log2_min_luma_coding_block_size_minus3);
	put_ue(b, pp->log2_diff_max_min_luma_coding_block_size);
	put_ue(b, pp->log2_min_transform_block_size_minus2);
	put_ue(b, pp->log2_diff_max_min_transform_block_size);
	put_ue(b, pp->max_transform_hierarchy_depth_inter);
	put_ue(b, pp->max_transform_hierarchy_depth_intra);
	put_bits(b, pp->pic_fields.bits.scaling_list_enabled_flag, 1);
	if (pp->pic_fields.bits.scaling_list_enabled_flag)
		put_bits(b, 0, 1); // sps_scaling_list_data_present_flag: the PPS has them
	put_bits(b, pp->pic_fields.bits.amp_enabled_flag, 1);
	put_bits(b, pp->slice_parsing_fields.bits.sample_adaptive_offset_enabled_flag, 1);
	put_bits(b, pp->pic_fields.bits.pcm_enabled_flag, 1);
	if (pp->pic_fields.bits.pcm_enabled_flag) {
		put_bits(b, pp->pcm_sample_bit_depth_luma_minus1, 4);
		put_bits(b, pp->pcm_sample_bit_depth_chroma_minus1, 4);
		put_ue(b, pp->log2_min_pcm_luma_coding_block_size_minus3);
		put_ue(b, pp->log2_diff_max_min_pcm_luma_coding_block_size);
		put_bits(b, pp->pic_fields.bits.pcm_loop_filter_disabled_flag, 1);
	}
	put_ue(b, 0);           // num_short_term_ref_pic_sets: each slice has its own
	put_bits(b, pp->slice_parsing_fields.bits.long_term_ref_pics_present_flag, 1);
	if (pp->slice_parsing_fields.bits.long_term_ref_pics_present_flag)
		put_ue(b, 0);   // num_long_term_ref_pics_sps: likewise
	put_bits(b, pp->slice_parsing_fields.bits.sps_temporal_mvp_enabled_flag, 1);
	put_bits(b, pp->pic_fields.bits.strong_intra_smoothing_enabled_flag, 1);
	put_bits(b, 0, 1);      // vui_parameters_present_flag
	put_bits(b, 0, 1);      // sps_extension_present_flag
	put_trailing(b);
}

// Up-right diagonal scan of an n x n block, as raster indices.
static void diag_scan(int n, uint8_t *out)
{
	int i = 0, x = 0, y = 0;
	while (i < n * n) {
		while (y >= 0) {
			if (x < n && y < n)
				out[i++] = (uint8_t)(y * n + x);
			y--;
			x++;
		}
		y = x;
		x = 0;
	}
}

// One scaling list: VA-API gives it in raster order, the bitstream wants
// up-right diagonal order, as differences.
static void hevc_list(struct bw *b, const uint8_t *raster, int n, int dc)
{
	uint8_t scan[64];
	diag_scan(n == 16 ? 4 : 8, scan);
	put_bits(b, 1, 1); // scaling_list_pred_mode_flag: explicit
	int next = 8;
	if (dc >= 0) {
		put_se(b, dc - 8);
		next = dc;
	}
	for (int i = 0; i < n; i++) {
		int v = raster[scan[i]];
		int d = v - next;
		if (d > 127)
			d -= 256;
		if (d < -128)
			d += 256;
		put_se(b, d);
		next = v;
	}
}

static void hevc_pps(struct context *c, struct bw *b, unsigned id)
{
	const VAPictureParameterBufferHEVC *pp = &c->hevc_pic;
	put_ue(b, id);
	put_ue(b, 0); // pps_seq_parameter_set_id
	put_bits(b, pp->slice_parsing_fields.bits.dependent_slice_segments_enabled_flag, 1);
	put_bits(b, pp->slice_parsing_fields.bits.output_flag_present_flag, 1);
	put_bits(b, pp->num_extra_slice_header_bits, 3);
	put_bits(b, pp->pic_fields.bits.sign_data_hiding_enabled_flag, 1);
	put_bits(b, pp->slice_parsing_fields.bits.cabac_init_present_flag, 1);
	put_ue(b, pp->num_ref_idx_l0_default_active_minus1);
	put_ue(b, pp->num_ref_idx_l1_default_active_minus1);
	put_se(b, pp->init_qp_minus26);
	put_bits(b, pp->pic_fields.bits.constrained_intra_pred_flag, 1);
	put_bits(b, pp->pic_fields.bits.transform_skip_enabled_flag, 1);
	put_bits(b, pp->pic_fields.bits.cu_qp_delta_enabled_flag, 1);
	if (pp->pic_fields.bits.cu_qp_delta_enabled_flag)
		put_ue(b, pp->diff_cu_qp_delta_depth);
	put_se(b, pp->pps_cb_qp_offset);
	put_se(b, pp->pps_cr_qp_offset);
	put_bits(b, pp->slice_parsing_fields.bits.pps_slice_chroma_qp_offsets_present_flag, 1);
	put_bits(b, pp->pic_fields.bits.weighted_pred_flag, 1);
	put_bits(b, pp->pic_fields.bits.weighted_bipred_flag, 1);
	put_bits(b, pp->pic_fields.bits.transquant_bypass_enabled_flag, 1);
	put_bits(b, pp->pic_fields.bits.tiles_enabled_flag, 1);
	put_bits(b, pp->pic_fields.bits.entropy_coding_sync_enabled_flag, 1);
	if (pp->pic_fields.bits.tiles_enabled_flag) {
		unsigned cols = pp->num_tile_columns_minus1 < 19 ? pp->num_tile_columns_minus1 : 19;
		unsigned rows = pp->num_tile_rows_minus1 < 21 ? pp->num_tile_rows_minus1 : 21;
		put_ue(b, cols);
		put_ue(b, rows);
		put_bits(b, 0, 1); // uniform_spacing_flag: the sizes, as VA-API worked them out
		for (unsigned i = 0; i < cols; i++)
			put_ue(b, pp->column_width_minus1[i]);
		for (unsigned i = 0; i < rows; i++)
			put_ue(b, pp->row_height_minus1[i]);
		put_bits(b, pp->pic_fields.bits.loop_filter_across_tiles_enabled_flag, 1);
	}
	put_bits(b, pp->pic_fields.bits.pps_loop_filter_across_slices_enabled_flag, 1);
	put_bits(b, 1, 1); // deblocking_filter_control_present_flag
	put_bits(b, pp->slice_parsing_fields.bits.deblocking_filter_override_enabled_flag, 1);
	put_bits(b, pp->slice_parsing_fields.bits.pps_disable_deblocking_filter_flag, 1);
	if (!pp->slice_parsing_fields.bits.pps_disable_deblocking_filter_flag) {
		put_se(b, pp->pps_beta_offset_div2);
		put_se(b, pp->pps_tc_offset_div2);
	}
	bool lists = pp->pic_fields.bits.scaling_list_enabled_flag && c->have_iq;
	put_bits(b, lists, 1); // pps_scaling_list_data_present_flag
	if (lists) {
		const VAIQMatrixBufferHEVC *q = &c->hevc_iq;
		for (int m = 0; m < 6; m++)
			hevc_list(b, q->ScalingList4x4[m], 16, -1);
		for (int m = 0; m < 6; m++)
			hevc_list(b, q->ScalingList8x8[m], 64, -1);
		for (int m = 0; m < 6; m++)
			hevc_list(b, q->ScalingList16x16[m], 64, q->ScalingListDC16x16[m]);
		for (int m = 0; m < 2; m++)
			hevc_list(b, q->ScalingList32x32[m], 64, q->ScalingListDC32x32[m]);
	}
	put_bits(b, pp->slice_parsing_fields.bits.lists_modification_present_flag, 1);
	put_ue(b, pp->log2_parallel_merge_level_minus2);
	put_bits(b, pp->slice_parsing_fields.bits.slice_segment_header_extension_present_flag, 1);
	put_bits(b, 0, 1); // pps_extension_present_flag
	put_trailing(b);
}

static int cmp_desc(const void *a, const void *b)
{
	return *(const int32_t *)b > *(const int32_t *)a ? 1 : *(const int32_t *)b < *(const int32_t *)a ? -1 : 0;
}

// The picture's short-term references as an explicit st_ref_pic_set(0)
// and its long-term ones spelled out, from VA-API's reference list.
static void hevc_rps(struct context *c, struct wbits *w)
{
	const VAPictureParameterBufferHEVC *pp = &c->hevc_pic;
	int32_t cur = pp->CurrPic.pic_order_cnt;
	// (poc << 1) | used, so one sort keeps the flag with its POC.
	int32_t neg[16], pos[16];
	int nn = 0, np = 0;
	struct { int32_t poc; bool used; } lt[16];
	int nl = 0;
	for (int i = 0; i < 15; i++) {
		const VAPictureHEVC *r = &pp->ReferenceFrames[i];
		if (r->picture_id == VA_INVALID_SURFACE || (r->flags & VA_PICTURE_HEVC_INVALID))
			continue;
		if (r->flags & VA_PICTURE_HEVC_LONG_TERM_REFERENCE) {
			lt[nl].poc = r->pic_order_cnt;
			lt[nl++].used = r->flags & VA_PICTURE_HEVC_RPS_LT_CURR;
			continue;
		}
		bool used = r->flags & (VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE | VA_PICTURE_HEVC_RPS_ST_CURR_AFTER);
		if (r->pic_order_cnt < cur)
			neg[nn++] = r->pic_order_cnt * 2 + used;
		else if (r->pic_order_cnt > cur)
			pos[np++] = -(r->pic_order_cnt * 2 + used);
	}
	// Negative ones nearest first (descending POC), positive ones nearest
	// first (ascending POC: sorted descending as negated).
	qsort(neg, (size_t)nn, sizeof(neg[0]), cmp_desc);
	qsort(pos, (size_t)np, sizeof(pos[0]), cmp_desc);
	wb_bits(w, 0, 1); // short_term_ref_pic_set_sps_flag
	// st_ref_pic_set(0): no inter RPS prediction for the first set.
	wb_ue(w, (uint32_t)nn);
	wb_ue(w, (uint32_t)np);
	int32_t prev = cur;
	for (int i = 0; i < nn; i++) {
		int32_t poc = neg[i] >> 1;
		wb_ue(w, (uint32_t)(prev - poc - 1));
		wb_bits(w, neg[i] & 1, 1);
		prev = poc;
	}
	prev = cur;
	for (int i = 0; i < np; i++) {
		int32_t v = -pos[i];
		int32_t poc = v >> 1;
		wb_ue(w, (uint32_t)(poc - prev - 1));
		wb_bits(w, v & 1, 1);
		prev = poc;
	}
	if (!pp->slice_parsing_fields.bits.long_term_ref_pics_present_flag)
		return;
	// Long-term: the SPS lists none, so each is spelled out with its LSBs
	// and how many POC MSB cycles back it is (ascending, as the deltas
	// are coded).
	uint32_t max_lsb = 1u << (pp->log2_max_pic_order_cnt_lsb_minus4 + 4);
	int64_t cycles[16];
	for (int i = 0; i < nl; i++) {
		int64_t cur_lsb = (int64_t)((uint32_t)cur & (max_lsb - 1));
		int64_t lt_lsb = (int64_t)((uint32_t)lt[i].poc & (max_lsb - 1));
		cycles[i] = ((int64_t)cur - lt[i].poc - (cur_lsb - lt_lsb)) / (int64_t)max_lsb;
	}
	for (int i = 1; i < nl; i++) // insertion sort by cycles
		for (int j = i; j > 0 && cycles[j] < cycles[j - 1]; j--) {
			int64_t t = cycles[j];
			cycles[j] = cycles[j - 1];
			cycles[j - 1] = t;
			__typeof__(lt[0]) u = lt[j];
			lt[j] = lt[j - 1];
			lt[j - 1] = u;
		}
	wb_ue(w, (uint32_t)nl); // num_long_term_pics
	int64_t last = 0;
	for (int i = 0; i < nl; i++) {
		wb_bits(w, (uint32_t)lt[i].poc & (max_lsb - 1), (int)(pp->log2_max_pic_order_cnt_lsb_minus4 + 4));
		wb_bits(w, lt[i].used, 1);
		wb_bits(w, 1, 1); // delta_poc_msb_present_flag
		wb_ue(w, (uint32_t)(cycles[i] - last));
		last = cycles[i];
	}
}

// One HEVC slice NAL unit, its header rewritten if it names the SPS's
// reference picture sets. False: it couldn't be read (it goes as it is).
static bool hevc_slice(struct context *c, const VASliceParameterBufferHEVC *sp, const uint8_t *nal, size_t len, unsigned *pps_id)
{
	const VAPictureParameterBufferHEVC *pp = &c->hevc_pic;
	*pps_id = 0;
	if (len < 3)
		return false;
	unsigned type = (nal[0] >> 1) & 0x3f;
	size_t hdr_end = sp->slice_data_byte_offset; // unescaped bytes, NAL header included
	if (hdr_end <= 2 || hdr_end > len)
		return false;
	// The header, unescaped, and where it ends in the raw bytes.
	uint8_t *rbsp = malloc(hdr_end);
	if (!rbsp)
		return false;
	size_t rlen = 0, raw = 2;
	int zeros = 0;
	while (raw < len && rlen < hdr_end - 2) {
		uint8_t v = nal[raw++];
		if (zeros >= 2 && v == 3) {
			zeros = 0;
			continue;
		}
		zeros = v == 0 ? zeros + 1 : 0;
		rbsp[rlen++] = v;
	}
	if (rlen != hdr_end - 2 || rbsp[rlen - 1] == 0) {
		free(rbsp);
		return false;
	}
	// The header proper ends at byte_alignment()'s one bit.
	size_t end = rlen * 8 - 1;
	while (!((rbsp[end / 8] >> (7 - end % 8)) & 1))
		end--;
	struct rbits r = { .p = rbsp, .nbits = end };
	bool first = rb_bits(&r, 1);
	if (type >= 16 && type <= 23)
		rb_bits(&r, 1);
	*pps_id = rb_ue(&r);
	bool dependent = false;
	unsigned ctb = pp->log2_min_luma_coding_block_size_minus3 + 3 + pp->log2_diff_max_min_luma_coding_block_size;
	unsigned wc = (pp->pic_width_in_luma_samples + (1u << ctb) - 1) >> ctb;
	unsigned hc = (pp->pic_height_in_luma_samples + (1u << ctb) - 1) >> ctb;
	if (!first) {
		if (pp->slice_parsing_fields.bits.dependent_slice_segments_enabled_flag)
			dependent = rb_bits(&r, 1);
		rb_bits(&r, (int)ceil_log2(wc * hc));
	}
	bool idr = type == 19 || type == 20;
	if (r.bad || *pps_id > 63) {
		free(rbsp);
		*pps_id = 0;
		return false;
	}
	if (dependent || idr) {
		// No reference sets in the header: as it is.
		free(rbsp);
		return slice_add(c, nal, len, true);
	}
	rb_bits(&r, pp->num_extra_slice_header_bits);
	rb_ue(&r); // slice_type
	if (pp->slice_parsing_fields.bits.output_flag_present_flag)
		rb_bits(&r, 1);
	if (pp->pic_fields.bits.separate_colour_plane_flag)
		rb_bits(&r, 2);
	rb_bits(&r, (int)(pp->log2_max_pic_order_cnt_lsb_minus4 + 4));
	size_t keep = r.pos;
	// What the original says about reference sets, skipped.
	if (rb_bits(&r, 1)) {
		if (pp->num_short_term_ref_pic_sets > 1)
			rb_bits(&r, (int)ceil_log2(pp->num_short_term_ref_pic_sets));
	} else {
		r.pos += pp->st_rps_bits;
	}
	if (pp->slice_parsing_fields.bits.long_term_ref_pics_present_flag) {
		unsigned nsps = pp->num_long_term_ref_pic_sps ? rb_ue(&r) : 0;
		unsigned npics = rb_ue(&r);
		if (nsps + npics > 32)
			r.bad = true;
		for (unsigned i = 0; i < nsps + npics && !r.bad; i++) {
			if (i < nsps) {
				if (pp->num_long_term_ref_pic_sps > 1)
					rb_bits(&r, (int)ceil_log2(pp->num_long_term_ref_pic_sps));
			} else {
				rb_bits(&r, (int)(pp->log2_max_pic_order_cnt_lsb_minus4 + 4) + 1);
			}
			if (rb_bits(&r, 1))
				rb_ue(&r);
		}
	}
	if (r.bad || r.pos > end) {
		free(rbsp);
		return false;
	}
	struct wbits w = { 0 };
	wb_copy(&w, &r, 0, keep);
	hevc_rps(c, &w);
	wb_copy(&w, &r, r.pos, end);
	wb_trailing(&w);
	free(rbsp);
	bool ok = !w.bad;
	if (ok) {
		// Start code, the NAL header, the new header escaped, then the
		// slice data as it was (the header's last byte isn't zero, so
		// no escape is needed where they meet).
		uint8_t sc[3] = { 0, 0, 1 };
		ok = slice_add(c, sc, 3, false) && slice_add(c, nal, 2, false);
		if (ok && grow(&c->slices, &c->slices_cap, c->slices_len, w.len * 3 / 2 + 4)) {
			int z = 0;
			for (size_t i = 0; i < w.len; i++) {
				if (z >= 2 && w.buf[i] <= 3) {
					c->slices[c->slices_len++] = 3;
					z = 0;
				}
				c->slices[c->slices_len++] = w.buf[i];
				z = w.buf[i] == 0 ? z + 1 : 0;
			}
		} else {
			ok = false;
		}
		ok = ok && slice_add(c, nal + raw, len - raw, false);
	}
	free(w.buf);
	return ok;
}

// HEVC slices of one RenderPicture call.
static VAStatus hevc_slices(struct context *c, struct buffer *params, struct buffer *data)
{
	if (!c->have_pic)
		return VA_STATUS_ERROR_INVALID_PARAMETER;
	if (params->size < sizeof(VASliceParameterBufferHEVC))
		return VA_STATUS_ERROR_INVALID_BUFFER;
	for (unsigned i = 0; i < params->num; i++) {
		VASliceParameterBufferHEVC sp;
		memcpy(&sp, params->data + (size_t)i * params->size, sizeof(sp));
		if ((uint64_t)sp.slice_data_offset + sp.slice_data_size > (uint64_t)data->size * data->num)
			return VA_STATUS_ERROR_INVALID_BUFFER;
		const uint8_t *nal = data->data + sp.slice_data_offset;
		size_t len = sp.slice_data_size;
		size_t skipped = 0;
		while (len > 3 && nal[0] == 0 && nal[1] == 0 && (nal[2] == 1 || (nal[2] == 0 && nal[3] == 1))) {
			size_t sc = nal[2] == 1 ? 3 : 4;
			nal += sc, len -= sc, skipped += sc;
		}
		sp.slice_data_byte_offset -= sp.slice_data_byte_offset >= skipped ? (uint32_t)skipped : 0;
		size_t before = c->slices_len;
		unsigned pps_id;
		if (!hevc_slice(c, &sp, nal, len, &pps_id)) {
			c->slices_len = before;
			dbg("an HEVC slice header we can't read: it goes as it is");
			if (!slice_add(c, nal, len, true))
				return VA_STATUS_ERROR_ALLOCATION_FAILED;
		}
		c->pps_used[pps_id] = true;
	}
	return VA_STATUS_SUCCESS;
}

static bool hevc_build(struct context *c)
{
	struct bw v = { 0 }, s = { 0 };
	hevc_vps(c, &v);
	hevc_sps(c, &s);
	if (v.overflow || s.overflow)
		return false;
	// VPS and SPS together decide whether they are sent again.
	uint8_t both[512];
	size_t blen = v.len + s.len;
	if (blen > sizeof(both) || blen > sizeof(c->last_sps))
		return false;
	memcpy(both, v.buf, v.len);
	memcpy(both + v.len, s.buf, s.len);
	if (blen != c->last_sps_len || memcmp(both, c->last_sps, blen)) {
		memcpy(c->last_sps, both, blen);
		c->last_sps_len = blen;
		memset(c->last_pps_len, 0, sizeof(c->last_pps_len));
		uint8_t hv[2] = { 32 << 1, 1 }, hs[2] = { 33 << 1, 1 };
		if (!au_nal(c, hv, 2, v.buf, v.len) || !au_nal(c, hs, 2, s.buf, s.len))
			return false;
	}
	for (unsigned id = 0; id < 64; id++) {
		if (!c->pps_used[id])
			continue;
		struct bw p = { 0 };
		hevc_pps(c, &p, id);
		if (p.overflow || p.len > sizeof(c->last_pps[0]))
			return false;
		if (p.len != c->last_pps_len[id] || memcmp(p.buf, c->last_pps[id], p.len)) {
			memcpy(c->last_pps[id], p.buf, p.len);
			c->last_pps_len[id] = (uint16_t)p.len;
			uint8_t h[2] = { 34 << 1, 1 };
			if (!au_nal(c, h, 2, p.buf, p.len))
				return false;
		}
	}
	if (!au_grow(c, c->slices_len))
		return false;
	memcpy(c->au + c->au_len, c->slices, c->slices_len);
	c->au_len += c->slices_len;
	return true;
}

static VAStatus vr_RenderPicture(VADriverContextP ctx, VAContextID cid, VABufferID *ids, int n)
{
	struct driver *d = DRV(ctx);
	VAStatus st = VA_STATUS_SUCCESS;
	pthread_mutex_lock(&d->lock);
	struct context *c = get_obj(d, cid, OBJ_CONTEXT);
	if (!c) {
		pthread_mutex_unlock(&d->lock);
		return VA_STATUS_ERROR_INVALID_CONTEXT;
	}
	struct buffer *slice_params = NULL;
	for (int i = 0; i < n && st == VA_STATUS_SUCCESS; i++) {
		struct buffer *b = get_obj(d, ids[i], OBJ_BUFFER);
		if (!b) {
			st = VA_STATUS_ERROR_INVALID_BUFFER;
			break;
		}
		switch (b->type) {
		case VAPictureParameterBufferType:
			if (profile_fourcc(c->profile) == V4L2_PIX_FMT_H264 && b->size >= sizeof(c->h264_pic)) {
				memcpy(&c->h264_pic, b->data, sizeof(c->h264_pic));
				c->have_pic = true;
			} else if (profile_fourcc(c->profile) == V4L2_PIX_FMT_VP9 && b->size >= sizeof(c->vp9_pic)) {
				memcpy(&c->vp9_pic, b->data, sizeof(c->vp9_pic));
				c->have_pic = true;
			} else if (profile_fourcc(c->profile) == V4L2_PIX_FMT_HEVC && b->size >= sizeof(c->hevc_pic)) {
				memcpy(&c->hevc_pic, b->data, sizeof(c->hevc_pic));
				c->have_pic = true;
			}
			break;
		case VAIQMatrixBufferType:
			if (profile_fourcc(c->profile) == V4L2_PIX_FMT_HEVC && b->size >= sizeof(c->hevc_iq)) {
				memcpy(&c->hevc_iq, b->data, sizeof(c->hevc_iq));
				c->have_iq = true;
			} else if (profile_fourcc(c->profile) == V4L2_PIX_FMT_H264 && b->size >= sizeof(c->h264_iq)) {
				memcpy(&c->h264_iq, b->data, sizeof(c->h264_iq));
				c->have_iq = true;
			}
			break;
		case VASliceParameterBufferType:
			slice_params = b;
			break;
		case VASliceDataBufferType:
			if (!slice_params) {
				st = VA_STATUS_ERROR_INVALID_PARAMETER;
				break;
			}
			if (profile_fourcc(c->profile) == V4L2_PIX_FMT_H264) {
				st = h264_slices(c, slice_params, b);
			} else if (profile_fourcc(c->profile) == V4L2_PIX_FMT_HEVC) {
				st = hevc_slices(c, slice_params, b);
			} else {
				// VP9: the whole frame, headers included.
				VASliceParameterBufferVP9 sp;
				if (slice_params->size < sizeof(sp)) {
					st = VA_STATUS_ERROR_INVALID_BUFFER;
					break;
				}
				memcpy(&sp, slice_params->data, sizeof(sp));
				if ((uint64_t)sp.slice_data_offset + sp.slice_data_size > (uint64_t)b->size * b->num)
					st = VA_STATUS_ERROR_INVALID_BUFFER;
				else if (!slice_add(c, b->data + sp.slice_data_offset, sp.slice_data_size, false))
					st = VA_STATUS_ERROR_ALLOCATION_FAILED;
			}
			slice_params = NULL;
			break;
		default:
			// Probabilities, Huffman tables and the like: not ours.
			break;
		}
	}
	pthread_mutex_unlock(&d->lock);
	return st;
}

static VAStatus vr_EndPicture(VADriverContextP ctx, VAContextID cid)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct context *c = get_obj(d, cid, OBJ_CONTEXT);
	VAStatus st = VA_STATUS_ERROR_INVALID_CONTEXT;
	if (c) {
		struct surface *s = get_obj(d, c->target, OBJ_SURFACE);
		c->au_len = 0;
		uint32_t f = profile_fourcc(c->profile);
		bool built = f == V4L2_PIX_FMT_H264 ? h264_build(c) : f == V4L2_PIX_FMT_HEVC ? hevc_build(c) : frame_build(c);
		if (!s)
			st = VA_STATUS_ERROR_INVALID_SURFACE;
		else if (!built || c->slices_len == 0)
			st = VA_STATUS_ERROR_INVALID_PARAMETER;
		else
			st = submit(c, s);
		// Chromium shows the picture once this returns, from the
		// dma-buf it exported at the start.
		if (st == VA_STATUS_SUCCESS && s->live) {
			wait_surface(d, s);
			if (!s->failed && !upload(d, s))
				st = VA_STATUS_ERROR_OPERATION_FAILED;
		}
		c->au_len = 0;
		c->slices_len = 0;
	}
	pthread_mutex_unlock(&d->lock);
	return st;
}

static VAStatus vr_SyncSurface(VADriverContextP ctx, VASurfaceID id)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct surface *s = get_obj(d, id, OBJ_SURFACE);
	if (s)
		wait_surface(d, s);
	pthread_mutex_unlock(&d->lock);
	return s ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_SURFACE;
}

static VAStatus vr_QuerySurfaceStatus(VADriverContextP ctx, VASurfaceID id, VASurfaceStatus *status)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct surface *s = get_obj(d, id, OBJ_SURFACE);
	if (s) {
		if (s->pending && s->ctx)
			pump(s->ctx, 0);
		*status = s->pending ? VASurfaceRendering : VASurfaceReady;
	}
	pthread_mutex_unlock(&d->lock);
	return s ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_SURFACE;
}

static VAStatus vr_QuerySurfaceError(VADriverContextP ctx, VASurfaceID id, VAStatus error, void **info)
{
	(void)ctx, (void)id, (void)error;
	*info = NULL;
	return VA_STATUS_SUCCESS;
}

static VAImageFormat image_format(bool p010)
{
	VAImageFormat f = { .fourcc = p010 ? VA_FOURCC_P010 : VA_FOURCC_NV12, .byte_order = VA_LSB_FIRST, .bits_per_pixel = p010 ? 24 : 12 };
	return f;
}

static VAStatus vr_QueryImageFormats(VADriverContextP ctx, VAImageFormat *list, int *n)
{
	struct driver *d = DRV(ctx);
	list[0] = image_format(false);
	*n = 1;
	if (d->p010)
		list[(*n)++] = image_format(true);
	return VA_STATUS_SUCCESS;
}

static VAStatus make_image(struct driver *d, VAImageFormat *fmt, unsigned w, unsigned h, struct surface *borrow, VASurfaceID sid, VAImage *out)
{
	bool p010 = fmt->fourcc == VA_FOURCC_P010;
	if (fmt->fourcc != VA_FOURCC_NV12 && !p010)
		return VA_STATUS_ERROR_INVALID_IMAGE_FORMAT;
	if (w == 0 || h == 0 || w > 4096 || h > 4096)
		return VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED;
	unsigned pitch = borrow ? borrow->pitch : ((w + 15) & ~15u) * (p010 ? 2 : 1);
	unsigned rows = borrow ? borrow->rows : (h + 15) & ~15u;
	size_t size = (size_t)pitch * rows * 3 / 2;
	struct buffer *b = calloc(1, sizeof(*b));
	struct image *im = calloc(1, sizeof(*im));
	if (!b || !im) {
		free(b);
		free(im);
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	}
	b->type = VAImageBufferType;
	b->size = (unsigned)size;
	b->num = 1;
	b->borrowed = borrow != NULL;
	b->data = borrow ? borrow->data : calloc(1, size);
	if (!b->data) {
		free(b);
		free(im);
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	}
	VAImage *i = &im->image;
	i->format = *fmt;
	i->width = (uint16_t)w;
	i->height = (uint16_t)h;
	i->data_size = (uint32_t)size;
	i->num_planes = 2;
	i->pitches[0] = i->pitches[1] = pitch;
	i->offsets[0] = 0;
	i->offsets[1] = pitch * rows;
	i->buf = new_obj(d, OBJ_BUFFER, b);
	i->image_id = new_obj(d, OBJ_IMAGE, im);
	im->derived = borrow ? sid : VA_INVALID_ID;
	*out = *i;
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_CreateImage(VADriverContextP ctx, VAImageFormat *fmt, int w, int h, VAImage *out)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	VAStatus st = make_image(d, fmt, (unsigned)w, (unsigned)h, NULL, VA_INVALID_ID, out);
	pthread_mutex_unlock(&d->lock);
	return st;
}

static VAStatus vr_DeriveImage(VADriverContextP ctx, VASurfaceID sid, VAImage *out)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct surface *s = get_obj(d, sid, OBJ_SURFACE);
	VAStatus st = VA_STATUS_ERROR_INVALID_SURFACE;
	if (s) {
		wait_surface(d, s);
		VAImageFormat f = image_format(s->p010);
		st = make_image(d, &f, s->width, s->height, s, sid, out);
	}
	pthread_mutex_unlock(&d->lock);
	return st;
}

static VAStatus vr_DestroyImage(VADriverContextP ctx, VAImageID id)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct image *im = get_obj(d, id, OBJ_IMAGE);
	if (im) {
		struct buffer *b = get_obj(d, im->image.buf, OBJ_BUFFER);
		if (b) {
			if (!b->borrowed)
				free(b->data);
			free(b);
			del_obj(d, im->image.buf);
		}
		free(im);
		del_obj(d, id);
	}
	pthread_mutex_unlock(&d->lock);
	return im ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_IMAGE;
}

static VAStatus vr_SetImagePalette(VADriverContextP ctx, VAImageID id, unsigned char *p)
{
	(void)ctx, (void)id, (void)p;
	return VA_STATUS_ERROR_UNIMPLEMENTED;
}

// Copy a rectangle between a surface and an image of the same format.
static VAStatus copy_rect(struct surface *s, struct image *im, struct buffer *b, int x, int y, unsigned w, unsigned h, bool to_image)
{
	VAImage *i = &im->image;
	if ((i->format.fourcc == VA_FOURCC_P010) != s->p010)
		return VA_STATUS_ERROR_INVALID_IMAGE_FORMAT;
	if (x < 0 || y < 0 || (unsigned)x + w > s->width || (unsigned)y + h > s->height || w > i->width || h > i->height)
		return VA_STATUS_ERROR_INVALID_PARAMETER;
	unsigned bpp = s->p010 ? 2 : 1;
	x &= ~1;
	y &= ~1;
	for (int plane = 0; plane < 2; plane++) {
		unsigned rows = plane ? h / 2 : h;
		size_t sbase = plane ? (size_t)s->pitch * s->rows : 0;
		for (unsigned r = 0; r < rows; r++) {
			unsigned sy = (unsigned)(plane ? y / 2 : y) + r;
			uint8_t *sp = s->data + sbase + (size_t)sy * s->pitch + (size_t)x * bpp;
			uint8_t *ip = b->data + i->offsets[plane] + (size_t)r * i->pitches[plane];
			if (i->offsets[plane] + (size_t)r * i->pitches[plane] + (size_t)w * bpp > b->size)
				return VA_STATUS_ERROR_INVALID_PARAMETER;
			if (to_image)
				memcpy(ip, sp, (size_t)w * bpp);
			else
				memcpy(sp, ip, (size_t)w * bpp);
		}
	}
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_GetImage(VADriverContextP ctx, VASurfaceID sid, int x, int y, unsigned w, unsigned h, VAImageID iid)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct surface *s = get_obj(d, sid, OBJ_SURFACE);
	struct image *im = get_obj(d, iid, OBJ_IMAGE);
	struct buffer *b = im ? get_obj(d, im->image.buf, OBJ_BUFFER) : NULL;
	VAStatus st = VA_STATUS_ERROR_INVALID_PARAMETER;
	if (s && b) {
		wait_surface(d, s);
		st = b->borrowed ? VA_STATUS_SUCCESS : copy_rect(s, im, b, x, y, w, h, true);
	}
	pthread_mutex_unlock(&d->lock);
	return st;
}

static VAStatus vr_PutImage(VADriverContextP ctx, VASurfaceID sid, VAImageID iid, int sx, int sy, unsigned sw, unsigned sh, int dx, int dy,
			    unsigned dw, unsigned dh)
{
	struct driver *d = DRV(ctx);
	if (sx != 0 || sy != 0 || sw != dw || sh != dh)
		return VA_STATUS_ERROR_UNIMPLEMENTED;
	pthread_mutex_lock(&d->lock);
	struct surface *s = get_obj(d, sid, OBJ_SURFACE);
	struct image *im = get_obj(d, iid, OBJ_IMAGE);
	struct buffer *b = im ? get_obj(d, im->image.buf, OBJ_BUFFER) : NULL;
	VAStatus st = VA_STATUS_ERROR_INVALID_PARAMETER;
	if (s && b && !b->borrowed) {
		wait_surface(d, s);
		st = copy_rect(s, im, b, dx, dy, dw, dh, false);
		s->bo_stale = true;
	}
	pthread_mutex_unlock(&d->lock);
	return st;
}

static void bo_free(struct surface *s)
{
	for (int k = 0; k < 2; k++) {
		if (s->bo_map[k])
			munmap(s->bo_map[k], s->bo_map_len[k]);
		s->bo_map[k] = NULL;
		if (s->bo[k])
			gbm_bo_destroy(s->bo[k]);
		s->bo[k] = NULL;
	}
}

// Write one plane into a virtio-gpu buffer: into its guest pages, then
// one transfer to the host. (GBM's own map reads the buffer back from
// the host first and waits for it: 60 ms a 1080p picture.) False: not a
// virtio-gpu buffer, use GBM's map.
static bool upload_virtgpu(struct driver *d, struct surface *s, int k, const uint8_t *src, size_t src_pitch, size_t row, unsigned rows)
{
	uint32_t handle = gbm_bo_get_handle(s->bo[k]).u32;
	uint32_t stride = gbm_bo_get_stride(s->bo[k]);
	size_t len = (size_t)stride * rows;
	if (!s->bo_map[k]) {
		struct drm_virtgpu_map m = { .handle = handle };
		if (xioctl(d->drm_fd, DRM_IOCTL_VIRTGPU_MAP, &m))
			return false;
		void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, d->drm_fd, (off_t)m.offset);
		if (p == MAP_FAILED)
			return false;
		s->bo_map[k] = p;
		s->bo_map_len[k] = len;
	}
	// The host may still be reading the last picture's transfer.
	struct drm_virtgpu_3d_wait w = { .handle = handle };
	xioctl(d->drm_fd, DRM_IOCTL_VIRTGPU_WAIT, &w);
	for (unsigned y = 0; y < rows; y++)
		memcpy(s->bo_map[k] + (size_t)y * stride, src + (size_t)y * src_pitch, row);
	struct drm_virtgpu_3d_transfer_to_host t = {
		.bo_handle = handle,
		.box = { .w = gbm_bo_get_width(s->bo[k]), .h = rows, .d = 1 },
		.stride = stride,
	};
	return xioctl(d->drm_fd, DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST, &t) == 0;
}

// Fill the surface's GBM buffers from its picture.
static bool upload(struct driver *d, struct surface *s)
{
	if (!d->gbm)
		return false;
	unsigned bpp = s->p010 ? 2 : 1;
	uint32_t fmts[2] = { s->p010 ? GBM_FORMAT_R16 : GBM_FORMAT_R8, s->p010 ? GBM_FORMAT_GR1616 : GBM_FORMAT_GR88 };
	unsigned ws[2] = { s->width, (s->width + 1) / 2 }, hs[2] = { s->height, (s->height + 1) / 2 };
	for (int k = 0; k < 2; k++) {
		if (!s->bo[k]) {
			s->bo[k] = gbm_bo_create(d->gbm, ws[k], hs[k], fmts[k], GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);
			if (!s->bo[k]) {
				dbg("gbm_bo_create %ux%u: %s", ws[k], hs[k], strerror(errno));
				bo_free(s);
				return false;
			}
			s->bo_stale = true;
		}
	}
	if (!s->bo_stale)
		return true;
	for (int k = 0; k < 2; k++) {
		const uint8_t *src = s->data + (k ? (size_t)s->pitch * s->rows : 0);
		size_t row = (size_t)ws[k] * bpp * (k ? 2 : 1);
		if (row > s->pitch)
			row = s->pitch;
		if (upload_virtgpu(d, s, k, src, s->pitch, row, hs[k]))
			continue;
		uint32_t stride = 0;
		void *map_data = NULL;
		uint8_t *p = gbm_bo_map(s->bo[k], 0, 0, ws[k], hs[k], GBM_BO_TRANSFER_WRITE, &stride, &map_data);
		if (!p) {
			dbg("gbm_bo_map failed");
			return false;
		}
		for (unsigned y = 0; y < hs[k]; y++)
			memcpy(p + (size_t)y * stride, src + (size_t)y * s->pitch, row);
		gbm_bo_unmap(s->bo[k], map_data);
	}
	s->bo_stale = false;
	return true;
}

static VAStatus vr_ExportSurfaceHandle(VADriverContextP ctx, VASurfaceID sid, uint32_t mem_type, uint32_t flags, void *descriptor)
{
	struct driver *d = DRV(ctx);
	if (mem_type != VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2)
		return VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE;
	VADRMPRIMESurfaceDescriptor *desc = descriptor;
	pthread_mutex_lock(&d->lock);
	struct surface *s = get_obj(d, sid, OBJ_SURFACE);
	if (!s) {
		pthread_mutex_unlock(&d->lock);
		return VA_STATUS_ERROR_INVALID_SURFACE;
	}
	if (!s->pending && s->seq == 0)
		s->live = true;
	// The player reads the picture as soon as it has it.
	int64_t t0 = now_ms();
	wait_surface(d, s);
	int64_t t1 = now_ms();
	bool up = upload(d, s);
	if (debug_level > 1)
		dbg("export %u: waited %lld ms, upload %lld ms", sid, (long long)(t1 - t0), (long long)(now_ms() - t1));
	if (!up) {
		pthread_mutex_unlock(&d->lock);
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	}
	memset(desc, 0, sizeof(*desc));
	desc->fourcc = s->p010 ? VA_FOURCC_P010 : VA_FOURCC_NV12;
	desc->width = s->width;
	desc->height = s->height;
	desc->num_objects = 2;
	for (int k = 0; k < 2; k++) {
		int fd = gbm_bo_get_fd(s->bo[k]);
		if (fd < 0) {
			for (int j = 0; j < k; j++)
				close(desc->objects[j].fd);
			pthread_mutex_unlock(&d->lock);
			return VA_STATUS_ERROR_OPERATION_FAILED;
		}
		desc->objects[k].fd = fd;
		off_t end = lseek(fd, 0, SEEK_END);
		desc->objects[k].size = end > 0 ? (uint32_t)end : gbm_bo_get_stride(s->bo[k]) * gbm_bo_get_height(s->bo[k]);
		lseek(fd, 0, SEEK_SET);
		desc->objects[k].drm_format_modifier = gbm_bo_get_modifier(s->bo[k]);
	}
	if (flags & VA_EXPORT_SURFACE_COMPOSED_LAYERS) {
		desc->num_layers = 1;
		desc->layers[0].drm_format = s->p010 ? DRM_FORMAT_P010 : DRM_FORMAT_NV12;
		desc->layers[0].num_planes = 2;
		for (int k = 0; k < 2; k++) {
			desc->layers[0].object_index[k] = (uint32_t)k;
			desc->layers[0].pitch[k] = gbm_bo_get_stride(s->bo[k]);
		}
	} else {
		desc->num_layers = 2;
		uint32_t fmts[2] = { s->p010 ? DRM_FORMAT_R16 : DRM_FORMAT_R8, s->p010 ? DRM_FORMAT_GR1616 : DRM_FORMAT_GR88 };
		for (int k = 0; k < 2; k++) {
			desc->layers[k].drm_format = fmts[k];
			desc->layers[k].num_planes = 1;
			desc->layers[k].object_index[0] = (uint32_t)k;
			desc->layers[k].pitch[0] = gbm_bo_get_stride(s->bo[k]);
		}
	}
	pthread_mutex_unlock(&d->lock);
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_PutSurface(VADriverContextP ctx, VASurfaceID s, void *draw, short sx, short sy, unsigned short sw, unsigned short sh,
			      short dx, short dy, unsigned short dw, unsigned short dh, VARectangle *clip, unsigned nclip, unsigned flags)
{
	(void)ctx, (void)s, (void)draw, (void)sx, (void)sy, (void)sw, (void)sh, (void)dx, (void)dy, (void)dw, (void)dh;
	(void)clip, (void)nclip, (void)flags;
	return VA_STATUS_ERROR_UNIMPLEMENTED;
}

static VAStatus vr_QuerySubpictureFormats(VADriverContextP ctx, VAImageFormat *list, unsigned *flags, unsigned *n)
{
	(void)ctx, (void)list, (void)flags;
	*n = 0;
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_Unimplemented(void)
{
	return VA_STATUS_ERROR_UNIMPLEMENTED;
}

static VAStatus vr_QueryDisplayAttributes(VADriverContextP ctx, VADisplayAttribute *list, int *n)
{
	(void)ctx, (void)list;
	*n = 0;
	return VA_STATUS_SUCCESS;
}

static VAStatus vr_BufferInfo(VADriverContextP ctx, VABufferID id, VABufferType *type, unsigned *size, unsigned *num)
{
	struct driver *d = DRV(ctx);
	pthread_mutex_lock(&d->lock);
	struct buffer *b = get_obj(d, id, OBJ_BUFFER);
	if (b) {
		*type = b->type;
		*size = b->size;
		*num = b->num;
	}
	pthread_mutex_unlock(&d->lock);
	return b ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_BUFFER;
}

#define STR2(x) #x
#define STR(x) STR2(x)
#define INIT_NAME2(a, b) __vaDriverInit_##a##_##b
#define INIT_NAME(a, b) INIT_NAME2(a, b)

__attribute__((visibility("default"))) VAStatus INIT_NAME(VA_MAJOR_VERSION, VA_MINOR_VERSION)(VADriverContextP ctx);

VAStatus INIT_NAME(VA_MAJOR_VERSION, VA_MINOR_VERSION)(VADriverContextP ctx)
{
	struct driver *d = calloc(1, sizeof(*d));
	if (!d)
		return VA_STATUS_ERROR_ALLOCATION_FAILED;
	pthread_mutex_init(&d->lock, NULL);
	if (!find_device(d)) {
		dbg("no vroom video decoder (is video_decode on, and the virtio-media driver installed?)");
		pthread_mutex_destroy(&d->lock);
		free(d);
		return VA_STATUS_ERROR_UNKNOWN;
	}
	struct drm_state *drm = ctx->drm_state;
	d->drm_fd = drm ? drm->fd : -1;
	if (d->drm_fd >= 0)
		d->gbm = gbm_create_device(d->drm_fd);
	if (!d->gbm)
		dbg("no GBM device: surfaces can't be exported as dma-bufs");
	ctx->pDriverData = d;
	ctx->version_major = VA_MAJOR_VERSION;
	ctx->version_minor = VA_MINOR_VERSION;
	ctx->max_profiles = MAX_PROFILES;
	ctx->max_entrypoints = MAX_ENTRYPOINTS;
	ctx->max_attributes = MAX_ATTRIBUTES;
	ctx->max_image_formats = MAX_IMAGE_FORMATS;
	ctx->max_subpic_formats = 1;
	ctx->max_display_attributes = 1;
	ctx->str_vendor = VENDOR;

	struct VADriverVTable *vt = ctx->vtable;
	memset(vt, 0, sizeof(*vt));
	vt->vaTerminate = vr_Terminate;
	vt->vaQueryConfigProfiles = vr_QueryConfigProfiles;
	vt->vaQueryConfigEntrypoints = vr_QueryConfigEntrypoints;
	vt->vaGetConfigAttributes = vr_GetConfigAttributes;
	vt->vaCreateConfig = vr_CreateConfig;
	vt->vaDestroyConfig = vr_DestroyConfig;
	vt->vaQueryConfigAttributes = vr_QueryConfigAttributes;
	vt->vaCreateSurfaces = vr_CreateSurfaces;
	vt->vaCreateSurfaces2 = vr_CreateSurfaces2;
	vt->vaDestroySurfaces = vr_DestroySurfaces;
	vt->vaQuerySurfaceAttributes = vr_QuerySurfaceAttributes;
	vt->vaCreateContext = vr_CreateContext;
	vt->vaDestroyContext = vr_DestroyContext;
	vt->vaCreateBuffer = vr_CreateBuffer;
	vt->vaBufferSetNumElements = vr_BufferSetNumElements;
	vt->vaMapBuffer = vr_MapBuffer;
	vt->vaUnmapBuffer = vr_UnmapBuffer;
	vt->vaDestroyBuffer = vr_DestroyBuffer;
	vt->vaBufferInfo = vr_BufferInfo;
	vt->vaBeginPicture = vr_BeginPicture;
	vt->vaRenderPicture = vr_RenderPicture;
	vt->vaEndPicture = vr_EndPicture;
	vt->vaSyncSurface = vr_SyncSurface;
	vt->vaQuerySurfaceStatus = vr_QuerySurfaceStatus;
	vt->vaQuerySurfaceError = vr_QuerySurfaceError;
	vt->vaPutSurface = vr_PutSurface;
	vt->vaQueryImageFormats = vr_QueryImageFormats;
	vt->vaCreateImage = vr_CreateImage;
	vt->vaDeriveImage = vr_DeriveImage;
	vt->vaDestroyImage = vr_DestroyImage;
	vt->vaSetImagePalette = vr_SetImagePalette;
	vt->vaGetImage = vr_GetImage;
	vt->vaPutImage = vr_PutImage;
	vt->vaQuerySubpictureFormats = vr_QuerySubpictureFormats;
	vt->vaCreateSubpicture = (void *)vr_Unimplemented;
	vt->vaDestroySubpicture = (void *)vr_Unimplemented;
	vt->vaSetSubpictureImage = (void *)vr_Unimplemented;
	vt->vaSetSubpictureChromakey = (void *)vr_Unimplemented;
	vt->vaSetSubpictureGlobalAlpha = (void *)vr_Unimplemented;
	vt->vaAssociateSubpicture = (void *)vr_Unimplemented;
	vt->vaDeassociateSubpicture = (void *)vr_Unimplemented;
	vt->vaQueryDisplayAttributes = vr_QueryDisplayAttributes;
	vt->vaGetDisplayAttributes = (void *)vr_Unimplemented;
	vt->vaSetDisplayAttributes = (void *)vr_Unimplemented;
	vt->vaLockSurface = (void *)vr_Unimplemented;
	vt->vaUnlockSurface = (void *)vr_Unimplemented;
	vt->vaExportSurfaceHandle = vr_ExportSurfaceHandle;
	dbg("ready (%s)", d->video);
	return VA_STATUS_SUCCESS;
}
