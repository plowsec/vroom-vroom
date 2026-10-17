use std::sync::Mutex;

use super::*;
use crate::queue::tests::{TestRing, DATA};

#[derive(Default)]
struct FakeLink {
    sent: Mutex<Vec<ToHelper>>,
    replies: Mutex<VecDeque<FromHelper>>,
}

impl HelperLink for FakeLink {
    fn send(&self, m: &ToHelper) -> bool {
        self.sent.lock().unwrap().push(m.clone());
        true
    }
    fn try_recv(&self) -> Option<FromHelper> {
        self.replies.lock().unwrap().pop_front()
    }
    fn set_poker(&self, _: Poker) {}
    fn alive(&self) -> bool {
        true
    }
}

struct T {
    h: Handler,
    link: Arc<FakeLink>,
}

impl T {
    fn new() -> T {
        let link = Arc::new(FakeLink::default());
        let window = GuestMemory::new(&[(GuestAddress(0), WINDOW)]).unwrap();
        let h = Handler {
            link: link.clone(),
            codecs: codec_list(u32::MAX),
            window,
            sessions: BTreeMap::new(),
            alloc: Alloc::default(),
            events: VecDeque::new(),
            next_cookie: 1,
            helper_lost: false,
            last_dump: None,
        };
        T { h, link }
    }

    /// Send a command (bytes) with room for `out` bytes of answer.
    fn cmd(&mut self, input: &[u8], out: u32) -> Vec<u8> {
        let mut ring = TestRing::new(4);
        let mut q = ring.q.clone();
        let head = ring.add_chain(&[(input.len() as u32, false), (out.max(1), true)]);
        ring.mem.write(GuestAddress(DATA + 0x1000 * head as u64), input).unwrap();
        let chain = q.pop(&ring.mem).unwrap().unwrap();
        let (mut r, mut w) = chain.into_rw().unwrap();
        self.h.command(&mut r, &mut w);
        let n = w.written();
        let mut v = vec![0u8; n];
        ring.mem.read(GuestAddress(DATA + 0x1000 * (head as u64 + 1)), &mut v).unwrap();
        v
    }

    /// An ioctl: (status, payload).
    fn ioctl(&mut self, code: u32, payload: &[u8], out: u32) -> (u32, Vec<u8>) {
        let mut c = vec![0u8; 16];
        put_u32(&mut c, 0, CMD_IOCTL);
        put_u32(&mut c, 8, 0);
        put_u32(&mut c, 12, code);
        c.extend_from_slice(payload);
        let r = self.cmd(&c, 8 + out);
        (get_u32(&r, 0), r.get(8..).unwrap_or(&[]).to_vec())
    }

    fn open(&mut self) -> u32 {
        let r = self.cmd(&[1, 0, 0, 0, 0, 0, 0, 0], 16);
        assert_eq!(get_u32(&r, 0), 0);
        get_u32(&r, 8)
    }

    fn fmt(&mut self, code: u32, t: u32, pix: u32, w: u32, h: u32) -> (u32, Vec<u8>) {
        let mut f = vec![0u8; FORMAT_SIZE];
        put_u32(&mut f, fmt::TYPE, t);
        put_u32(&mut f, fmt::WIDTH, w);
        put_u32(&mut f, fmt::HEIGHT, h);
        put_u32(&mut f, fmt::PIXELFORMAT, pix);
        self.ioctl(code, &f, FORMAT_SIZE as u32)
    }

    fn reqbufs(&mut self, t: u32, count: u32) -> u32 {
        let mut b = vec![0u8; REQBUFS_SIZE];
        put_u32(&mut b, 0, count);
        put_u32(&mut b, 4, t);
        put_u32(&mut b, 8, MEMORY_MMAP);
        let (st, r) = self.ioctl(ioc::REQBUFS, &b, REQBUFS_SIZE as u32);
        assert_eq!(st, 0);
        get_u32(&r, 0)
    }

