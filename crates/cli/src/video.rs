//! `vroom video-helper`: decodes the guest's H.264 / HEVC streams with
//! VideoToolbox for the VMM's virtio-media decoder.
//!
//! Started by the VMM in a sandbox of its own (VideoToolbox and the
//! services it uses, nothing else); stdin is a socket to the VMM carrying
//! `vroom_video::proto` messages. Everything the VMM sends is guest data:
//! streams go through `vroom_video::nal` first, sizes and counts are
//! capped, and a malformed message ends the helper.
//!
//! `vroom video-helper --file IN --codec h264|hevc [--out OUT.nv12]`
//! decodes a file the same way, for tests (frames per second, and the
//! pictures to compare with another decoder).

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use vroom_video::nal::{self, ParamSets};
use vroom_video::poc::{Picture, Poc, Reorder};
use vroom_video::proto::{FromHelper, ToHelper, MAX_HELD};
use vroom_video::{Codec, Layout};

#[cfg(target_os = "macos")]
use crate::vt::Session as Vt;

#[cfg(not(target_os = "macos"))]
mod novt {
    //! No VideoToolbox: every stream is refused.
    use vroom_video::{Codec, Layout};

    pub struct Decoded {
        pub tag: u64,
        pub data: Option<Vec<u8>>,
    }

    pub struct Vt {
        pub layout: Layout,
        pub visible: (u32, u32),
    }

    impl Vt {
        pub fn new(_: Codec, _: &[&[u8]]) -> Option<Vt> {
            None
        }
        pub fn accepts(&self, _: Codec, _: &[&[u8]]) -> Option<bool> {
            None
        }
        pub fn decode(&mut self, _: &[u8], _: u64, _: i64) -> bool {
            false
        }
        pub fn finish(&mut self) {}
        pub fn take(&mut self) -> Vec<Decoded> {
            Vec::new()
        }
    }
}
#[cfg(not(target_os = "macos"))]
use novt::Vt;

/// Pictures the VMM is asked to keep in its buffers beyond what the
/// stream needs: VideoToolbox keeps its own references, so a few do.
const MIN_BUFFERS: u32 = 4;
/// Most parameter set updates or access units we act on per buffer.
const MAX_UNITS: usize = 256;

/// What goes back to the VMM, in order.
enum Out {
    Format { layout: Layout, visible: (u32, u32) },
    Picture { cookie: u32, data: Vec<u8> },
}

struct Stream {
    codec: Codec,
    params: ParamSets,
    poc: Poc,
    vt: Option<Vt>,
    /// The layout the VMM was last told about.
    told: Option<(Layout, (u32, u32))>,
    inputs: VecDeque<(u32, Vec<u8>)>,
    /// Pictures sent to VideoToolbox: tag -> (cookie, order, reorder depth).
    decoding: BTreeMap<u64, (u32, i64, u32)>,
    /// Decoded pictures waiting for their turn in display order.
    reorder: Reorder<(u32, Vec<u8>)>,
    out: VecDeque<Out>,
    epoch: u32,
    /// The epoch we last sent a new size under: credits still coming for
    /// it were granted for buffers of the old size.
    closed: Option<u32>,
    credits: u32,
    draining: bool,
    tag: u64,
    dead: bool,
}

impl Stream {
    fn new(codec: Codec) -> Self {
        Stream {
            codec,
            params: ParamSets::new(codec),
            poc: Poc::new(codec),
            vt: None,
            told: None,
            inputs: VecDeque::new(),
            decoding: BTreeMap::new(),
            reorder: Reorder::default(),
            out: VecDeque::new(),
            epoch: 0,
            closed: None,
            credits: 0,
            draining: false,
            tag: 0,
            dead: false,
        }
    }

    fn pictures_held(&self) -> usize {
        self.out.iter().filter(|o| matches!(o, Out::Picture { .. })).count()
    }

