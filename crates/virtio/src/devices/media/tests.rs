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
            window,
            sessions: BTreeMap::new(),
            alloc: Alloc::default(),
            events: VecDeque::new(),
            next_cookie: 1,
            helper_lost: false,
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
    assert_eq!(t.sent(), vec![ToHelper::Open { session: 0, codec: Codec::H264 }]);

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
    t.reply(FromHelper::Format { session: 0, layout, visible: (1280, 720), min_buffers: 4 });
    assert_eq!(t.events(), vec![(EVT_EVENT, EVENT_SOURCE_CHANGE, 0, 0, 0)]);
    let (_, f) = t.fmt(ioc::G_FMT, CAPTURE_MPLANE, 0, 0, 0);
    assert_eq!((get_u32(&f, fmt::WIDTH), get_u32(&f, fmt::HEIGHT), get_u32(&f, fmt::PIXELFORMAT)), (1280, 720, PIX_NV12));
    // The buffers, made before the size was known (for 1920x1088), fit.
    assert_eq!(t.stream(ioc::STREAMON, CAPTURE_MPLANE), 0);
    for i in 0..3 {
        assert_eq!(t.qbuf(CAPTURE_MPLANE, i, 0), 0);
    }
    let credits: u32 = t.sent().iter().map(|m| if let ToHelper::Credit { n, .. } = m { *n } else { 0 }).sum();
    assert_eq!(credits, 3);
    let epoch = t.h.sessions[&0].epoch;

    // A picture fills the first queued buffer, with the bitstream's time.
    let pic = vec![7u8; layout.size() as usize];
    t.reply(FromHelper::Picture { session: 0, epoch, cookie, data: pic.clone() });
    assert_eq!(t.events(), vec![(EVT_DQBUF, CAPTURE_MPLANE, 0, BUF_FLAG_TIMESTAMP_COPY, layout.size())]);
    let off = t.h.sessions[&0].cap_bufs[0].off;
    let mut got = vec![0u8; pic.len()];
    t.h.window.read(GuestAddress(off), &mut got).unwrap();
    assert_eq!(got, pic);
    assert_eq!(get_u32(&t.h.sessions[&0].cap_bufs[0].ts, 0), 1001);

    // Stale epoch, wrong size: dropped.
    t.reply(FromHelper::Picture { session: 0, epoch: epoch + 1, cookie, data: pic.clone() });
    t.reply(FromHelper::Picture { session: 0, epoch, cookie, data: vec![0; 10] });
    assert!(t.events().is_empty());

    // Drain: the helper is told after the last bitstream, then the LAST
    // buffer and EOS come back.
    let mut c = vec![0u8; DECODER_CMD_SIZE];
    put_u32(&mut c, 0, DEC_CMD_STOP);
    assert_eq!(t.ioctl(ioc::DECODER_CMD, &c, DECODER_CMD_SIZE as u32).0, 0);
    assert!(t.sent().contains(&ToHelper::Drain { session: 0 }));
    t.reply(FromHelper::DrainDone { session: 0 });
    assert_eq!(t.events(), vec![(EVT_DQBUF, CAPTURE_MPLANE, 1, BUF_FLAG_LAST | BUF_FLAG_TIMESTAMP_COPY, 0), (EVT_EVENT, EVENT_EOS, 0, 0, 0)]);

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
    t.reply(FromHelper::Format { session: 0, layout: Layout { width: 640, height: 480 }, visible: (640, 480), min_buffers: 4 });
    t.qbuf(CAPTURE_MPLANE, 0, 0);
    t.qbuf(CAPTURE_MPLANE, 1, 0);
    t.events();
    t.sent();
    // Bigger than the buffers: the next one goes back empty and LAST, and
    // no credit until the guest makes new buffers.
    t.reply(FromHelper::Format { session: 0, layout: Layout { width: 4096, height: 2304 }, visible: (4096, 2304), min_buffers: 4 });
    assert_eq!(t.events(), vec![(EVT_EVENT, EVENT_SOURCE_CHANGE, 0, 0, 0), (EVT_DQBUF, CAPTURE_MPLANE, 0, BUF_FLAG_LAST | BUF_FLAG_TIMESTAMP_COPY, 0)]);
    assert!(!t.sent().iter().any(|m| matches!(m, ToHelper::Credit { n, .. } if *n > 0)));
    t.stream(ioc::STREAMOFF, CAPTURE_MPLANE);
    assert_eq!(t.reqbufs(CAPTURE_MPLANE, 2), 2);
    assert!(t.h.sessions[&0].cap_bufs.iter().all(|b| b.len >= 4096 * 2304 * 3 / 2));
    t.stream(ioc::STREAMON, CAPTURE_MPLANE);
    t.qbuf(CAPTURE_MPLANE, 0, 0);
    assert!(t.sent().iter().any(|m| matches!(m, ToHelper::Credit { n: 1, .. })));
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
    t.reply(FromHelper::Picture { session: 3, epoch: 0, cookie: 0, data: vec![] });
    t.reply(FromHelper::Picture { session: 0, epoch: 0, cookie: 0, data: vec![0; 16 * 16 * 3 / 2] });
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
    assert_eq!(n, 17);
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
                0 => FromHelper::Format { session: s, layout: Layout { width: 16 * (1 + rnd() % 50), height: 16 * (1 + rnd() % 50) }, visible: (16, 16), min_buffers: 4 },
                1 => FromHelper::Picture { session: s, epoch: rnd() % 3, cookie: rnd() % 5, data: vec![1; (rnd() % 5000) as usize] },
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