    fn qbuf(&mut self, t: u32, index: u32, bytesused: u32) -> u32 {
        let mut b = vec![0u8; BUFFER_SIZE + PLANE_SIZE];
        put_u32(&mut b, buf::INDEX, index);
        put_u32(&mut b, buf::TYPE, t);
        put_u32(&mut b, buf::MEMORY, MEMORY_MMAP);
        put_u32(&mut b, buf::LENGTH, 1);
        put_u32(&mut b, buf::TIMESTAMP, 1000 + index);
        put_u32(&mut b, BUFFER_SIZE + plane::BYTESUSED, bytesused);
        self.ioctl(ioc::QBUF, &b, (BUFFER_SIZE + PLANE_SIZE) as u32).0
    }

    fn stream(&mut self, code: u32, t: u32) -> u32 {
        self.ioctl(code, &t.to_le_bytes(), 0).0
    }

    fn reply(&mut self, m: FromHelper) {
        self.link.replies.lock().unwrap().push_back(m);
        self.h.messages();
    }

    fn sent(&self) -> Vec<ToHelper> {
        std::mem::take(&mut *self.link.sent.lock().unwrap())
    }

    /// Events so far: (kind, queue type or v4l2 event type, index, flags, bytesused).
    fn events(&mut self) -> Vec<(u32, u32, u32, u32, u32)> {
        self.h
            .events
            .drain(..)
            .map(|e| match get_u32(&e, 0) {
                EVT_DQBUF => (EVT_DQBUF, get_u32(&e, 8 + buf::TYPE), get_u32(&e, 8 + buf::INDEX), get_u32(&e, 8 + buf::FLAGS), get_u32(&e, 8 + BUFFER_SIZE)),
                k => (k, get_u32(&e, 8), 0, 0, 0),
            })
            .collect()
    }
}

fn subscribe(t: &mut T, kind: u32) {
    let mut s = vec![0u8; SUBSCRIPTION_SIZE];
    put_u32(&mut s, 0, kind);
    assert_eq!(t.ioctl(ioc::SUBSCRIBE_EVENT, &s, 0).0, 0);
}

