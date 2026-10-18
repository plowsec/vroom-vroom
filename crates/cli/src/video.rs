//! `vroom video-helper`: decodes the guest's H.264, HEVC, VP9 and AV1
//! streams with VideoToolbox for the VMM's virtio-media decoder.
//!
//! Started by the VMM in a sandbox of its own (VideoToolbox and the
//! services it uses, nothing else); stdin is a socket to the VMM carrying
//! `vroom_video::proto` messages. First comes the buffer window (shared
//! memory the guest's buffers live in): pictures are written straight
//! into the guest buffers the VMM grants. Everything the VMM sends is
//! guest data: streams go through `vroom_video` first, sizes and counts
//! are capped, a write goes only into a granted buffer inside the window,
//! and a malformed message ends the helper.
//!
//! `vroom video-helper --file IN --codec h264|hevc|vp9|av1 [--out OUT]
//! [--decode-order] [--split]` decodes a file the same way, for tests (pictures a
//! second, and the pictures, NV12 or P010, to compare with another
//! decoder). H.264 / HEVC files are Annex B streams, VP9 and AV1 are IVF.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use vroom_video::nal::{self, ParamSets};
use vroom_video::poc::{Picture, Poc, Reorder};
use vroom_video::proto::{FromHelper, ToHelper, MAX_GRANTS, MAX_HELD, MAX_RESUME, OPEN_DECODE_ORDER};
use vroom_video::vpx::{self, Config};
use vroom_video::{Codec, Layout, PixFmt};

#[cfg(target_os = "macos")]
use crate::vt::{Desc, Image, Session as Vt};

#[cfg(not(target_os = "macos"))]
mod novt {
    //! No VideoToolbox: every stream is refused.
    use vroom_video::vpx::Config;
    use vroom_video::{Codec, Layout, PixFmt};

    #[derive(Debug)]
    pub struct Image;

    impl Image {
        /// # Safety
        /// Never called: there are no images.
        pub unsafe fn write(&self, _: *mut u8, _: usize, _: Layout, _: PixFmt) -> bool {
            false
        }
    }

    pub struct Decoded {
        pub tag: u64,
        pub image: Option<Image>,
    }

    #[derive(Clone, Copy)]
    #[allow(dead_code)] // what the macOS session reads
    pub enum Desc<'a> {
        Nal(Codec, &'a [&'a [u8]], u32),
        Vp9(&'a Config),
        Av1(&'a Config, &'a [u8]),
    }

    pub struct Vt {
        pub layout: Layout,
        pub visible: (u32, u32),
        pub depth: u32,
    }

    impl Vt {
        pub fn new(_: Desc) -> Option<Vt> {
            None
        }
        pub fn accept(&mut self, _: Codec, _: &[&[u8]]) -> Option<bool> {
            None
        }
        pub fn decode(&mut self, _: &[u8], _: u64, _: i64, _: bool) -> bool {
            false
        }
        pub fn finish(&mut self) {}
        pub fn take(&mut self) -> Vec<Decoded> {
            Vec::new()
        }
    }

    pub fn caps() -> u32 {
        0
    }
}
#[cfg(not(target_os = "macos"))]
use novt::{Desc, Image, Vt};

/// Pictures the VMM is asked to keep in its buffers beyond what the
/// stream needs: VideoToolbox keeps its own references, so a few do.
const MIN_BUFFERS: u32 = 4;
/// Most parameter set updates or access units we act on per buffer.
const MAX_UNITS: usize = 256;

/// What goes back to the VMM, in order.
enum Out {
    Format { layout: Layout, visible: (u32, u32), depth: u32 },
    /// A picture, or (decoding order only) one that failed.
    Picture { cookie: u32, image: Option<Image> },
}

/// A guest buffer the VMM granted us.
#[derive(Debug, Clone, Copy)]
struct Grant {
    offset: u32,
    len: u32,
    format: PixFmt,
}

/// The guest's buffer window, mapped from the VMM's shared memory.
pub(crate) struct Window {
    base: *mut u8,
    len: usize,
}

