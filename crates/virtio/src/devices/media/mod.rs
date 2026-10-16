//! virtio-media (device 48) as a video decoder: the guest sees a V4L2
//! stateful decoder (`/dev/videoN`, H.264 and HEVC in, NV12 out), as on
//! a phone or a Raspberry Pi; GStreamer's `v4l2h264dec` and FFmpeg's
//! `h264_v4l2m2m` use it as is. The guest driver is Google's virtio-media
//! (out of tree for now, see docs/GUIDE.md).
//!
//! virtio-media carries V4L2 ioctls: the guest kernel forwards each one
//! on the command queue, we answer, and finished buffers go back on the
//! event queue. Buffers live in a shared memory window (region 0) the
//! guest maps; we copy bitstreams out of it and pictures into it.
//!
//! No decoding happens here. Bitstreams go to `vroom video-helper` (a
//! sandboxed process using VideoToolbox) through a [`HelperLink`]; we
//! only grant it as many pictures as the guest gave us buffers for (see
//! `vroom_video::proto`). Everything from the guest and the helper is
//! checked: buffer counts and sizes, indices, lengths, the window's
//! bounds.

pub mod v4l2;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use vroom_memory::{GuestAddress, GuestMemory};
use vroom_video::proto::{FromHelper, ToHelper, MAX_SESSIONS};
use vroom_video::{Codec, Layout, MAX_BITSTREAM, MAX_SIDE, MIN_SIDE};

use crate::worker::{Poker, QueueCtx, QueueHandler, WorkerSlot, POKE_TOKEN, RESUME_TOKEN};
use crate::{types, ActivateContext, DeviceState, Reader, Result, VirtioDevice, Writer};
use v4l2::*;

/// The shared memory window holding every buffer of every session.
pub const WINDOW: u64 = 512 << 20;
/// Buffers start at multiples of this in the window.
const BUF_ALIGN: u64 = 64 << 10;
const MAX_OUTPUT_BUFS: u32 = 16;
const MAX_CAPTURE_BUFS: u32 = 24;
const MIN_BITSTREAM: u32 = 64 << 10;
const DEFAULT_BITSTREAM: u32 = 2 << 20;
/// Bitstream bytes at the helper per session; the rest waits here.
const MAX_IN_FLIGHT: usize = 16 << 20;
const MAX_IN_FLIGHT_BUFS: usize = 32;
/// Events waiting for the guest to give us event buffers.
const MAX_EVENTS: usize = 1024;

const CMD_OPEN: u32 = 1;
const CMD_CLOSE: u32 = 2;
const CMD_IOCTL: u32 = 3;
const CMD_MMAP: u32 = 4;
const CMD_MUNMAP: u32 = 5;
const EVT_ERROR: u32 = 0;
const EVT_DQBUF: u32 = 1;
const EVT_EVENT: u32 = 2;

const EIO: u32 = 5;
const ENOMEM: u32 = 12;
const EBUSY: u32 = 16;
const EINVAL: u32 = 22;
const ENOTTY: u32 = 25;

/// The decoding side: `vroom video-helper` behind a socket (the VMM
/// implements it), or a fake in tests.
pub trait HelperLink: Send + Sync {
    /// False: the helper is gone.
    fn send(&self, m: &ToHelper) -> bool;
    fn try_recv(&self) -> Option<FromHelper>;
    /// Wake the device when messages come in.
    fn set_poker(&self, p: Poker);
    fn alive(&self) -> bool;
}

/// Maps the window into the guest where the guest put it.
pub trait WindowPlacer: Send + Sync {
    fn place(&self, gpa: u64);
}

pub struct Media {
    link: Arc<dyn HelperLink>,
    window: GuestMemory,
    placer: Arc<dyn WindowPlacer>,
    card: String,
    worker: WorkerSlot<Handler>,
}

impl std::fmt::Debug for Media {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Media").finish_non_exhaustive()
    }
}

impl Media {
    /// `window`: [`WINDOW`] bytes at guest address 0 (our view of it).
    pub fn new(link: Arc<dyn HelperLink>, window: GuestMemory, placer: Arc<dyn WindowPlacer>, card: &str) -> Self {
        Media { link, window, placer, card: card.to_string(), worker: WorkerSlot::default() }
    }

    fn handler(&self) -> Handler {
        Handler {
            link: self.link.clone(),
            window: self.window.clone(),
            sessions: BTreeMap::new(),
            alloc: Alloc::default(),
            events: VecDeque::new(),
            next_cookie: 1,
            helper_lost: false,
        }
    }
}

/// First-fit allocator over the window.
#[derive(Debug, Default)]
struct Alloc {
    used: BTreeMap<u64, u64>,
}

impl Alloc {
    fn alloc(&mut self, len: u64) -> Option<u64> {
        let len = len.div_ceil(BUF_ALIGN) * BUF_ALIGN;
        let mut at = 0;
        for (&off, &l) in &self.used {
            if off - at >= len {
                break;
            }
            at = off + l;
        }
        if at + len > WINDOW {
            return None;
        }
        self.used.insert(at, len);
        Some(at)
    }

    fn free(&mut self, off: u64) {
        self.used.remove(&off);
    }

