//! virtio-snd: one playback stream (S16, 1-2 channels, 44.1 or 48 kHz).
//! There is no capture stream: the guest can't ask for the microphone.
//!
//! Buffers are completed at playback speed (a clock started on PCM_START),
//! which paces the guest; the PCM itself goes to an [`AudioSink`] as soon
//! as it arrives (the sink buffers a little).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::chain::Writer;
use crate::worker::{QueueCtx, QueueHandler, WorkerSlot};
use crate::{types, ActivateContext, DeviceState, Error, Result, VirtioDevice};

/// Where guest audio goes. Must not block: drop data rather than stall.
pub trait AudioSink: Send {
    fn start(&mut self, rate: u32, channels: u8);
    /// Interleaved signed 16-bit little-endian frames.
    fn write(&mut self, pcm: &[u8]);
    fn stop(&mut self);
}

const Q_CTL: usize = 0;
const Q_TX: usize = 2;
const Q_RX: usize = 3;

const R_JACK_INFO: u32 = 1;
const R_PCM_INFO: u32 = 0x100;
const R_PCM_SET_PARAMS: u32 = 0x101;
const R_PCM_PREPARE: u32 = 0x102;
const R_PCM_RELEASE: u32 = 0x103;
const R_PCM_START: u32 = 0x104;
const R_PCM_STOP: u32 = 0x105;
const R_CHMAP_INFO: u32 = 0x200;

const S_OK: u32 = 0x8000;
const S_BAD_MSG: u32 = 0x8001;
const S_NOT_SUPP: u32 = 0x8002;
const S_IO_ERR: u32 = 0x8003;

const FMT_S16: u8 = 5;
const RATE_44100: u8 = 6;
const RATE_48000: u8 = 7;
const DIR_OUTPUT: u8 = 0;

/// Most bytes one transfer may carry.
const MAX_XFER: usize = 1 << 20;
const MAX_PENDING: usize = 64;
const TICK: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Params {
    buffer_bytes: u32,
    period_bytes: u32,
    channels: u8,
    rate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum State {
    #[default]
    Idle,
    Prepared,
    Running,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Snap {
    params: Option<Params>,
    state: State,
}

struct Pending {
    head: u16,
    status: Writer,
    /// Stream position (bytes) once this buffer has played.
    end: u64,
}

pub struct SoundHandler {
    sink: Box<dyn AudioSink>,
    params: Option<Params>,
    state: State,
    pending: VecDeque<Pending>,
    queued: u64,
    started: Option<Instant>,
    /// PCM queued before START (drivers pre-fill): sent on START.
    prefill: Vec<u8>,
}

impl std::fmt::Debug for SoundHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundHandler").field("state", &self.state).field("pending", &self.pending.len()).finish()
    }
}

fn rd32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

impl SoundHandler {
    fn new(sink: Box<dyn AudioSink>, snap: Snap) -> Self {
        // Running streams come back prepared: the guest restarts them.
        let state = if snap.state == State::Running { State::Prepared } else { snap.state };
        SoundHandler { sink, params: snap.params, state, pending: VecDeque::new(), queued: 0, started: None, prefill: Vec::new() }
    }

    fn frame_bytes(&self) -> u64 {
        self.params.map_or(4, |p| 2 * p.channels as u64)
    }

    /// Bytes the host has played since START (plus two periods of lead,
    /// so the sink never runs dry).
    fn played(&self) -> u64 {
        let (Some(t0), Some(p)) = (self.started, self.params) else { return 0 };
        let us = t0.elapsed().as_micros() as u64;
        us * p.rate as u64 / 1_000_000 * self.frame_bytes() + 2 * p.period_bytes as u64
    }

    fn complete(&mut self, ctx: &mut QueueCtx, all: bool) {
        let played = self.played();
        while let Some(p) = self.pending.front() {
            if !all && p.end > played {
                break;
            }
            let mut p = self.pending.pop_front().expect("front exists");
            let mut st = [0u8; 8];
            st[..4].copy_from_slice(&S_OK.to_le_bytes());
            let len = if p.status.write_all(&st).is_ok() { 8 } else { 0 };
            ctx.add_used(Q_TX, p.head, len);
        }
    }