impl Window {
    /// A window over our own memory (test modes).
    pub(crate) fn of(room: &mut [u8]) -> Window {
        Window { base: room.as_mut_ptr(), len: room.len() }
    }

    /// Where `len` bytes at `offset` are, if inside the window.
    pub(crate) fn range(&self, offset: u32, len: u32) -> Option<*mut u8> {
        let end = (offset as usize).checked_add(len as usize)?;
        // SAFETY: offset is inside our mapping (end <= len).
        #[allow(unsafe_code)]
        (end <= self.len).then(|| unsafe { self.base.add(offset as usize) })
    }

    /// Copy `data` into the grant at `offset` (`len` bytes), if it lies in
    /// the window and `data` fits. The bytes written, or None.
    pub(crate) fn put(&self, offset: u32, len: u32, data: &[u8]) -> Option<u32> {
        let dst = self.range(offset, len).filter(|_| data.len() <= len as usize)?;
        // SAFETY: dst..dst+len is inside our mapping (checked by range),
        // data.len() <= len; nothing in this process holds a reference
        // into the window.
        #[allow(unsafe_code)]
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len())
        };
        Some(data.len() as u32)
    }

    /// Write `image` into the grant, if it lies inside the window and is
    /// big enough. The picture's bytes, or None.
    fn write(&self, g: Grant, image: &Image, layout: Layout) -> Option<u32> {
        let need = layout.size(g.format);
        let end = (g.offset as usize).checked_add(g.len as usize)?;
        if end > self.len || g.len < need {
            return None;
        }
        // SAFETY: offset..offset+len is inside our mapping (checked above);
        // nothing in this process holds a reference into it.
        unsafe { image.write(self.base.add(g.offset as usize), g.len as usize, layout, g.format) }.then_some(need)
    }
}

/// Stream state VP9 and AV1 keep between buffers.
#[derive(Default)]
struct VpxState {
    /// The sizes in VP9's 8 reference slots.
    refs: [(u32, u32); 8],
    config: Option<Config>,
    /// AV1: the sequence header OBU the session was made with.
    sequence: Vec<u8>,
}

struct Stream {
    codec: Codec,
    /// Pictures go out as VideoToolbox decodes them (no reordering), and
    /// failed ones are reported.
    decode_order: bool,
    params: ParamSets,
    poc: Poc,
    vpx: VpxState,
    vt: Option<Vt>,
    /// The layout the VMM was last told about.
    told: Option<(Layout, (u32, u32), u32)>,
    inputs: VecDeque<(u32, Vec<u8>)>,
    /// Pictures sent to VideoToolbox: tag -> (cookie, order, reorder depth).
    decoding: BTreeMap<u64, (u32, i64, u32)>,
    /// Decoded pictures waiting for their turn in display order.
    reorder: Reorder<(u32, Option<Image>)>,
    out: VecDeque<Out>,
    epoch: u32,
    /// The epoch we last sent a new size under: grants still coming for
    /// it are buffers of the old size.
    closed: Option<u32>,
    grants: VecDeque<Grant>,
    draining: bool,
    tag: u64,
    dead: bool,
    /// What a new helper needs to open this stream again (snapshots), to
    /// send to the VMM; and what we last sent.
    resume: Option<Vec<u8>>,
    resume_sent: Vec<u8>,
    /// Samples sent to VideoToolbox, pictures back, pictures to the VMM
    /// (debug logs).
    stats: (u64, u64, u64),
}

impl Stream {
    fn new(codec: Codec, flags: u8) -> Self {
        Stream {
            codec,
            decode_order: flags & OPEN_DECODE_ORDER != 0,
            params: ParamSets::new(codec),
            poc: Poc::new(codec),
            vpx: VpxState::default(),
            vt: None,
            told: None,
            inputs: VecDeque::new(),
            decoding: BTreeMap::new(),
            reorder: Reorder::default(),
            out: VecDeque::new(),
            epoch: 0,
            closed: None,
            grants: VecDeque::new(),
            draining: false,
            tag: 0,
            dead: false,
            resume: None,
            resume_sent: Vec::new(),
            stats: (0, 0, 0),
        }
    }