#[test]
fn decodes_through_the_helper() {
    let mut t = T::new();
    assert_eq!(t.open(), 0);
    let (st, f) = t.fmt(ioc::S_FMT, OUTPUT_MPLANE, PIX_H264, 1280, 720);
    assert_eq!(st, 0);
    assert_eq!(get_u32(&f, fmt::PIXELFORMAT), PIX_H264);
    assert!(get_u32(&f, fmt::SIZEIMAGE) >= 1280 * 720 * 3 / 2);
    subscribe(&mut t, EVENT_SOURCE_CHANGE);
    subscribe(&mut t, EVENT_EOS);
    assert_eq!(t.reqbufs(OUTPUT_MPLANE, 2), 2);
    assert_eq!(t.reqbufs(CAPTURE_MPLANE, 3), 3);
    assert_eq!(t.stream(ioc::STREAMON, OUTPUT_MPLANE), 0);
    assert_eq!(t.sent(), vec![ToHelper::Open { session: 0, codec: Codec::H264, flags: 0 }]);

    // A bitstream buffer: its bytes go to the helper, the buffer comes back.
    let off = t.h.sessions[&0].out_bufs[1].off;
    t.h.window.write(GuestAddress(off), &[0, 0, 1, 0x65, 0x88]).unwrap();
    assert_eq!(t.qbuf(OUTPUT_MPLANE, 1, 5), 0);
    let sent = t.sent();
    let ToHelper::Decode { cookie, data, .. } = &sent[0] else { panic!("{sent:?}") };
    assert_eq!(data, &[0, 0, 1, 0x65, 0x88]);
    let cookie = *cookie;
    assert_eq!(t.events(), vec![(EVT_DQBUF, OUTPUT_MPLANE, 1, BUF_FLAG_TIMESTAMP_COPY, 5)]);

    // The stream's size: an event, then the picture queue's format.
    let layout = Layout { width: 1280, height: 720 };
    t.reply(FromHelper::Format { session: 0, layout, visible: (1280, 720), min_buffers: 4, depth: 8 });
    assert_eq!(t.events(), vec![(EVT_EVENT, EVENT_SOURCE_CHANGE, 0, 0, 0)]);
    let (_, f) = t.fmt(ioc::G_FMT, CAPTURE_MPLANE, 0, 0, 0);
    assert_eq!((get_u32(&f, fmt::WIDTH), get_u32(&f, fmt::HEIGHT), get_u32(&f, fmt::PIXELFORMAT)), (1280, 720, PIX_NV12));
    // The buffers, made before the size was known (for 1920x1088), fit.
    assert_eq!(t.stream(ioc::STREAMON, CAPTURE_MPLANE), 0);
    for i in 0..3 {
        assert_eq!(t.qbuf(CAPTURE_MPLANE, i, 0), 0);
    }
    // Each queued buffer is granted, where it is, in order.
    let offs: Vec<u32> = (0..3).map(|i| t.h.sessions[&0].cap_bufs[i].off as u32).collect();
    let grants: Vec<u32> = t.sent().iter().filter_map(|m| if let ToHelper::Grant { offset, len, .. } = m { (*len > 0).then_some(*offset) } else { None }).collect();
    assert_eq!(grants, offs);
    let epoch = t.h.sessions[&0].epoch;
    let size = layout.size(PixFmt::Nv12);

    // The helper wrote a picture into the first one: it goes back with
    // the bitstream's time.
    t.reply(FromHelper::Picture { session: 0, epoch, cookie, offset: offs[0], bytes: size, ok: true });
    assert_eq!(t.events(), vec![(EVT_DQBUF, CAPTURE_MPLANE, 0, BUF_FLAG_TIMESTAMP_COPY, size)]);
    assert_eq!(get_u32(&t.h.sessions[&0].cap_bufs[0].ts, 0), 1001);

    // Stale epoch, not the next granted buffer: dropped. Wrong size: the
    // buffer goes back as an error.
    t.reply(FromHelper::Picture { session: 0, epoch: epoch + 1, cookie, offset: offs[1], bytes: size, ok: true });
    t.reply(FromHelper::Picture { session: 0, epoch, cookie, offset: offs[2], bytes: size, ok: true });
    assert!(t.events().is_empty());
    t.reply(FromHelper::Picture { session: 0, epoch, cookie, offset: offs[1], bytes: 10, ok: true });
    assert_eq!(t.events(), vec![(EVT_DQBUF, CAPTURE_MPLANE, 1, BUF_FLAG_ERROR | BUF_FLAG_TIMESTAMP_COPY, 0)]);

    // Drain: the helper is told after the last bitstream, then the LAST
    // buffer and EOS come back.
    let mut c = vec![0u8; DECODER_CMD_SIZE];
    put_u32(&mut c, 0, DEC_CMD_STOP);
    assert_eq!(t.ioctl(ioc::DECODER_CMD, &c, DECODER_CMD_SIZE as u32).0, 0);
    assert!(t.sent().contains(&ToHelper::Drain { session: 0 }));
    t.reply(FromHelper::DrainDone { session: 0 });
    assert_eq!(t.events(), vec![(EVT_DQBUF, CAPTURE_MPLANE, 2, BUF_FLAG_LAST | BUF_FLAG_TIMESTAMP_COPY, 0), (EVT_EVENT, EVENT_EOS, 0, 0, 0)]);

    // Closing frees the window.
    let mut close = vec![0u8; 16];
    put_u32(&mut close, 0, CMD_CLOSE);
    t.cmd(&close, 0);
    assert!(t.h.alloc.used.is_empty());
    assert!(t.sent().contains(&ToHelper::Close { session: 0 }));
}