    fn control(&mut self, ctx: &mut QueueCtx, req: &[u8], w: &mut Writer) -> u32 {
        let Some(code) = rd32(req, 0) else { return S_BAD_MSG };
        match code {
            R_PCM_INFO => {
                let (Some(start), Some(count), Some(size)) = (rd32(req, 4), rd32(req, 8), rd32(req, 12)) else { return S_BAD_MSG };
                if start != 0 || count != 1 || size < 32 {
                    return S_BAD_MSG;
                }
                let mut info = vec![0u8; size.min(64) as usize];
                // features 0; formats: S16; rates: 44.1k, 48k.
                info[8..16].copy_from_slice(&(1u64 << FMT_S16).to_le_bytes());
                info[16..24].copy_from_slice(&((1u64 << RATE_44100) | (1u64 << RATE_48000)).to_le_bytes());
                info[24] = DIR_OUTPUT;
                info[25] = 1;
                info[26] = 2;
                let mut out = S_OK.to_le_bytes().to_vec();
                out.extend_from_slice(&info);
                if w.write_all(&out).is_err() {
                    return S_IO_ERR;
                }
                return u32::MAX; // written already
            }
            R_JACK_INFO | R_CHMAP_INFO => return S_NOT_SUPP,
            _ => {}
        }
        // PCM requests carry the stream id.
        if rd32(req, 4) != Some(0) {
            return S_BAD_MSG;
        }
        match code {
            R_PCM_SET_PARAMS => {
                let (Some(buffer_bytes), Some(period_bytes)) = (rd32(req, 8), rd32(req, 12)) else { return S_BAD_MSG };
                let (channels, format, rate) = (req.get(20).copied(), req.get(21).copied(), req.get(22).copied());
                let rate = match rate {
                    Some(RATE_44100) => 44100,
                    Some(RATE_48000) => 48000,
                    _ => return S_NOT_SUPP,
                };
                let Some(channels @ 1..=2) = channels else { return S_NOT_SUPP };
                if format != Some(FMT_S16) {
                    return S_NOT_SUPP;
                }
                if period_bytes == 0 || buffer_bytes < period_bytes || buffer_bytes as usize > 4 * MAX_XFER {
                    return S_BAD_MSG;
                }
                if self.state == State::Running {
                    return S_BAD_MSG;
                }
                self.params = Some(Params { buffer_bytes, period_bytes, channels, rate });
                S_OK
            }
            R_PCM_PREPARE => {
                if self.params.is_none() || self.state == State::Running {
                    return S_BAD_MSG;
                }
                self.state = State::Prepared;
                S_OK
            }
            R_PCM_START => {
                let Some(p) = self.params else { return S_BAD_MSG };
                if self.state != State::Prepared {
                    return S_BAD_MSG;
                }
                self.state = State::Running;
                // Play time counts from START; buffers queued before it
                // (the usual pre-fill) play first.
                self.started = Some(Instant::now());
                self.sink.start(p.rate, p.channels);
                let pre = std::mem::take(&mut self.prefill);
                if !pre.is_empty() {
                    self.sink.write(&pre);
                }
                S_OK
            }
            R_PCM_STOP => {
                if self.state != State::Running {
                    return S_BAD_MSG;
                }
                self.state = State::Prepared;
                self.started = None;
                self.sink.stop();
                self.prefill.clear();
                self.complete(ctx, true);
                self.queued = 0;
                S_OK
            }
            R_PCM_RELEASE => {
                if self.state == State::Running {
                    self.sink.stop();
                }
                self.state = State::Idle;
                self.started = None;
                self.prefill.clear();
                self.complete(ctx, true);
                self.queued = 0;
                S_OK
            }
            _ => S_NOT_SUPP,
        }
    }