    /// Note the stream's resume state if it changed.
    fn note_resume(&mut self, state: Vec<u8>) {
        if state.len() <= MAX_RESUME && state != self.resume_sent {
            self.resume = Some(state);
        }
    }

    /// Open the stream from a resume state (after a restore): its
    /// parameter sets, VP9 key frame header or AV1 sequence header. Guest
    /// data like any other: it goes through the same parsing.
    fn prime(&mut self, state: &[u8]) {
        match self.codec {
            Codec::H264 | Codec::Hevc => {
                let parsed = nal::parse(self.codec, state);
                for &(_, p, id, n) in parsed.params.iter().take(MAX_UNITS) {
                    self.param(p, id, n);
                }
                self.reformat();
            }
            Codec::Vp9 => {
                let mut refs = [(0, 0); 8];
                if let Some(c) = vpx::vp9_header(state, &mut refs).filter(|h| h.key).and_then(|h| h.config) {
                    self.vpx.refs = refs;
                    if self.start(Desc::Vp9(&c)) {
                        self.vpx.config = Some(c);
                    }
                }
            }
            Codec::Av1 => {
                for o in vpx::av1_obus(state) {
                    if o.kind != vpx::obu::SEQUENCE_HEADER || o.data.len() > vpx::MAX_SEQUENCE_HEADER {
                        continue;
                    }
                    if let Some(c) = vpx::av1_sequence(&o.data[o.payload..]) {
                        if self.start(Desc::Av1(&c, o.data)) {
                            self.vpx.config = Some(c);
                            self.vpx.sequence = o.data.to_vec();
                        }
                    }
                }
            }
        }
        self.resume_sent = state.to_vec();
    }

    fn pictures_held(&self) -> usize {
        self.out.iter().filter(|o| matches!(o, Out::Picture { .. })).count()
    }

    /// Move finished pictures from VideoToolbox (through the reorder
    /// buffer, in display order) to the output queue.
    fn collect(&mut self) {
        let Some(vt) = &mut self.vt else { return };
        for d in vt.take() {
            self.stats.1 += 1;
            let Some((cookie, poc, depth)) = self.decoding.remove(&d.tag) else { continue };
            if self.decode_order {
                self.out.push_back(Out::Picture { cookie, image: d.image });
            } else if d.image.is_some() {
                for (cookie, image) in self.reorder.push(poc, depth, (cookie, d.image)) {
                    self.out.push_back(Out::Picture { cookie, image });
                }
            }
        }
    }

    /// Hand out every picture still held back.
    fn flush_reorder(&mut self) {
        for (cookie, image) in self.reorder.flush() {
            self.out.push_back(Out::Picture { cookie, image });
        }
    }

    /// Hand out the pictures held back, keep the session.
    fn finish(&mut self) {
        if let Some(vt) = &mut self.vt {
            vt.finish();
        }
        self.collect();
        // Pictures VideoToolbox never answered for.
        if self.decode_order {
            for (_, (cookie, _, _)) in std::mem::take(&mut self.decoding) {
                self.out.push_back(Out::Picture { cookie, image: None });
            }
        }
        self.decoding.clear();
        self.flush_reorder();
    }

    /// Finish the current VideoToolbox session's pictures and end it.
    fn retire(&mut self) {
        self.finish();
        self.vt = None;
    }

    /// Drop everything decoding or decoded (a seek).
    fn drop_all(&mut self) {
        self.inputs.clear();
        if let Some(vt) = &mut self.vt {
            vt.finish();
            vt.take();
        }
        self.decoding.clear();
        self.reorder.clear();
        self.out.retain(|o| matches!(o, Out::Format { .. }));
        self.poc.restart();
    }