#[test]
fn a_size_change_mid_stream_needs_new_buffers() {
    let mut t = T::new();
    t.open();
    subscribe(&mut t, EVENT_SOURCE_CHANGE);
    t.reqbufs(OUTPUT_MPLANE, 1);
    t.reqbufs(CAPTURE_MPLANE, 2);
    t.stream(ioc::STREAMON, OUTPUT_MPLANE);
    t.stream(ioc::STREAMON, CAPTURE_MPLANE);
    t.reply(FromHelper::Format { session: 0, layout: Layout { width: 640, height: 480 }, visible: (640, 480), min_buffers: 4, depth: 8 });
    t.qbuf(CAPTURE_MPLANE, 0, 0);
    t.qbuf(CAPTURE_MPLANE, 1, 0);
    t.events();
    t.sent();
    // Bigger than the buffers: the next one goes back empty and LAST, and
    // no credit until the guest makes new buffers.
    t.reply(FromHelper::Format { session: 0, layout: Layout { width: 4096, height: 2304 }, visible: (4096, 2304), min_buffers: 4, depth: 8 });
    assert_eq!(t.events(), vec![(EVT_EVENT, EVENT_SOURCE_CHANGE, 0, 0, 0), (EVT_DQBUF, CAPTURE_MPLANE, 0, BUF_FLAG_LAST | BUF_FLAG_TIMESTAMP_COPY, 0)]);
    assert!(!t.sent().iter().any(|m| matches!(m, ToHelper::Grant { len, .. } if *len > 0)));
    t.stream(ioc::STREAMOFF, CAPTURE_MPLANE);
    assert_eq!(t.reqbufs(CAPTURE_MPLANE, 2), 2);
    assert!(t.h.sessions[&0].cap_bufs.iter().all(|b| b.len >= 4096 * 2304 * 3 / 2));
    t.stream(ioc::STREAMON, CAPTURE_MPLANE);
    t.qbuf(CAPTURE_MPLANE, 0, 0);
    assert_eq!(t.sent().iter().filter(|m| matches!(m, ToHelper::Grant { len, .. } if *len > 0)).count(), 1);
}

#[test]
fn hostile_requests_are_refused() {
    let mut t = T::new();
    t.open();
    // Unknown session, queue type, buffer index, memory type.
    let mut c = vec![0u8; 16];
    put_u32(&mut c, 0, CMD_IOCTL);
    put_u32(&mut c, 8, 5);
    put_u32(&mut c, 12, ioc::STREAMON);
    c.extend_from_slice(&OUTPUT_MPLANE.to_le_bytes());
    assert_eq!(get_u32(&t.cmd(&c, 8), 0), EINVAL);
    assert_eq!(t.stream(ioc::STREAMON, 3), EINVAL);
    assert_eq!(t.stream(ioc::STREAMON, OUTPUT_MPLANE), EINVAL);
    assert_eq!(t.reqbufs(OUTPUT_MPLANE, 1000), MAX_OUTPUT_BUFS);
    assert_eq!(t.qbuf(OUTPUT_MPLANE, 99, 10), EINVAL);
    assert_eq!(t.qbuf(OUTPUT_MPLANE, 0, 10), 0);
    assert_eq!(t.qbuf(OUTPUT_MPLANE, 0, 10), EINVAL, "queued twice");
    let mut b = vec![0u8; REQBUFS_SIZE];
    put_u32(&mut b, 0, 1);
    put_u32(&mut b, 4, CAPTURE_MPLANE);
    put_u32(&mut b, 8, 2); // USERPTR
    assert_eq!(t.ioctl(ioc::REQBUFS, &b, REQBUFS_SIZE as u32).0, EINVAL);
    // Too many planes, truncated payloads, unknown ioctls.
    let mut q = vec![0u8; BUFFER_SIZE];
    put_u32(&mut q, buf::TYPE, OUTPUT_MPLANE);
    put_u32(&mut q, buf::LENGTH, 9);
    assert_eq!(t.ioctl(ioc::QBUF, &q, 600).0, EINVAL);
    assert_eq!(t.ioctl(ioc::S_FMT, &[0; 10], 0).0, EINVAL);
    assert_eq!(t.ioctl(200, &[], 0).0, ENOTTY);
    // Mapping something that isn't a buffer.
    let mut m = vec![0u8; 20];
    put_u32(&mut m, 0, CMD_MMAP);
    put_u32(&mut m, 16, 12345);
    assert_eq!(get_u32(&t.cmd(&m, 24), 0), EINVAL);
    // A helper message for a session that isn't there, a picture without
    // a credit.
    t.reply(FromHelper::Picture { session: 3, epoch: 0, cookie: 0, offset: 0, bytes: 0, ok: true });
    t.reply(FromHelper::Picture { session: 0, epoch: 0, cookie: 0, offset: 0, bytes: 16 * 16 * 3 / 2, ok: true });
    // At most MAX_SESSIONS sessions.
    for _ in 1..MAX_SESSIONS {
        t.open();
    }
    let r = t.cmd(&[1, 0, 0, 0, 0, 0, 0, 0], 16);
    assert_eq!(get_u32(&r, 0), ENOMEM);
}