    fn len(&self, off: u64) -> Option<u64> {
        self.used.get(&off).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Output,
    Capture,
}

fn dir(t: u32) -> Option<Dir> {
    match t {
        OUTPUT_MPLANE => Some(Dir::Output),
        CAPTURE_MPLANE => Some(Dir::Capture),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct Buf {
    off: u64,
    len: u32,
    queued: bool,
    bytesused: u32,
    ts: [u8; 16],
}

/// A bitstream buffer on its way to the helper.
#[derive(Debug, Clone)]
struct Input {
    cookie: u32,
    index: u32,
    start: u32,
    len: u32,
}

#[derive(Debug)]
struct Session {
    codec: Codec,
    out_w: u32,
    out_h: u32,
    sizeimage: u32,
    /// The stream's layout once the helper said; before that, what the
    /// guest asked for.
    layout: Option<Layout>,
    provisional: Layout,
    visible: (u32, u32),
    min_buffers: u32,
    out_bufs: Vec<Buf>,
    cap_bufs: Vec<Buf>,
    out_streaming: bool,
    cap_streaming: bool,
    /// The helper has this session open (with `codec`).
    open: Option<Codec>,
    pending: VecDeque<Input>,
    /// Bitstream at the helper: cookie -> bytes.
    in_flight: BTreeMap<u32, usize>,
    in_flight_bytes: usize,
    stamps: VecDeque<(u32, [u8; 16])>,
    cap_queue: VecDeque<u32>,
    epoch: u32,
    granted: u32,
    /// The size changed mid-stream: no pictures until the guest restarts
    /// its picture queue.
    awaiting_restart: bool,
    /// The next picture buffer goes back empty with the LAST flag.
    last_pending: bool,
    last_is_eos: bool,
    draining: bool,
    drain_sent: bool,
    sub_src: bool,
    sub_eos: bool,
    sequence: u32,
    dead: bool,
}

impl Session {
    fn new() -> Self {
        let provisional = Layout { width: 1920, height: 1088 };
        Session {
            codec: Codec::H264,
            out_w: 1920,
            out_h: 1080,
            sizeimage: DEFAULT_BITSTREAM,
            layout: None,
            provisional,
            visible: (1920, 1080),
            min_buffers: 4,
            out_bufs: Vec::new(),
            cap_bufs: Vec::new(),
            out_streaming: false,
            cap_streaming: false,
            open: None,
            pending: VecDeque::new(),
            in_flight: BTreeMap::new(),
            in_flight_bytes: 0,
            stamps: VecDeque::new(),
            cap_queue: VecDeque::new(),
            epoch: 0,
            granted: 0,
            awaiting_restart: false,
            last_pending: false,
            last_is_eos: false,
            draining: false,
            drain_sent: false,
            sub_src: false,
            sub_eos: false,
            sequence: 0,
            dead: false,
        }
    }

    fn cap_layout(&self) -> Layout {
        self.layout.unwrap_or(self.provisional)
    }

    fn bufs(&mut self, d: Dir) -> &mut Vec<Buf> {
        match d {
            Dir::Output => &mut self.out_bufs,
            Dir::Capture => &mut self.cap_bufs,
        }
    }
}

struct Handler {
    link: Arc<dyn HelperLink>,
    window: GuestMemory,
    sessions: BTreeMap<u32, Session>,
    alloc: Alloc,
    events: VecDeque<Vec<u8>>,
    next_cookie: u32,
    helper_lost: bool,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler").field("sessions", &self.sessions.len()).finish()
    }
}

/// An ioctl's answer: a payload, or an error number.
type Answer = std::result::Result<Vec<u8>, u32>;

fn read_payload(r: &mut Reader, n: usize) -> std::result::Result<Vec<u8>, u32> {
    let mut v = vec![0u8; n];
    r.read_exact(&mut v).map_err(|_| EINVAL)?;
    Ok(v)
}

fn resp_header(status: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..4].copy_from_slice(&status.to_le_bytes());
    h
}

fn coded_pixfmt(c: Codec) -> u32 {
    match c {
        Codec::H264 => PIX_H264,
        Codec::Hevc => PIX_HEVC,
    }
}

impl Handler {
    fn event(&mut self, ev: Vec<u8>) {
        if self.events.len() >= MAX_EVENTS {
            log::warn!("media: guest isn't taking events, dropping one");
            self.events.pop_front();
        }
        self.events.push_back(ev);
    }

    fn v4l2_event(&mut self, session: u32, kind: u32, changes: u32) {
        log::debug!("media: session {session}: event {kind}");
        let mut ev = vec![0u8; 8 + EVENT_SIZE];
        put_u32(&mut ev, 0, EVT_EVENT);
        put_u32(&mut ev, 4, session);
        put_u32(&mut ev, 8, kind);
        put_u32(&mut ev, 16, changes);
        self.event(ev);
    }

    fn error_event(&mut self, session: u32, errno: u32) {
        let mut ev = vec![0u8; 16];
        put_u32(&mut ev, 0, EVT_ERROR);
        put_u32(&mut ev, 4, session);
        put_u32(&mut ev, 8, errno);
        self.event(ev);
    }

    /// A buffer goes back to the guest.
    fn dqbuf_event(&mut self, session: u32, d: Dir, index: u32, flags: u32, sequence: u32) {
        let Some(s) = self.sessions.get_mut(&session) else { return };
        let Some(b) = s.bufs(d).get_mut(index as usize) else { return };
        b.queued = false;
        log::trace!("media: session {session}: {d:?} buffer {index} back, {} bytes, flags {flags:#x}", b.bytesused);
        let mut ev = vec![0u8; 8 + BUFFER_SIZE + MAX_PLANES * PLANE_SIZE];
        put_u32(&mut ev, 0, EVT_DQBUF);
        put_u32(&mut ev, 4, session);
        let o = 8;
        put_u32(&mut ev, o + buf::INDEX, index);
        put_u32(&mut ev, o + buf::TYPE, if d == Dir::Output { OUTPUT_MPLANE } else { CAPTURE_MPLANE });
        put_u32(&mut ev, o + buf::FLAGS, flags | BUF_FLAG_TIMESTAMP_COPY);
        put_u32(&mut ev, o + buf::FIELD, FIELD_NONE);
        ev[o + buf::TIMESTAMP..o + buf::TIMESTAMP + 16].copy_from_slice(&b.ts);
        put_u32(&mut ev, o + buf::SEQUENCE, sequence);
        put_u32(&mut ev, o + buf::MEMORY, MEMORY_MMAP);
        put_u32(&mut ev, o + buf::LENGTH, 1);
        let p = o + BUFFER_SIZE;
        put_u32(&mut ev, p + plane::BYTESUSED, b.bytesused);
        put_u32(&mut ev, p + plane::LENGTH, b.len);
        put_u32(&mut ev, p + plane::MEM_OFFSET, b.off as u32);
        self.event(ev);
    }