    /// Move finished pictures from VideoToolbox through the reorder
    /// buffer to the output queue.
    fn collect(&mut self) {
        let Some(vt) = &mut self.vt else { return };
        for d in vt.take() {
            let Some((cookie, poc, depth)) = self.decoding.remove(&d.tag) else { continue };
            if let Some(data) = d.data {
                for (cookie, data) in self.reorder.push(poc, depth, (cookie, data)) {
                    self.out.push_back(Out::Picture { cookie, data });
                }
            }
        }
    }

    /// Hand out every picture still held back.
    fn flush_reorder(&mut self) {
        for (cookie, data) in self.reorder.flush() {
            self.out.push_back(Out::Picture { cookie, data });
        }
    }

    /// Hand out the pictures held back, keep the session.
    fn finish(&mut self) {
        if let Some(vt) = &mut self.vt {
            vt.finish();
        }
        self.collect();
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

    /// Parameter sets changed: keep the session if it can go on with them,
    /// else start a new one (telling the VMM if the size changed).
    fn reformat(&mut self) -> bool {
        if !self.params.complete() {
            return true;
        }
        let sets: Vec<Vec<u8>> = self.params.list().into_iter().map(|s| s.to_vec()).collect();
        let sets: Vec<&[u8]> = sets.iter().map(|s| s.as_slice()).collect();
        if let Some(vt) = &self.vt {
            if vt.accepts(self.codec, &sets) == Some(true) {
                return true;
            }
        }
        self.retire();
        let Some(vt) = Vt::new(self.codec, &sets) else { return false };
        let now = (vt.layout, vt.visible);
        if self.told != Some(now) {
            self.out.push_back(Out::Format { layout: now.0, visible: now.1 });
            self.told = Some(now);
        }
        self.vt = Some(vt);
        true
    }

    fn param(&mut self, p: nal::Param, id: u32, n: &[u8]) -> bool {
        self.poc.param(p, n);
        self.params.update(p, id, n)
    }

    /// Decode one guest buffer. False: nothing in it decoded.
    fn decode(&mut self, cookie: u32, data: &[u8]) -> bool {
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
                return false;
            }
            if self.vt.is_none() {
                continue;
            }
            self.tag += 1;
            // Pictures we can't place go out in decoding order.
            let pic = self.poc.picture(au).unwrap_or(Picture { poc: i64::MAX, reset: false, reorder: 0 });
            if pic.reset {
                self.finish();
            }
            self.decoding.insert(self.tag, (cookie, pic.poc, pic.reorder));
            let tag = self.tag;
            if let Some(vt) = &mut self.vt {
                ok |= vt.decode(&nal::length_prefixed(au), tag, tag as i64);
            }
            self.collect();
            // A picture VideoToolbox never answered for.
            self.decoding.retain(|&t, _| t + 64 > tag);
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
    if let Some(i) = args.iter().position(|a| a == "--file") {
        let file = args.get(i + 1).ok_or("--file needs a path")?;
        let codec = match args.iter().position(|a| a == "--codec").and_then(|i| args.get(i + 1)).map(|s| s.as_str()) {
            Some("hevc") | Some("h265") => Codec::Hevc,
            _ => Codec::H264,
        };
        let out = args.iter().position(|a| a == "--out").and_then(|i| args.get(i + 1));
        return decode_file(file, codec, out.map(|s| s.as_str()));
    }
    // SAFETY: fd 0 is the socket the VMM set up for us; nothing else here
    // uses stdin.
    #[allow(unsafe_code)]
    let sock = unsafe { UnixStream::from_raw_fd(0) };
    let mut rx = sock.try_clone().map_err(|e| e.to_string())?;
    let mut link = Link(sock);
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
            !s.dead && ((!s.inputs.is_empty() && s.pictures_held() < MAX_HELD) || s.draining || matches!(s.out.front(), Some(Out::Format { .. })) || (s.credits > 0 && !s.out.is_empty()))
        });
        let first = if busy {
            match msgs.try_recv() {
                Ok(m) => Some(m),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        } else {
            match msgs.recv() {
                Ok(m) => Some(m),
                Err(_) => return Ok(()),
            }
        };
        for m in first.into_iter().chain(msgs.try_iter()) {
            handle(&mut streams, &mut link, m);
        }
        for (&id, s) in streams.iter_mut() {
            work(id, s, &mut link);
        }
    }
}