#[test]
fn the_window_runs_out_cleanly() {
    let mut a = Alloc::default();
    let mut n = 0;
    while a.alloc(30 << 20).is_some() {
        n += 1;
    }
    assert_eq!(n, 34);
    let first = *a.used.keys().next().unwrap();
    a.free(first);
    assert_eq!(a.alloc(1), Some(first));
}

#[test]
fn junk_commands_never_panic() {
    let mut t = T::new();
    t.open();
    t.reqbufs(OUTPUT_MPLANE, 2);
    t.reqbufs(CAPTURE_MPLANE, 2);
    let mut seed = 0x2545_f491u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for _ in 0..3000 {
        let len = (rnd() % 700) as usize + 8;
        let mut c: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
        put_u32(&mut c, 0, rnd() % 7);
        put_u32(&mut c, 8, rnd() % 3);
        put_u32(&mut c, 12, rnd() % 110);
        if rnd() % 2 == 0 {
            put_u32(&mut c, 16 + 4, [OUTPUT_MPLANE, CAPTURE_MPLANE][rnd() as usize % 2]);
        }
        t.cmd(&c, rnd() % 800);
        if rnd() % 5 == 0 {
            let s = rnd() % 3;
            let m = match rnd() % 5 {
                0 => FromHelper::Format { session: s, layout: Layout { width: 16 * (1 + rnd() % 50), height: 16 * (1 + rnd() % 50) }, visible: (16, 16), min_buffers: 4, depth: 8 + 2 * (rnd() % 2) },
                1 => FromHelper::Picture { session: s, epoch: rnd() % 3, cookie: rnd() % 5, offset: (rnd() % 8) << 16, bytes: rnd() % 5000, ok: rnd() % 2 == 0 },
                2 => FromHelper::Consumed { session: s, cookie: rnd() % 5, ok: true },
                3 => FromHelper::DrainDone { session: s },
                _ => FromHelper::Error { session: s, errno: rnd() },
            };
            t.reply(m);
        }
        t.events();
        t.sent();
    }
}