    /// Hand queued events to the guest.
    fn flush_events(&mut self, ctx: &mut QueueCtx) {
        while !self.events.is_empty() {
            let chain = match ctx.queues[1].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("media: eventq: {e}");
                    break;
                }
            };
            let head = chain.head();
            let Some(ev) = self.events.pop_front() else { break };
            let mut n = 0;
            if let Ok((_, mut w)) = chain.into_rw() {
                if w.available() >= ev.len() && w.write_all(&ev).is_ok() {
                    n = ev.len() as u32;
                }
            }
            ctx.add_used(1, head, n);
        }
    }

    fn send(&mut self, m: &ToHelper) {
        if !self.link.send(m) && !self.helper_lost {
            self.lost();
        }
    }

    /// The helper died: every session fails.
    fn lost(&mut self) {
        log::warn!("media: the video helper is gone; decoding stops");
        self.helper_lost = true;
        let ids: Vec<u32> = self.sessions.keys().copied().collect();
        for id in ids {
            if let Some(s) = self.sessions.get_mut(&id) {
                if !s.dead {
                    s.dead = true;
                    self.error_event(id, EIO);
                }
            }
        }
    }

    // ---- credits and pictures ----

    /// Give the helper credits for picture buffers it hasn't got yet.
    fn grant(&mut self, id: u32) {
        let Some(s) = self.sessions.get_mut(&id) else { return };
        let Some(layout) = s.layout else { return };
        let fits = s.cap_bufs.iter().all(|b| b.len >= layout.size());
        if s.dead || s.open.is_none() || !s.cap_streaming || s.awaiting_restart || s.last_pending || !fits {
            return;
        }
        let have = s.cap_queue.len() as u32;
        if have > s.granted {
            let m = ToHelper::Credit { session: id, epoch: s.epoch, n: have - s.granted };
            s.granted = have;
            self.send(&m);
        }
    }

    /// Start a new credit epoch: pictures in flight under the old one are
    /// dropped when they come.
    fn new_epoch(&mut self, id: u32) {
        let Some(s) = self.sessions.get_mut(&id) else { return };
        s.epoch = s.epoch.wrapping_add(1);
        s.granted = 0;
        if s.open.is_some() {
            let m = ToHelper::Credit { session: id, epoch: s.epoch, n: 0 };
            self.send(&m);
        }
    }

    /// Return the empty LAST buffer if one is due and a buffer is queued.
    fn deliver_last(&mut self, id: u32) {
        let Some(s) = self.sessions.get_mut(&id) else { return };
        if !s.last_pending || !s.cap_streaming {
            return;
        }
        let Some(index) = s.cap_queue.pop_front() else { return };
        s.last_pending = false;
        let eos = std::mem::take(&mut s.last_is_eos) && s.sub_eos;
        let seq = s.sequence;
        s.sequence = s.sequence.wrapping_add(1);
        if let Some(b) = s.cap_bufs.get_mut(index as usize) {
            b.bytesused = 0;
        }
        self.dqbuf_event(id, Dir::Capture, index, BUF_FLAG_LAST, seq);
        if eos {
            self.v4l2_event(id, EVENT_EOS, 0);
        }
        self.new_epoch(id);
    }

    /// Send bitstream buffers to the helper, up to the in-flight cap.
    fn pump(&mut self, id: u32) {
        loop {
            let Some(s) = self.sessions.get_mut(&id) else { return };
            if s.dead || s.open.is_none() || !s.out_streaming {
                return;
            }
            let Some(inp) = s.pending.front().cloned() else { break };
            if !s.in_flight.is_empty() && (s.in_flight_bytes + inp.len as usize > MAX_IN_FLIGHT || s.in_flight.len() >= MAX_IN_FLIGHT_BUFS) {
                return;
            }
            s.pending.pop_front();
            let Some(b) = s.out_bufs.get(inp.index as usize) else { continue };
            let mut data = vec![0u8; inp.len as usize];
            if self.window.read(GuestAddress(b.off + inp.start as u64), &mut data).is_err() {
                data.clear();
            }
            let ts = b.ts;
            s.in_flight.insert(inp.cookie, data.len());
            s.in_flight_bytes += data.len();
            s.stamps.push_back((inp.cookie, ts));
            while s.stamps.len() > 128 {
                s.stamps.pop_front();
            }
            self.send(&ToHelper::Decode { session: id, cookie: inp.cookie, data });
            // We have our copy: the guest may fill the buffer again. (A
            // player that finds every bitstream buffer busy at the end of
            // its input can mistake a free one for "no more pictures".)
            self.dqbuf_event(id, Dir::Output, inp.index, 0, 0);
        }
        let Some(s) = self.sessions.get_mut(&id) else { return };
        if s.draining && !s.drain_sent && s.pending.is_empty() {
            s.drain_sent = true;
            self.send(&ToHelper::Drain { session: id });
        }
    }

    fn from_helper(&mut self, m: FromHelper) {
        let id = m.session();
        let Some(s) = self.sessions.get_mut(&id) else { return };
        if s.dead {
            return;
        }
        if log::log_enabled!(log::Level::Trace) {
            let what = match &m {
                FromHelper::Picture { epoch, cookie, .. } => format!("picture {cookie} epoch {epoch} (ours {}, queued {})", s.epoch, s.cap_queue.len()),
                FromHelper::Consumed { cookie, ok, .. } => format!("consumed {cookie} {ok}"),
                m => format!("{m:?}"),
            };
            log::trace!("media: session {id}: helper: {what}");
        }
        match m {
            FromHelper::Format { layout, visible, min_buffers, .. } => {
                let old = s.layout.replace(layout);
                s.visible = visible;
                s.min_buffers = min_buffers;
                if old.is_some_and(|o| o != layout) && s.cap_streaming {
                    s.awaiting_restart = true;
                    s.last_pending = true;
                    s.last_is_eos = false;
                }
                let sub = s.sub_src;
                self.new_epoch(id);
                self.deliver_last(id);
                if sub {
                    self.v4l2_event(id, EVENT_SOURCE_CHANGE, SRC_CH_RESOLUTION);
                }
                self.grant(id);
            }
            FromHelper::Picture { epoch, cookie, data, .. } => {
                if epoch != s.epoch {
                    return;
                }
                let Some(layout) = s.layout else { return };
                if data.len() != layout.size() as usize {
                    log::warn!("media: picture of the wrong size from the helper");
                    return;
                }
                let Some(index) = s.cap_queue.pop_front() else { return };
                s.granted = s.granted.saturating_sub(1);
                let ts = s.stamps.iter().find(|(c, _)| *c == cookie).map_or([0; 16], |&(_, t)| t);
                let seq = s.sequence;
                s.sequence = s.sequence.wrapping_add(1);
                let Some(b) = s.cap_bufs.get_mut(index as usize) else { return };
                if (b.len as usize) < data.len() || self.window.write(GuestAddress(b.off), &data).is_err() {
                    b.bytesused = 0;
                    self.dqbuf_event(id, Dir::Capture, index, BUF_FLAG_ERROR, seq);
                    return;
                }
                b.bytesused = data.len() as u32;
                b.ts = ts;
                self.dqbuf_event(id, Dir::Capture, index, 0, seq);
            }
            FromHelper::Consumed { cookie, ok, .. } => {
                let Some(len) = s.in_flight.remove(&cookie) else { return };
                s.in_flight_bytes -= len;
                if !ok {
                    log::debug!("media: session {id}: a buffer didn't decode");
                }
                self.pump(id);
            }
            FromHelper::DrainDone { .. } => {
                log::debug!("media: session {id}: drained");
                if s.draining {
                    s.draining = false;
                    s.drain_sent = false;
                    s.last_pending = true;
                    s.last_is_eos = true;
                    self.deliver_last(id);
                }
            }
            FromHelper::Error { errno, .. } => {
                log::info!("media: session {id}: the helper gave up on the stream");
                s.dead = true;
                self.error_event(id, errno.clamp(1, 4095));
            }
        }
    }

    // ---- commands ----

    fn command(&mut self, r: &mut Reader, w: &mut Writer) {
        let mut h = [0u8; 8];
        if r.read_exact(&mut h).is_err() {
            return;
        }
        let cmd = get_u32(&h, 0);
        let out: Vec<u8> = match cmd {
            CMD_OPEN => {
                let free = (0..MAX_SESSIONS).find(|i| !self.sessions.contains_key(i));
                match free {
                    Some(id) if !self.helper_lost => {
                        self.sessions.insert(id, Session::new());
                        let mut v = resp_header(0).to_vec();
                        v.extend_from_slice(&id.to_le_bytes());
                        v.extend_from_slice(&[0; 4]);
                        v
                    }
                    Some(_) => resp_header(EIO).to_vec(),
                    None => resp_header(ENOMEM).to_vec(),
                }
            }
            CMD_CLOSE => {
                if let Ok(b) = read_payload(r, 8) {
                    self.close(get_u32(&b, 0));
                }
                Vec::new()
            }
            CMD_IOCTL => match read_payload(r, 8) {
                Ok(b) => {
                    let (id, code) = (get_u32(&b, 0), get_u32(&b, 4));
                    match self.ioctl(id, code, r) {
                        Ok(payload) => {
                            let mut v = resp_header(0).to_vec();
                            v.extend_from_slice(&payload);
                            v
                        }
                        Err(e) => resp_header(e).to_vec(),
                    }
                }
                Err(e) => resp_header(e).to_vec(),
            },
            CMD_MMAP => match read_payload(r, 12) {
                Ok(b) => {
                    let (id, off) = (get_u32(&b, 0), get_u32(&b, 8) as u64);
                    let ok = self.sessions.get(&id).is_some_and(|s| s.out_bufs.iter().chain(&s.cap_bufs).any(|x| x.off == off));
                    match self.alloc.len(off).filter(|_| ok) {
                        Some(len) => {
                            let mut v = resp_header(0).to_vec();
                            v.extend_from_slice(&off.to_le_bytes());
                            v.extend_from_slice(&len.to_le_bytes());
                            v
                        }
                        None => resp_header(EINVAL).to_vec(),
                    }
                }
                Err(e) => resp_header(e).to_vec(),
            },
            CMD_MUNMAP => resp_header(0).to_vec(),
            _ => resp_header(EINVAL).to_vec(),
        };
        if !out.is_empty() && w.write_all(&out[..out.len().min(w.available())]).is_err() {
            log::debug!("media: short response buffer");
        }
    }

    fn close(&mut self, id: u32) {
        let Some(s) = self.sessions.remove(&id) else { return };
        for b in s.out_bufs.iter().chain(&s.cap_bufs) {
            self.alloc.free(b.off);
        }
        if s.open.is_some() {
            self.send(&ToHelper::Close { session: id });
        }
        self.events.retain(|e| get_u32(e, 4) != id);
    }

    fn close_all(&mut self) {
        let ids: Vec<u32> = self.sessions.keys().copied().collect();
        for id in ids {
            self.close(id);
        }
    }

    fn ioctl(&mut self, id: u32, code: u32, r: &mut Reader) -> Answer {
        if !self.sessions.contains_key(&id) {
            return Err(EINVAL);
        }
        match code {
            ioc::ENUM_FMT => {
                let mut b = read_payload(r, FMTDESC_SIZE)?;
                let (index, t) = (get_u32(&b, 0), get_u32(&b, 4));
                let (fourcc, flags, name) = match (dir(t).ok_or(EINVAL)?, index) {
                    (Dir::Output, 0) => (PIX_H264, FMT_FLAG_COMPRESSED | FMT_FLAG_DYN_RESOLUTION, "H.264"),
                    (Dir::Output, 1) => (PIX_HEVC, FMT_FLAG_COMPRESSED | FMT_FLAG_DYN_RESOLUTION, "HEVC"),
                    (Dir::Capture, 0) => (PIX_NV12, 0, "Y/UV 4:2:0"),
                    _ => return Err(EINVAL),
                };
                b[8..].fill(0);
                put_u32(&mut b, 8, flags);
                put_str(&mut b, 12, 32, name);
                put_u32(&mut b, 44, fourcc);
                Ok(b)
            }
            ioc::ENUM_FRAMESIZES => {
                let mut b = read_payload(r, FRMSIZEENUM_SIZE)?;
                let f = get_u32(&b, 4);
                if get_u32(&b, 0) != 0 || ![PIX_H264, PIX_HEVC, PIX_NV12].contains(&f) {
                    return Err(EINVAL);
                }
                b[8..].fill(0);
                // Any size in between (1080 isn't a multiple of 16; the
                // pictures' buffers are rounded up, not the stream).
                put_u32(&mut b, 8, 2); // continuous
                for (i, v) in [MIN_SIDE, MAX_SIDE, 1, MIN_SIDE, MAX_SIDE, 1].into_iter().enumerate() {
                    put_u32(&mut b, 12 + 4 * i, v);
                }
                Ok(b)
            }
            ioc::G_FMT | ioc::S_FMT | ioc::TRY_FMT => {
                let mut b = read_payload(r, FORMAT_SIZE)?;
                let d = dir(get_u32(&b, fmt::TYPE)).ok_or(EINVAL)?;
                let s = self.sessions.get_mut(&id).ok_or(EINVAL)?;
                if code != ioc::G_FMT {
                    let (w, h) = (get_u32(&b, fmt::WIDTH), get_u32(&b, fmt::HEIGHT));
                    match d {
                        Dir::Output => {
                            let codec = if get_u32(&b, fmt::PIXELFORMAT) == PIX_HEVC { Codec::Hevc } else { Codec::H264 };
                            // A buffer must take a whole access unit: at least
                            // what the guest asks, and no less than a raw
                            // picture of that size (an intra picture can come
                            // close), within the caps.
                            let raw = w.clamp(MIN_SIDE, MAX_SIDE) * h.clamp(MIN_SIDE, MAX_SIDE) * 3 / 2;
                            let size = match get_u32(&b, fmt::SIZEIMAGE) {
                                0 => DEFAULT_BITSTREAM.max(raw),
                                n => n.max(raw),
                            }
                            .clamp(MIN_BITSTREAM, MAX_BITSTREAM);
                            if code == ioc::S_FMT {
                                if s.out_streaming {
                                    return Err(EBUSY);
                                }
                                s.codec = codec;
                                s.sizeimage = size;
                                if w > 0 && h > 0 {
                                    s.out_w = w.clamp(MIN_SIDE, MAX_SIDE);
                                    s.out_h = h.clamp(MIN_SIDE, MAX_SIDE);
                                    if s.layout.is_none() {
                                        if let Some(l) = Layout::for_picture(s.out_w, s.out_h) {
                                            s.provisional = l;
                                            s.visible = (s.out_w, s.out_h);
                                        }
                                    }
                                }
                            } else {
                                return Ok(output_format(b, codec, w.clamp(MIN_SIDE, MAX_SIDE), h.clamp(MIN_SIDE, MAX_SIDE), size));
                            }
                        }
                        Dir::Capture => {
                            // The stream decides; before it does, the guest may.
                            if s.layout.is_none() && code == ioc::S_FMT && !s.cap_streaming {
                                if let Some(l) = Layout::for_picture(w, h) {
                                    s.provisional = l;
                                    s.visible = (w, h);
                                }
                            }
                            if code == ioc::TRY_FMT && s.layout.is_none() {
                                let l = Layout::for_picture(w, h).unwrap_or(s.provisional);
                                return Ok(capture_format(b, l));
                            }
                        }
                    }
                }
                Ok(match d {
                    Dir::Output => output_format(b, s.codec, s.out_w, s.out_h, s.sizeimage),
                    Dir::Capture => {
                        let l = s.cap_layout();
                        b = capture_format(b, l);
                        b
                    }
                })
            }
            ioc::REQBUFS => {
                let mut b = read_payload(r, REQBUFS_SIZE)?;
                let (count, t, mem) = (get_u32(&b, 0), get_u32(&b, 4), get_u32(&b, 8));
                let d = dir(t).ok_or(EINVAL)?;
                if mem != MEMORY_MMAP {
                    return Err(EINVAL);
                }
                // Implicit stream off, then new buffers.
                self.streamoff(id, d);
                let s = self.sessions.get_mut(&id).ok_or(EINVAL)?;
                let (max, size) = match d {
                    Dir::Output => (MAX_OUTPUT_BUFS, s.sizeimage),
                    Dir::Capture => (MAX_CAPTURE_BUFS, s.cap_layout().size()),
                };
                let old = std::mem::take(s.bufs(d));
                for x in &old {
                    self.alloc.free(x.off);
                }
                let mut new = Vec::new();
                for _ in 0..count.min(max) {
                    match self.alloc.alloc(size as u64) {
                        Some(off) => new.push(Buf { off, len: size, queued: false, bytesused: 0, ts: [0; 16] }),
                        None => break,
                    }
                }
                if count > 0 && new.is_empty() {
                    return Err(ENOMEM);
                }
                let n = new.len() as u32;
                let s = self.sessions.get_mut(&id).ok_or(EINVAL)?;
                *s.bufs(d) = new;
                if d == Dir::Capture {
                    s.awaiting_restart = false;
                }
                put_u32(&mut b, 0, n);
                put_u32(&mut b, 12, BUF_CAP_SUPPORTS_MMAP);
                Ok(b)
            }
            ioc::QUERYBUF | ioc::QBUF => {
                let mut b = read_payload(r, BUFFER_SIZE)?;
                let (index, t) = (get_u32(&b, buf::INDEX), get_u32(&b, buf::TYPE));
                let d = dir(t).ok_or(EINVAL)?;
                let nplanes = get_u32(&b, buf::LENGTH) as usize;
                if !(1..=MAX_PLANES).contains(&nplanes) {
                    return Err(EINVAL);
                }
                let mut planes = read_payload(r, PLANE_SIZE * nplanes)?;
                if code == ioc::QBUF {
                    self.qbuf(id, d, index, &b, &planes)?;
                }
                let s = self.sessions.get_mut(&id).ok_or(EINVAL)?;
                let x = s.bufs(d).get(index as usize).ok_or(EINVAL)?.clone();
                put_u32(&mut b, buf::FLAGS, (if x.queued { BUF_FLAG_QUEUED } else { 0 }) | BUF_FLAG_TIMESTAMP_COPY);
                put_u32(&mut b, buf::MEMORY, MEMORY_MMAP);
                put_u32(&mut b, buf::FIELD, FIELD_NONE);
                put_u32(&mut b, buf::LENGTH, 1);
                b[buf::M..buf::M + 8].fill(0);
                planes[..PLANE_SIZE].fill(0);
                put_u32(&mut planes, plane::BYTESUSED, x.bytesused);
                put_u32(&mut planes, plane::LENGTH, x.len);
                put_u32(&mut planes, plane::MEM_OFFSET, x.off as u32);
                b.extend_from_slice(&planes);
                Ok(b)
            }
            ioc::STREAMON | ioc::STREAMOFF => {
                let b = read_payload(r, 4)?;
                let d = dir(get_u32(&b, 0)).ok_or(EINVAL)?;
                if code == ioc::STREAMOFF {
                    self.streamoff(id, d);
                    return Ok(Vec::new());
                }
                if self.helper_lost {
                    return Err(EIO);
                }
                let s = self.sessions.get_mut(&id).ok_or(EINVAL)?;
                if s.bufs(d).is_empty() {
                    return Err(EINVAL);
                }
                match d {
                    Dir::Output => {
                        s.out_streaming = true;
                        let codec = s.codec;
                        let was = s.open.replace(codec);
                        if was != Some(codec) {
                            s.layout = None;
                            s.epoch = 0;
                            s.granted = 0;
                            if was.is_some() {
                                self.send(&ToHelper::Close { session: id });
                            }
                            self.send(&ToHelper::Open { session: id, codec });
                        }
                        self.pump(id);
                    }
                    Dir::Capture => {
                        s.cap_streaming = true;
                        self.deliver_last(id);
                    }
                }
                self.grant(id);
                Ok(Vec::new())
            }
            ioc::SUBSCRIBE_EVENT | ioc::UNSUBSCRIBE_EVENT => {
                let b = read_payload(r, SUBSCRIPTION_SIZE)?;
                let s = self.sessions.get_mut(&id).ok_or(EINVAL)?;
                let on = code == ioc::SUBSCRIBE_EVENT;
                match get_u32(&b, 0) {
                    EVENT_SOURCE_CHANGE => s.sub_src = on,
                    EVENT_EOS => s.sub_eos = on,
                    EVENT_ALL if !on => {
                        s.sub_src = false;
                        s.sub_eos = false;
                    }
                    _ => return Err(EINVAL),
                }
                Ok(Vec::new())
            }
            ioc::G_SELECTION | ioc::S_SELECTION => {
                let mut b = read_payload(r, SELECTION_SIZE)?;
                let (t, target) = (get_u32(&b, 0), get_u32(&b, 4));
                if t != CAPTURE && t != CAPTURE_MPLANE {
                    return Err(EINVAL);
                }
                let s = self.sessions.get(&id).ok_or(EINVAL)?;
                let l = s.cap_layout();
                let (w, h) = match target {
                    SEL_TGT_CROP_BOUNDS | SEL_TGT_COMPOSE_PADDED | SEL_TGT_NATIVE_SIZE => (l.width, l.height),
                    SEL_TGT_CROP | SEL_TGT_CROP_DEFAULT | SEL_TGT_COMPOSE | SEL_TGT_COMPOSE_DEFAULT | SEL_TGT_COMPOSE_BOUNDS => s.visible,
                    _ => return Err(EINVAL),
                };
                put_i32(&mut b, 12, 0);
                put_i32(&mut b, 16, 0);
                put_u32(&mut b, 20, w);
                put_u32(&mut b, 24, h);
                Ok(b)
            }
            ioc::G_CTRL => {
                let mut b = read_payload(r, CONTROL_SIZE)?;
                let c = control(get_u32(&b, 0)).ok_or(EINVAL)?;
                let v = if c.id == CID_MIN_BUFFERS_FOR_CAPTURE { self.sessions.get(&id).map_or(4, |s| s.min_buffers) } else { c.default };
                put_u32(&mut b, 4, v);
                Ok(b)
            }
            ioc::QUERYCTRL => {
                let mut b = read_payload(r, QUERYCTRL_SIZE)?;
                let want = get_u32(&b, 0);
                let c = if want & CTRL_FLAG_NEXT_CTRL != 0 {
                    let after = want & !(CTRL_FLAG_NEXT_CTRL | CTRL_FLAG_NEXT_COMPOUND);
                    CONTROLS.iter().find(|c| c.id > after)
                } else {
                    control(want)
                }
                .ok_or(EINVAL)?;
                b.fill(0);
                put_u32(&mut b, 0, c.id);
                put_u32(&mut b, 4, if c.menu.is_empty() { CTRL_TYPE_INTEGER } else { CTRL_TYPE_MENU });
                put_str(&mut b, 8, 32, c.name);
                let flags = if c.id == CID_MIN_BUFFERS_FOR_CAPTURE { CTRL_FLAG_READ_ONLY | CTRL_FLAG_VOLATILE } else { 0 };
                for (i, v) in [c.min, c.max, 1, c.default, flags].into_iter().enumerate() {
                    put_u32(&mut b, 40 + 4 * i, v);
                }
                Ok(b)
            }
            ioc::QUERYMENU => {
                let mut b = read_payload(r, QUERYMENU_SIZE)?;
                let c = control(get_u32(&b, 0)).ok_or(EINVAL)?;
                let index = get_u32(&b, 4);
                if index < c.min || index > c.max {
                    return Err(EINVAL);
                }
                let name = c.menu.get(index as usize).copied().ok_or(EINVAL)?;
                put_str(&mut b, 8, 32, name);
                put_u32(&mut b, 40, 0);
                Ok(b)
            }
            ioc::DECODER_CMD | ioc::TRY_DECODER_CMD => {
                let mut b = read_payload(r, DECODER_CMD_SIZE)?;
                let cmd = get_u32(&b, 0);
                if cmd != DEC_CMD_START && cmd != DEC_CMD_STOP {
                    return Err(EINVAL);
                }
                b[4..].fill(0);
                if code == ioc::DECODER_CMD {
                    self.decoder_cmd(id, cmd);
                }
                Ok(b)
            }
            _ => Err(ENOTTY),
        }
    }

    fn qbuf(&mut self, id: u32, d: Dir, index: u32, b: &[u8], planes: &[u8]) -> std::result::Result<(), u32> {
        let cookie = self.next_cookie;
        log::trace!("media: session {id}: qbuf {d:?} {index}");
        let s = self.sessions.get_mut(&id).ok_or(EINVAL)?;
        if s.dead {
            return Err(EIO);
        }
        if get_u32(b, buf::MEMORY) != MEMORY_MMAP {
            return Err(EINVAL);
        }
        let x = s.bufs(d).get_mut(index as usize).ok_or(EINVAL)?;
        if x.queued {
            return Err(EINVAL);
        }
        x.queued = true;
        let mut ts = [0u8; 16];
        ts.copy_from_slice(&b[buf::TIMESTAMP..buf::TIMESTAMP + 16]);
        match d {
            Dir::Output => {
                let used = get_u32(planes, plane::BYTESUSED).min(x.len);
                let start = get_u32(planes, plane::DATA_OFFSET).min(used);
                x.ts = ts;
                x.bytesused = used;
                if used == start {
                    // Nothing in it: straight back.
                    self.dqbuf_event(id, Dir::Output, index, 0, 0);
                    return Ok(());
                }
                self.next_cookie = self.next_cookie.wrapping_add(1).max(1);
                s.pending.push_back(Input { cookie, index, start, len: used - start });
                self.pump(id);
            }
            Dir::Capture => {
                x.bytesused = 0;
                s.cap_queue.push_back(index);
                self.deliver_last(id);
                self.grant(id);
            }
        }
        Ok(())
    }

    fn streamoff(&mut self, id: u32, d: Dir) {
        log::debug!("media: session {id}: {d:?} stream off");
        let Some(s) = self.sessions.get_mut(&id) else { return };
        for b in s.bufs(d).iter_mut() {
            b.queued = false;
        }
        match d {
            Dir::Output => {
                let was = s.out_streaming || !s.pending.is_empty() || !s.in_flight.is_empty();
                s.out_streaming = false;
                s.pending.clear();
                s.in_flight.clear();
                s.in_flight_bytes = 0;
                s.draining = false;
                s.drain_sent = false;
                if was && s.open.is_some() {
                    self.send(&ToHelper::Flush { session: id });
                }
                self.new_epoch(id);
                self.grant(id);
            }
            Dir::Capture => {
                s.cap_streaming = false;
                s.cap_queue.clear();
                s.awaiting_restart = false;
                s.last_pending = false;
                self.new_epoch(id);
            }
        }
    }

    fn decoder_cmd(&mut self, id: u32, cmd: u32) {
        log::debug!("media: session {id}: decoder command {cmd}");
        let Some(s) = self.sessions.get_mut(&id) else { return };
        if cmd == DEC_CMD_START {
            s.draining = false;
            s.drain_sent = false;
            s.last_pending = false;
            self.grant(id);
            return;
        }
        if s.draining {
            return;
        }
        if s.out_streaming && s.open.is_some() {
            s.draining = true;
            self.pump(id);
        } else {
            s.last_pending = true;
            s.last_is_eos = true;
            self.deliver_last(id);
        }
    }

    fn messages(&mut self) {
        while let Some(m) = self.link.try_recv() {
            self.from_helper(m);
        }
        if !self.helper_lost && !self.link.alive() {
            self.lost();
        }
    }
}