    fn tx(&mut self, ctx: &mut QueueCtx) {
        loop {
            if self.pending.len() >= MAX_PENDING {
                // The guest outran us badly: make room (audio glitch).
                self.complete(ctx, true);
            }
            let chain = match ctx.queues[Q_TX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let head = chain.head();
            let Ok((mut r, mut w)) = chain.into_rw() else {
                ctx.add_used(Q_TX, head, 0);
                continue;
            };
            let ok_stream = r.read_obj::<u32>().is_ok_and(|v| u32::from_le(v) == 0);
            let n = r.available().min(MAX_XFER);
            let mut pcm = vec![0u8; n];
            let ok = ok_stream && self.state != State::Idle && r.read_exact(&mut pcm).is_ok() && w.available() >= 8;
            if !ok {
                let mut st = [0u8; 8];
                st[..4].copy_from_slice(&S_BAD_MSG.to_le_bytes());
                let _ = w.write_all(&st);
                ctx.add_used(Q_TX, head, w.written() as u32);
                continue;
            }
            // Whole frames only.
            let fb = self.frame_bytes() as usize;
            pcm.truncate(pcm.len() / fb * fb);
            if self.state == State::Running {
                self.sink.write(&pcm);
            } else if self.prefill.len() + pcm.len() <= self.params.map_or(0, |p| p.buffer_bytes as usize) {
                self.prefill.extend_from_slice(&pcm);
            }
            self.queued += pcm.len() as u64;
            // Keep only the 8-byte status part for later.
            let status = match w.split_tail(8) {
                Ok(s) => s,
                Err(_) => w,
            };
            self.pending.push_back(Pending { head, status, end: self.queued });
        }
        if self.state == State::Running {
            self.complete(ctx, false);
        }
    }
}

impl QueueHandler for SoundHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        match q {
            Q_CTL => loop {
                let chain = match ctx.queues[Q_CTL].pop(&ctx.mem) {
                    Ok(Some(c)) => c,
                    _ => break,
                };
                let head = chain.head();
                let mut len = 0;
                if let Ok((mut r, mut w)) = chain.into_rw() {
                    let n = r.available().min(256);
                    let mut req = vec![0u8; n];
                    let st = if r.read_exact(&mut req).is_ok() { self.control(ctx, &req, &mut w) } else { S_BAD_MSG };
                    if st != u32::MAX {
                        let _ = w.write_all(&st.to_le_bytes());
                    }
                    len = w.written() as u32;
                }
                ctx.add_used(Q_CTL, head, len);
            },
            Q_TX => self.tx(ctx),
            Q_RX => {
                // No capture: hand back anything offered.
                while let Ok(Some(c)) = ctx.queues[Q_RX].pop(&ctx.mem) {
                    ctx.add_used(Q_RX, c.head(), 0);
                }
            }
            // Event queue: we never have events; keep the buffers.
            _ => {}
        }
    }

    fn tick_interval(&mut self) -> Option<Duration> {
        (self.state == State::Running && !self.pending.is_empty()).then_some(TICK)
    }

    fn tick(&mut self, ctx: &mut QueueCtx) {
        self.complete(ctx, false);
    }
}

pub struct Sound {
    make_sink: Box<dyn FnMut() -> Box<dyn AudioSink> + Send>,
    worker: WorkerSlot<SoundHandler>,
}

impl std::fmt::Debug for Sound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sound").finish_non_exhaustive()
    }
}

impl Sound {
    /// `make_sink` is called on every activation (a guest reboot gets a
    /// fresh one).
    pub fn new(make_sink: Box<dyn FnMut() -> Box<dyn AudioSink> + Send>) -> Self {
        Sound { make_sink, worker: WorkerSlot::default() }
    }

    /// Fuzzing: run every queue once, synchronously, then a tick and a
    /// release. Not for anything else.
    #[doc(hidden)]
    pub fn fuzz_queues(&mut self, ctx: ActivateContext) {
        let mut h = SoundHandler::new((self.make_sink)(), Snap::default());
        let mut c = crate::worker::QueueCtx::new(ctx);
        for _ in 0..2 {
            for q in 0..4 {
                h.process_queue(&mut c, q);
            }
            h.tick(&mut c);
        }
        h.complete(&mut c, true);
    }
}

impl VirtioDevice for Sound {
    fn device_type(&self) -> u32 {
        types::SOUND
    }
    fn name(&self) -> String {
        "sound".into()
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64, 64, 64, 64]
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // jacks 0, streams 1, chmaps 0.
        let c: [u8; 12] = [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        data.fill(0);
        let o = offset as usize;
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        let h = SoundHandler::new((self.make_sink)(), Snap::default());
        self.worker.start("sound", ctx, h, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.worker.stop_with(|h| h.sink.stop());
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        // In-flight buffers can't be carried over: hand them back now.
        let snap = self
            .worker
            .with(|ctx, h| {
                h.complete(ctx, true);
                ctx.flush();
                Snap { params: h.params, state: h.state }
            })
            .unwrap_or_default();
        let data = serde_json::to_vec(&snap).map_err(|e| Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        let snap: Snap = serde_json::from_slice(data).map_err(|e| Error::Device(e.to_string()))?;
        if snap.params.is_some_and(|p| p.period_bytes == 0 || p.buffer_bytes < p.period_bytes || !(1..=2).contains(&p.channels)) {
            return Err(Error::Device("sound snapshot: bad params".into()));
        }
        let h = SoundHandler::new((self.make_sink)(), snap);
        self.worker.start("sound", ctx, h, true)
    }
}