    /// A new VideoToolbox session for `desc`, telling the VMM if the size
    /// or depth changed. False: VideoToolbox won't decode it.
    fn start(&mut self, desc: Desc) -> bool {
        self.retire();
        let Some(vt) = Vt::new(desc) else { return false };
        let now = (vt.layout, vt.visible, vt.depth);
        if self.told != Some(now) {
            self.out.push_back(Out::Format { layout: now.0, visible: now.1, depth: now.2 });
            self.told = Some(now);
        }
        self.vt = Some(vt);
        true
    }

    /// Parameter sets changed: keep the session if it can go on with them,
    /// else start a new one.
    fn reformat(&mut self) -> bool {
        if !self.params.complete() {
            return true;
        }
        let sets: Vec<Vec<u8>> = self.params.list().into_iter().map(|s| s.to_vec()).collect();
        let sets: Vec<&[u8]> = sets.iter().map(|s| s.as_slice()).collect();
        let depth = self.params.depth();
        let mut state = Vec::new();
        for s in &sets {
            state.extend_from_slice(&[0, 0, 0, 1]);
            state.extend_from_slice(s);
        }
        if let Some(vt) = &mut self.vt {
            if vt.depth == depth && vt.accept(self.codec, &sets) == Some(true) {
                self.note_resume(state);
                return true;
            }
        }
        let ok = self.start(Desc::Nal(self.codec, &sets, depth));
        if ok {
            self.note_resume(state);
        }
        ok
    }

    fn param(&mut self, p: nal::Param, id: u32, n: &[u8]) -> bool {
        self.poc.param(p, n);
        self.params.update(p, id, n)
    }

    /// Send one sample to VideoToolbox. False: refused.
    fn submit(&mut self, cookie: u32, sample: &[u8], pic: Picture) -> bool {
        self.tag += 1;
        let tag = self.tag;
        if pic.reset && !self.decode_order {
            self.finish();
        }
        self.decoding.insert(tag, (cookie, pic.poc, pic.reorder));
        self.stats.0 += 1;
        let reorder = !self.decode_order;
        let ok = self.vt.as_mut().is_some_and(|vt| vt.decode(sample, tag, tag as i64, reorder));
        self.collect();
        if self.decode_order && !ok && self.decoding.remove(&tag).is_some() {
            self.out.push_back(Out::Picture { cookie, image: None });
        }
        // A picture VideoToolbox never answered for.
        self.decoding.retain(|&t, _| t + 64 > tag);
        ok
    }

    /// A picture we couldn't even send: in decoding order, say so.
    fn skipped(&mut self, cookie: u32) {
        if self.decode_order {
            self.out.push_back(Out::Picture { cookie, image: None });
        }
    }

    /// Decode one guest buffer. False: nothing in it decoded.
    fn decode(&mut self, cookie: u32, data: &[u8]) -> bool {
        match self.codec {
            Codec::H264 | Codec::Hevc => self.decode_nal(cookie, data),
            Codec::Vp9 => self.decode_vp9(cookie, data),
            Codec::Av1 => self.decode_av1(cookie, data),
        }
    }

    fn decode_nal(&mut self, cookie: u32, data: &[u8]) -> bool {
        let parsed = nal::parse(self.codec, data);
        let mut params = parsed.params.iter().take(MAX_UNITS).peekable();
        let mut ok = false;
        for (i, au) in parsed.units.iter().enumerate().take(MAX_UNITS) {
            let mut changed = false;
            while let Some(&&(at, p, id, n)) = params.peek() {
                if at > i {
                    break;
                }
                changed |= self.param(p, id, n);
                params.next();
            }
            if (changed || self.vt.is_none()) && !self.reformat() {
                self.skipped(cookie);
                return false;
            }
            if self.vt.is_none() {
                self.skipped(cookie);
                continue;
            }
            // Pictures we can't place go out in decoding order.
            let pic = if self.decode_order { None } else { self.poc.picture(au) };
            let pic = pic.unwrap_or(Picture { poc: i64::MAX, reset: false, reorder: 0 });
            ok |= self.submit(cookie, &nal::length_prefixed(au), pic);
        }
        // Parameter sets after the last picture (or alone).
        let mut changed = false;
        for &(_, p, id, n) in params {
            changed |= self.param(p, id, n);
        }
        if changed && !self.reformat() {
            return false;
        }
        ok || parsed.units.is_empty()
    }