/// A control the decoder has: read-only, the stream decides.
#[derive(Debug)]
struct Control {
    id: u32,
    name: &'static str,
    min: u32,
    max: u32,
    default: u32,
    /// Menu entries by value (empty: an integer).
    menu: &'static [&'static str],
}

/// What VideoToolbox decodes: H.264 up to High, level 5.2; HEVC Main,
/// level 5.1. Players read these to decide what to send.
const CONTROLS: &[Control] = &[
    Control { id: CID_MIN_BUFFERS_FOR_CAPTURE, name: "Min Number of Capture Buffers", min: 1, max: 32, default: 4, menu: &[] },
    Control {
        id: CID_H264_LEVEL,
        name: "H264 Level",
        min: 0,
        max: 16,
        default: 11,
        menu: &["1", "1b", "1.1", "1.2", "1.3", "2", "2.1", "2.2", "3", "3.1", "3.2", "4", "4.1", "4.2", "5", "5.1", "5.2"],
    },
    Control {
        id: CID_H264_PROFILE,
        name: "H264 Profile",
        min: 0,
        max: 4,
        default: 4,
        menu: &["Baseline", "Constrained Baseline", "Main", "Extended", "High"],
    },
    Control { id: CID_HEVC_PROFILE, name: "HEVC Profile", min: 0, max: 1, default: 0, menu: &["Main", "Main Still Picture"] },
    Control {
        id: CID_HEVC_LEVEL,
        name: "HEVC Level",
        min: 0,
        max: 8,
        default: 8,
        menu: &["1", "2", "2.1", "3", "3.1", "4", "4.1", "5", "5.1"],
    },
];