fn handle(streams: &mut BTreeMap<u32, Stream>, link: &mut Link, m: ToHelper) {
    match m {
        ToHelper::Open { session, codec } => {
            streams.insert(session, Stream::new(codec));
        }
        ToHelper::Decode { session, cookie, data } => match streams.get_mut(&session) {
            // The VMM sends no more than the guest's buffers; cap anyway.
            Some(s) if !s.dead && s.inputs.len() < 64 => s.inputs.push_back((cookie, data)),
            _ => link.send(FromHelper::Consumed { session, cookie, ok: false }),
        },
        ToHelper::Credit { session, epoch, n } => {
            if let Some(s) = streams.get_mut(&session) {
                if s.closed == Some(epoch) {
                    return;
                }
                if epoch != s.epoch {
                    s.epoch = epoch;
                    s.credits = 0;
                }
                s.credits = s.credits.saturating_add(n).min(64);
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
                s.credits = 0;
                s.draining = false;
            }
        }
        ToHelper::Close { session } => {
            streams.remove(&session);
        }
    }
}

/// Decode what we may, send what we can.
fn work(session: u32, s: &mut Stream, link: &mut Link) {
    if s.dead {
        return;
    }
    while s.pictures_held() < MAX_HELD {
        let Some((cookie, data)) = s.inputs.pop_front() else { break };
        let ok = s.decode(cookie, &data);
        link.send(FromHelper::Consumed { session, cookie, ok });
        if !ok && s.vt.is_none() && s.params.complete() {
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
            Out::Format { layout, visible } => {
                link.send(FromHelper::Format { session, layout: *layout, visible: *visible, min_buffers: MIN_BUFFERS });
                // New buffers for the new size: the VMM grants again, under
                // a new epoch.
                s.credits = 0;
                s.closed = Some(s.epoch);
            }
            Out::Picture { .. } if s.credits == 0 => break,
            Out::Picture { .. } => {}
        }
        if let Some(Out::Picture { cookie, data }) = s.out.pop_front() {
            s.credits -= 1;
            link.send(FromHelper::Picture { session, epoch: s.epoch, cookie, data });
        }
    }
    if s.draining && s.inputs.is_empty() && s.out.is_empty() {
        s.draining = false;
        link.send(FromHelper::DrainDone { session });
    }
}

/// Test mode: decode a file in buffers of one access unit each.
fn decode_file(path: &str, codec: Codec, out: Option<&str>) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let mut f = match out {
        Some(o) => Some(std::fs::File::create(o).map_err(|e| format!("{o}: {e}"))?),
        None => None,
    };
    let mut s = Stream::new(codec);
    let t0 = std::time::Instant::now();
    let mut pictures = 0u64;
    let mut write = |s: &mut Stream| -> Result<(), String> {
        while let Some(o) = s.out.pop_front() {
            match o {
                Out::Format { layout, visible } => eprintln!("format {}x{} in {}x{}", visible.0, visible.1, layout.width, layout.height),
                Out::Picture { data, .. } => {
                    pictures += 1;
                    if let (Some(f), Some((l, v))) = (&mut f, s.told) {
                        // Visible part only, NV12.
                        let (w, h) = (v.0 as usize, v.1 as usize);
                        let lw = l.width as usize;
                        for y in 0..h {
                            f.write_all(&data[y * lw..y * lw + w]).map_err(|e| e.to_string())?;
                        }
                        let c = lw * l.height as usize;
                        for y in 0..h / 2 {
                            f.write_all(&data[c + y * lw..c + y * lw + w]).map_err(|e| e.to_string())?;
                        }
                    }
                }
            }
        }
        Ok(())
    };
    // One access unit (with the parameter sets before it) per buffer.
    let parsed = nal::parse(codec, &data);
    let units: Vec<Vec<u8>> = {
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
        v
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
    eprintln!("{} buffers, {pictures} pictures in {dt:.2} s: {:.0} fps", units.len(), pictures as f64 / dt);
    Ok(())
}