    /// VP9: a frame or a superframe. Frames go to VideoToolbox as they
    /// came (a superframe whole: its hidden frames and the shown one).
    fn decode_vp9(&mut self, cookie: u32, data: &[u8]) -> bool {
        let mut config = None;
        let mut shows = false;
        let mut any = false;
        let mut key: &[u8] = &[];
        for f in vpx::vp9_frames(data) {
            let Some(h) = vpx::vp9_header(f, &mut self.vpx.refs) else { continue };
            any = true;
            shows |= h.show || h.show_existing.is_some();
            if h.key {
                config = h.config;
                key = f;
            }
        }
        if !any {
            self.skipped(cookie);
            return false;
        }
        if let Some(c) = config {
            if self.vt.is_none() || self.vpx.config.as_ref() != Some(&c) {
                let ok = self.start(Desc::Vp9(&c));
                self.vpx.config = Some(c);
                if !ok {
                    self.skipped(cookie);
                    return false;
                }
                // The key frame's header is enough to open the stream again.
                self.note_resume(key[..key.len().min(64)].to_vec());
            }
        }
        if self.vt.is_none() {
            // No key frame yet.
            self.skipped(cookie);
            return false;
        }
        let ok = self.submit(cookie, data, Picture { poc: i64::MAX, reset: false, reorder: 0 });
        if !shows && !self.decode_order {
            // A frame that isn't shown on its own (an alt-ref frame sent
            // apart from its superframe): VideoToolbox still hands it out
            // (at once: VP9 decodes synchronously), a player mustn't see it.
            // In decoding order it is what a VA-API player waits for.
            self.out.retain(|o| !matches!(o, Out::Picture { cookie: c, .. } if *c == cookie));
            self.decoding.retain(|_, d| d.0 != cookie);
        }
        ok
    }

    /// AV1: a temporal unit. VideoToolbox gets its OBUs without temporal
    /// delimiters and padding, and the sequence header up front.
    fn decode_av1(&mut self, cookie: u32, data: &[u8]) -> bool {
        let obus = vpx::av1_obus(data);
        let mut sample = Vec::with_capacity(data.len());
        for o in &obus {
            match o.kind {
                vpx::obu::SEQUENCE_HEADER => {
                    if o.data.len() > vpx::MAX_SEQUENCE_HEADER {
                        continue;
                    }
                    let Some(c) = vpx::av1_sequence(&o.data[o.payload..]) else { continue };
                    if self.vt.is_none() || self.vpx.config.as_ref() != Some(&c) || self.vpx.sequence != o.data {
                        self.finish();
                        let ok = self.start(Desc::Av1(&c, o.data));
                        self.vpx.config = Some(c);
                        self.vpx.sequence = o.data.to_vec();
                        if !ok {
                            self.skipped(cookie);
                            return false;
                        }
                        self.note_resume(o.data.to_vec());
                    }
                    sample.extend_from_slice(o.data);
                }
                vpx::obu::TEMPORAL_DELIMITER | vpx::obu::PADDING => {}
                _ => sample.extend_from_slice(o.data),
            }
        }
        if self.vt.is_none() || sample.is_empty() {
            self.skipped(cookie);
            return false;
        }
        self.submit(cookie, &sample, Picture { poc: i64::MAX, reset: false, reorder: 0 })
    }
}

/// Sends to the VMM; a broken socket ends the helper.
struct Link(UnixStream);

impl Link {
    fn send(&mut self, m: FromHelper) {
        if m.write_to(&mut self.0).is_err() {
            std::process::exit(0);
        }
    }
}