fn control(id: u32) -> Option<&'static Control> {
    CONTROLS.iter().find(|c| c.id == id)
}

fn output_format(mut b: Vec<u8>, codec: Codec, w: u32, h: u32, size: u32) -> Vec<u8> {
    b[8..].fill(0);
    put_u32(&mut b, fmt::WIDTH, w);
    put_u32(&mut b, fmt::HEIGHT, h);
    put_u32(&mut b, fmt::PIXELFORMAT, coded_pixfmt(codec));
    put_u32(&mut b, fmt::FIELD, FIELD_NONE);
    put_u32(&mut b, fmt::COLORSPACE, COLORSPACE_REC709);
    put_u32(&mut b, fmt::SIZEIMAGE, size);
    put_u8(&mut b, fmt::NUM_PLANES, 1);
    b
}

fn capture_format(mut b: Vec<u8>, l: Layout) -> Vec<u8> {
    b[8..].fill(0);
    put_u32(&mut b, fmt::WIDTH, l.width);
    put_u32(&mut b, fmt::HEIGHT, l.height);
    put_u32(&mut b, fmt::PIXELFORMAT, PIX_NV12);
    put_u32(&mut b, fmt::FIELD, FIELD_NONE);
    put_u32(&mut b, fmt::COLORSPACE, COLORSPACE_REC709);
    put_u32(&mut b, fmt::SIZEIMAGE, l.size());
    put_u32(&mut b, fmt::BYTESPERLINE, l.stride());
    put_u8(&mut b, fmt::NUM_PLANES, 1);
    put_u8(&mut b, fmt::YCBCR_ENC, YCBCR_ENC_709);
    put_u8(&mut b, fmt::QUANTIZATION, QUANTIZATION_LIM_RANGE);
    put_u8(&mut b, fmt::XFER_FUNC, XFER_FUNC_709);
    b
}