#[test]
fn decoding_order_and_10_bits() {
    let mut t = T::new();
    t.open();
    // VP9, pictures in decoding order (display delay 0), P010 pictures.
    let (_, f) = t.fmt(ioc::S_FMT, OUTPUT_MPLANE, PIX_VP9, 1920, 1080);
    assert_eq!(get_u32(&f, fmt::PIXELFORMAT), PIX_VP9);
    let ctrl = |id: u32, v: u32| {
        let mut c = vec![0u8; CONTROL_SIZE];
        put_u32(&mut c, 0, id);
        put_u32(&mut c, 4, v);
        c
    };
    assert_eq!(t.ioctl(ioc::S_CTRL, &ctrl(CID_DEC_DISPLAY_DELAY_ENABLE, 1), CONTROL_SIZE as u32).0, 0);
    assert_eq!(t.ioctl(ioc::S_CTRL, &ctrl(CID_DEC_DISPLAY_DELAY, 99), CONTROL_SIZE as u32).0, EINVAL);
    assert_eq!(t.ioctl(ioc::S_CTRL, &ctrl(CID_VP9_PROFILE, 2), CONTROL_SIZE as u32).0, EINVAL, "read only");
    let (_, f) = t.fmt(ioc::S_FMT, CAPTURE_MPLANE, PIX_P010, 0, 0);
    assert_eq!(get_u32(&f, fmt::PIXELFORMAT), PIX_P010);
    assert_eq!(get_u32(&f, fmt::BYTESPERLINE), 1920 * 2);
    t.reqbufs(OUTPUT_MPLANE, 1);
    t.reqbufs(CAPTURE_MPLANE, 2);
    assert!(t.h.sessions[&0].cap_bufs.iter().all(|b| b.len >= 1920 * 1088 * 3));
    t.stream(ioc::STREAMON, OUTPUT_MPLANE);
    assert_eq!(t.sent(), vec![ToHelper::Open { session: 0, codec: Codec::Vp9, flags: OPEN_DECODE_ORDER }]);
    // Too late to change now.
    assert_eq!(t.ioctl(ioc::S_CTRL, &ctrl(CID_DEC_DISPLAY_DELAY_ENABLE, 0), CONTROL_SIZE as u32).0, EBUSY);
    t.stream(ioc::STREAMON, CAPTURE_MPLANE);
    let layout = Layout { width: 1920, height: 1088 };
    t.reply(FromHelper::Format { session: 0, layout, visible: (1920, 1080), min_buffers: 4, depth: 10 });
    t.qbuf(CAPTURE_MPLANE, 0, 0);
    t.qbuf(CAPTURE_MPLANE, 1, 0);
    let grants: Vec<PixFmt> = t.sent().iter().filter_map(|m| if let ToHelper::Grant { len, format, .. } = m { (*len > 0).then_some(*format) } else { None }).collect();
    assert_eq!(grants, vec![PixFmt::P010, PixFmt::P010]);
    t.events();
    // A picture that failed comes back as an error.
    let (epoch, off) = (t.h.sessions[&0].epoch, t.h.sessions[&0].cap_bufs[0].off as u32);
    t.reply(FromHelper::Picture { session: 0, epoch, cookie: 1, offset: off, bytes: 0, ok: false });
    assert_eq!(t.events(), vec![(EVT_DQBUF, CAPTURE_MPLANE, 0, BUF_FLAG_ERROR | BUF_FLAG_TIMESTAMP_COPY, 0)]);
    let off = t.h.sessions[&0].cap_bufs[1].off as u32;
    t.reply(FromHelper::Picture { session: 0, epoch, cookie: 1, offset: off, bytes: layout.size(PixFmt::P010), ok: true });
    assert_eq!(t.events(), vec![(EVT_DQBUF, CAPTURE_MPLANE, 1, BUF_FLAG_TIMESTAMP_COPY, layout.size(PixFmt::P010))]);
}

#[test]
fn codecs_the_mac_lacks_are_not_offered() {
    let mut t = T::new();
    t.h.codecs = codec_list(0);
    t.open();
    let mut e = vec![0u8; FMTDESC_SIZE];
    put_u32(&mut e, 4, OUTPUT_MPLANE);
    let mut got = Vec::new();
    for i in 0..4 {
        put_u32(&mut e, 0, i);
        let (st, b) = t.ioctl(ioc::ENUM_FMT, &e, FMTDESC_SIZE as u32);
        if st == 0 {
            got.push(get_u32(&b, 44));
        }
    }
    assert_eq!(got, vec![PIX_H264, PIX_HEVC]);
    // Asking for VP9 gets H.264.
    let (_, f) = t.fmt(ioc::S_FMT, OUTPUT_MPLANE, PIX_VP9, 640, 480);
    assert_eq!(get_u32(&f, fmt::PIXELFORMAT), PIX_H264);
}