pub fn helper(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| a == "--encode") {
        return crate::venc::helper(args);
    }
    if let Some(i) = args.iter().position(|a| a == "--file") {
        let file = args.get(i + 1).ok_or("--file needs a path")?;
        let codec = match args.iter().position(|a| a == "--codec").and_then(|i| args.get(i + 1)).map(|s| s.as_str()) {
            Some("hevc") | Some("h265") => Codec::Hevc,
            Some("vp9") => Codec::Vp9,
            Some("av1") => Codec::Av1,
            _ => Codec::H264,
        };
        let out = args.iter().position(|a| a == "--out").and_then(|i| args.get(i + 1));
        let flags = if args.iter().any(|a| a == "--decode-order") { OPEN_DECODE_ORDER } else { 0 };
        return decode_file(file, codec, flags, args.iter().any(|a| a == "--split"), out.map(|s| s.as_str()));
    }
    // SAFETY: fd 0 is the socket the VMM set up for us; nothing else here
    // uses stdin.
    #[allow(unsafe_code)]
    let sock = unsafe { UnixStream::from_raw_fd(0) };
    let window = map_window(&sock)?;
    let mut rx = sock.try_clone().map_err(|e| e.to_string())?;
    let mut link = Link(sock);
    link.send(FromHelper::Caps { codecs: caps() });
    // Messages are read on a thread of their own, so a VMM that keeps
    // sending never blocks on us while we decode (and the other way).
    let (tx, msgs) = mpsc::sync_channel::<ToHelper>(256);
    std::thread::Builder::new()
        .name("video-rx".into())
        .spawn(move || {
            while let Ok(m) = ToHelper::read_from(&mut rx) {
                if tx.send(m).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let mut streams: BTreeMap<u32, Stream> = BTreeMap::new();
    loop {
        let busy = streams.values().any(|s| {
            !s.dead
                && ((!s.inputs.is_empty() && s.pictures_held() < MAX_HELD)
                    || s.draining
                    || matches!(s.out.front(), Some(Out::Format { .. }))
                    || (!s.grants.is_empty() && !s.out.is_empty()))
        });
        let first = if busy {
            match msgs.try_recv() {
                Ok(m) => Some(m),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        } else {
            if log::log_enabled!(log::Level::Debug) {
                for (id, s) in &streams {
                    log::debug!(
                        "video: session {id} waits: {} inputs, {} out ({} pictures), {} grants, epoch {}, decoding {}, reorder {}, samples/pictures/sent {:?}",
                        s.inputs.len(),
                        s.out.len(),
                        s.pictures_held(),
                        s.grants.len(),
                        s.epoch,
                        s.decoding.len(),
                        s.reorder.len(),
                        s.stats
                    );
                }
            }
            match msgs.recv() {
                Ok(m) => Some(m),
                Err(_) => return Ok(()),
            }
        };
        for m in first.into_iter().chain(msgs.try_iter()) {
            handle(&mut streams, &mut link, m);
        }
        for (&id, s) in streams.iter_mut() {
            work(id, s, &mut link, &window);
        }
    }
}

#[cfg(target_os = "macos")]
fn caps() -> u32 {
    crate::vt::caps()
}
#[cfg(not(target_os = "macos"))]
fn caps() -> u32 {
    novt::caps()
}

/// The buffer window: the first thing on the socket is its shared memory
/// object. Mapped once, for the helper's life.
pub(crate) fn map_window(sock: &UnixStream) -> Result<Window, String> {
    use std::os::fd::AsRawFd;
    let fd = vroom_ipc::recv_fd(sock).map_err(|e| format!("video window: {e}"))?;
    #[allow(unsafe_code)]
    // SAFETY: fstat on our fd into a zeroed stat.
    let len = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) != 0 {
            return Err("video window: fstat".into());
        }
        st.st_size as usize
    };
    if len == 0 || len > 4 << 30 {
        return Err("video window: size".into());
    }
    #[allow(unsafe_code)]
    // SAFETY: a fresh shared mapping of the whole object; kept for the
    // life of the process.
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if p == libc::MAP_FAILED {
        return Err(format!("video window: mmap: {}", std::io::Error::last_os_error()));
    }
    Ok(Window { base: p as *mut u8, len })
}

fn handle(streams: &mut BTreeMap<u32, Stream>, link: &mut Link, m: ToHelper) {
    match m {
        ToHelper::Open { session, codec, flags } => {
            streams.insert(session, Stream::new(codec, flags));
        }
        ToHelper::Decode { session, cookie, data } => match streams.get_mut(&session) {
            // The VMM sends no more than the guest's buffers; cap anyway.
            Some(s) if !s.dead && s.inputs.len() < 64 => s.inputs.push_back((cookie, data)),
            _ => link.send(FromHelper::Consumed { session, cookie, ok: false }),
        },
        ToHelper::Grant { session, epoch, offset, len, format } => {
            if let Some(s) = streams.get_mut(&session) {
                if s.closed == Some(epoch) {
                    return;
                }
                if epoch != s.epoch {
                    s.epoch = epoch;
                    s.grants.clear();
                }
                if len > 0 && s.grants.len() < MAX_GRANTS {
                    s.grants.push_back(Grant { offset, len, format });
                }
            }
        }
        ToHelper::Drain { session } => {
            if let Some(s) = streams.get_mut(&session) {
                s.draining = true;
            }
        }
        ToHelper::Flush { session } => {
            if let Some(s) = streams.get_mut(&session) {
                s.drop_all();
                s.grants.clear();
                s.draining = false;
            }
        }
        ToHelper::Close { session } => {
            streams.remove(&session);
        }
        ToHelper::Resume { session, codec, flags, state } => {
            let mut s = Stream::new(codec, flags);
            s.prime(&state);
            streams.insert(session, s);
        }
        // An encoder's messages: not for us.
        ToHelper::Configure { .. } | ToHelper::Encode { .. } => {}
    }
}

/// Decode what we may, send what we can.
fn work(session: u32, s: &mut Stream, link: &mut Link, window: &Window) {
    if s.dead {
        return;
    }
    if let Some(state) = s.resume.take() {
        link.send(FromHelper::Resume { session, state: state.clone() });
        s.resume_sent = state;
    }
    while s.pictures_held() < MAX_HELD {
        let Some((cookie, data)) = s.inputs.pop_front() else { break };
        let ok = s.decode(cookie, &data);
        link.send(FromHelper::Consumed { session, cookie, ok });
        let known = match s.codec {
            Codec::H264 | Codec::Hevc => s.params.complete(),
            Codec::Vp9 | Codec::Av1 => s.vpx.config.is_some(),
        };
        if !ok && s.vt.is_none() && known {
            // VideoToolbox won't take this stream at all.
            s.dead = true;
            s.inputs.clear();
            link.send(FromHelper::Error { session, errno: 22 });
            return;
        }
    }
    if s.draining && s.inputs.is_empty() {
        s.finish();
    }
    while let Some(o) = s.out.front() {
        match o {
            Out::Format { layout, visible, depth } => {
                link.send(FromHelper::Format { session, layout: *layout, visible: *visible, min_buffers: MIN_BUFFERS, depth: *depth });
                // New buffers for the new size: the VMM grants again, under
                // a new epoch.
                s.grants.clear();
                s.closed = Some(s.epoch);
                s.out.pop_front();
            }
            Out::Picture { .. } => {
                let Some(g) = s.grants.pop_front() else { break };
                let Some(Out::Picture { cookie, image }) = s.out.pop_front() else { break };
                let layout = s.told.map(|t| t.0);
                let bytes = match (&image, layout) {
                    (Some(i), Some(l)) => window.write(g, i, l),
                    _ => None,
                };
                s.stats.2 += 1;
                link.send(FromHelper::Picture { session, epoch: s.epoch, cookie, offset: g.offset, bytes: bytes.unwrap_or(0), ok: bytes.is_some() });
            }
        }
    }
    if s.draining && s.inputs.is_empty() && s.out.is_empty() {
        s.draining = false;
        link.send(FromHelper::DrainDone { session });
    }
}

/// Test mode: the buffers of a file. H.264 / HEVC: one access unit (with
/// the parameter sets before it) each. VP9 / AV1: IVF frames.
fn file_buffers(codec: Codec, data: &[u8], split: bool) -> Result<Vec<Vec<u8>>, String> {
    if matches!(codec, Codec::Vp9 | Codec::Av1) {
        if data.len() < 32 || &data[..4] != b"DKIF" {
            return Err("not an IVF file".into());
        }
        let hdr = u16::from_le_bytes([data[6], data[7]]) as usize;
        let mut at = hdr;
        let mut v = Vec::new();
        while at + 12 <= data.len() {
            let n = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
            at += 12;
            let end = at.checked_add(n).filter(|&e| e <= data.len()).ok_or("IVF frame past the end")?;
            // --split: VP9 superframes as their frames, as a VA-API
            // player sends them.
            if codec == Codec::Vp9 && split {
                v.extend(vpx::vp9_frames(&data[at..end]).into_iter().map(|f| f.to_vec()));
            } else {
                v.push(data[at..end].to_vec());
            }
            at = end;
        }
        return Ok(v);
    }
    let parsed = nal::parse(codec, data);
    let mut v = Vec::new();
    let mut pi = 0;
    for (i, au) in parsed.units.iter().enumerate() {
        let mut b = Vec::new();
        while pi < parsed.params.len() && parsed.params[pi].0 <= i {
            b.extend_from_slice(&[0, 0, 0, 1]);
            b.extend_from_slice(parsed.params[pi].3);
            pi += 1;
        }
        for n in &au.nals {
            b.extend_from_slice(&[0, 0, 0, 1]);
            b.extend_from_slice(n);
        }
        v.push(b);
    }
    Ok(v)
}

/// Test mode: decode a file, write the visible part of each picture.
fn decode_file(path: &str, codec: Codec, flags: u8, split: bool, out: Option<&str>) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let mut f = match out {
        Some(o) => Some(std::fs::File::create(o).map_err(|e| format!("{o}: {e}"))?),
        None => None,
    };
    let units = file_buffers(codec, &data, split)?;
    let mut s = Stream::new(codec, flags);
    // One picture's room, granted again for each picture.
    let mut room = vec![0u8; vroom_video::MAX_PICTURE as usize];
    let t0 = std::time::Instant::now();
    let (mut pictures, mut failed) = (0u64, 0u64);
    let mut write = |s: &mut Stream| -> Result<(), String> {
        while let Some(o) = s.out.pop_front() {
            match o {
                Out::Format { layout, visible, depth } => eprintln!("format {}x{} in {}x{}, {depth} bits", visible.0, visible.1, layout.width, layout.height),
                Out::Picture { image: None, .. } => failed += 1,
                Out::Picture { image: Some(image), .. } => {
                    pictures += 1;
                    let Some((l, v, depth)) = s.told else { continue };
                    let format = PixFmt::for_depth(depth);
                    let window = Window { base: room.as_mut_ptr(), len: room.len() };
                    if window.write(Grant { offset: 0, len: room.len() as u32, format }, &image, l).is_none() {
                        failed += 1;
                        continue;
                    }
                    if let Some(f) = &mut f {
                        // Visible part only.
                        let b = format.bytes() as usize;
                        let (w, h) = (v.0 as usize * b, v.1 as usize);
                        let stride = l.width as usize * b;
                        for y in 0..h {
                            f.write_all(&room[y * stride..y * stride + w]).map_err(|e| e.to_string())?;
                        }
                        let c = stride * l.height as usize;
                        for y in 0..h.div_ceil(2) {
                            f.write_all(&room[c + y * stride..c + y * stride + w]).map_err(|e| e.to_string())?;
                        }
                    }
                }
            }
        }
        Ok(())
    };
    for (i, u) in units.iter().enumerate() {
        if !s.decode(i as u32, u) {
            eprintln!("buffer {i}: not decoded");
        }
        write(&mut s)?;
    }
    s.retire();
    write(&mut s)?;
    let dt = t0.elapsed().as_secs_f64();
    eprintln!("{} buffers, {pictures} pictures ({failed} failed) in {dt:.2} s: {:.0} fps", units.len(), pictures as f64 / dt);
    Ok(())
}