impl QueueHandler for Handler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        if q == 0 {
            self.messages();
            loop {
                let chain = match ctx.queues[0].pop(&ctx.mem) {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!("media: commandq: {e}");
                        break;
                    }
                };
                let head = chain.head();
                let mut n = 0;
                if let Ok((mut r, mut w)) = chain.into_rw() {
                    self.command(&mut r, &mut w);
                    n = w.written() as u32;
                }
                ctx.add_used(0, head, n);
            }
        }
        self.flush_events(ctx);
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, token: usize) {
        if token == POKE_TOKEN || token == RESUME_TOKEN {
            self.messages();
            self.flush_events(ctx);
        }
    }

    fn attach(&mut self, poker: Poker) {
        self.link.set_poker(poker);
    }
}

impl VirtioDevice for Media {
    fn device_type(&self) -> u32 {
        types::MEDIA
    }
    fn name(&self) -> String {
        "media".into()
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![128, 128]
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 40];
        put_u32(&mut cfg, 0, CAP_VIDEO_M2M_MPLANE | CAP_STREAMING | CAP_EXT_PIX_FORMAT);
        put_u32(&mut cfg, 4, 0); // VFL_TYPE_VIDEO
        put_str(&mut cfg, 8, 32, &self.card);
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn shm_regions(&self) -> Vec<(u8, u64)> {
        vec![(0, WINDOW)]
    }
    fn set_shm_base(&mut self, id: u8, gpa: u64) {
        if id == 0 {
            self.placer.place(gpa);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        let h = self.handler();
        self.worker.start("media", ctx, h, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.worker.stop_with(|h| h.close_all());
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        // Decoders don't survive a snapshot (VideoToolbox state can't be
        // saved): keep their ids, and tell the guest they failed on restore.
        let ids: Vec<u32> = self.worker.with(|_, h| h.sessions.keys().copied().collect()).unwrap_or_default();
        let data = serde_json::to_vec(&ids).map_err(|e| crate::Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        let ids: Vec<u32> = serde_json::from_slice(data).unwrap_or_default();
        let mut h = self.handler();
        for id in ids.into_iter().filter(|&i| i < MAX_SESSIONS) {
            let mut s = Session::new();
            s.dead = true;
            h.sessions.insert(id, s);
            h.error_event(id, EIO);
        }
        self.worker.start("media", ctx, h, true)
    }
}

#[cfg(test)]
mod tests;