#[test]
fn a_decoder_comes_back_after_a_restore() {
    let mut t = T::new();
    t.open();
    subscribe(&mut t, EVENT_SOURCE_CHANGE);
    t.fmt(ioc::S_FMT, OUTPUT_MPLANE, PIX_HEVC, 1280, 720);
    t.reqbufs(OUTPUT_MPLANE, 2);
    t.reqbufs(CAPTURE_MPLANE, 3);
    t.stream(ioc::STREAMON, OUTPUT_MPLANE);
    t.stream(ioc::STREAMON, CAPTURE_MPLANE);
    let layout = Layout { width: 1280, height: 720 };
    t.reply(FromHelper::Format { session: 0, layout, visible: (1280, 720), min_buffers: 4, depth: 8 });
    t.reply(FromHelper::Resume { session: 0, state: vec![0, 0, 0, 1, 0x40, 1] });
    for i in 0..3 {
        t.qbuf(CAPTURE_MPLANE, i, 0);
    }
    // A bitstream buffer still waiting when the snapshot is taken: the
    // helper is busy (in flight at its cap).
    t.h.sessions.get_mut(&0).unwrap().in_flight.insert(99, MAX_IN_FLIGHT);
    t.h.sessions.get_mut(&0).unwrap().in_flight_bytes = MAX_IN_FLIGHT;
    t.qbuf(OUTPUT_MPLANE, 1, 100);
    let saved: Vec<Saved> = t.h.sessions.iter().map(|(&id, s)| Saved::of(id, s)).collect();
    let data = serde_json::to_vec(&saved).unwrap();
    let offs: Vec<u64> = t.h.sessions[&0].cap_bufs.iter().map(|b| b.off).collect();

    // A new VMM, a new helper.
    let mut u = T::new();
    u.h.restore(&data);
    let sent = u.sent();
    assert_eq!(sent[0], ToHelper::Resume { session: 0, codec: Codec::Hevc, flags: 0, state: vec![0, 0, 0, 1, 0x40, 1] });
    let grants: Vec<u64> = sent.iter().filter_map(|m| if let ToHelper::Grant { offset, len, .. } = m { (*len > 0).then_some(*offset as u64) } else { None }).collect();
    assert_eq!(grants, offs, "the queued picture buffers, where they were");
    // The waiting bitstream buffer comes back to the guest.
    assert_eq!(u.events(), vec![(EVT_DQBUF, OUTPUT_MPLANE, 1, BUF_FLAG_TIMESTAMP_COPY, 0)]);
    // Same buffers mapped at the same offsets; the window has room for no
    // overlap.
    assert_eq!(u.h.alloc.used.len(), 5);
    assert_eq!(u.qbuf(OUTPUT_MPLANE, 1, 10), 0);

    // A state that doesn't hold together fails that decoder only.
    let mut bad = saved.clone();
    bad[0].cap_bufs[1].0 = bad[0].cap_bufs[0].0; // overlapping buffers
    let mut v = T::new();
    v.h.restore(&serde_json::to_vec(&bad).unwrap());
    assert!(v.h.sessions[&0].dead);
    assert!(v.h.alloc.used.is_empty());
    assert_eq!(v.events(), vec![(EVT_ERROR, EIO, 0, 0, 0)]);
    // An older snapshot (ids only): failed.
    let mut w = T::new();
    w.h.restore(b"[0]");
    assert!(w.h.sessions[&0].dead);
}
