"""Drive a vroom VM through its --ui-fd viewer socket.

usage: vmdrive.py <vroom binary> <cfg> <state-dir> [--fresh]
Commands, one per line, on <state-dir>.cmd (a fifo):
  click X Y | rclick X Y | dclick X Y | move X Y   (guest pixels)
  key CODE[+CODE...]     press in order, release in reverse (evdev codes)
  type TEXT              US layout
  shot                   screendump (path shows in <state-dir>.log; other
                         monitors next to it, as -2.png, -3.png...)
  snapshot | suspend     what Ctrl-A s / Ctrl-A z do on the console
  resize W H | focus 0|1 | clip TEXT | cmd N | quit
  hmove N X Y | hclick N X Y   on monitor N (2, 3...), its guest pixels
  hresize N W H          monitor N's window size (0 0: closed)
  coherence 0|1          Coherence on/off (windows show as "windows N: ...")
  win ID activate|minimize|close | win ID move X Y W H
  startapp NAME          start a guest app by name (icons show as "icon ID ...")
  serial TEXT            to the serial console (\\n for Enter)
  drop X Y X2 Y2 PATH... drag files in at X,Y, move to X2,Y2, drop there
                         (what the app does when files come from Finder)
  usb ID [SERIAL]        plug a host USB device in, as the app does: start
                         `vroom usb-host` for it, hand the VMM its socket
                         (VROOM_USB_FAKE=acm in our environment: a pretend
                         serial port)
  usbdetach PORT         pull out the device on that port pair
  dragout X Y X2 Y2 DIR  press at X,Y, drag to X2,Y2 (an edge), ask the guest
                         what it drags out (as the app does when the mouse
                         leaves the window), cancel its drag with Escape and
                         fetch every item into DIR (as the Mac drop would)
Viewer events (mode, clipboard, status, mic, guest pointer: its picture as
<state-dir>.cursor-N.png) go to <state-dir>.events.
"""
import os, socket, struct, subprocess, sys, threading, time

vroom, cfg, state = sys.argv[1:4]
extra = sys.argv[4:]
cmdp, evp = state + ".cmd", state + ".events"
if not os.path.exists(cmdp):
    os.mkfifo(cmdp)
a, b = socket.socketpair()
p = subprocess.Popen([vroom, "run", cfg, "--ui-fd", str(b.fileno()), "--state-dir", state] + extra,
                     pass_fds=[b.fileno()], stdin=subprocess.PIPE,
                     stdout=open(state + ".log", "ab"), stderr=subprocess.STDOUT)
b.close()
socket.recv_fds(a, 1, 1)  # framebuffer shm; we use screendumps instead
mode = [800, 600]
hmodes = {}
cursors = [0]
ev = open(evp, "a", buffering=1)

def write_png(path, w, h, rgba):
    import zlib
    rows = b"".join(b"\0" + rgba[y * w * 4:(y + 1) * w * 4] for y in range(h))
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))

def reader():
    buf = b""
    while True:
        d = a.recv(65536)
        if not d:
            ev.write("closed\n"); return
        buf += d
        while len(buf) >= 5:
            t, n = struct.unpack("<BI", buf[:5])
            if len(buf) < 5 + n:
                break
            pl, buf = buf[5:5 + n], buf[5 + n:]
            if t == 1:
                mode[:] = struct.unpack("<II", pl); ev.write(f"mode {mode[0]}x{mode[1]}\n")
            elif t == 16:
                h, w, hh = struct.unpack("<III", pl); hmodes[h] = (w, hh); ev.write(f"mode{h + 1} {w}x{hh}\n")
            elif t == 3:
                ev.write("clipboard " + pl.decode(errors="replace").replace("\n", "\\n") + "\n")
            elif t == 4:
                ev.write("status " + pl.decode(errors="replace") + "\n")
            elif t == 5:
                ev.write("closed\n")
            elif t == 6:
                ev.write(f"mic {pl[0]}\n")
            elif t == 8:
                i, live = struct.unpack("<IB", pl[:5]); ev.write(f"dragready {i} {live}\n")
                drag_ev.append(("ready", i, live)); drag_cv.set()
            elif t == 9:
                i, ok = struct.unpack("<IB", pl[:5]); place = pl[5:].decode(errors="replace")
                ev.write(f"dropdone {i} {ok} {place}\n")
                drag_ev.append(("done", i, ok)); drag_cv.set()
            elif t == 11:
                n, o, out = pl[0], 1, []
                for _ in range(n):
                    port, flags = pl[o], pl[o + 1]; o += 2
                    f = []
                    for _ in range(3):
                        ln = struct.unpack("<H", pl[o:o + 2])[0]; f.append(pl[o + 2:o + 2 + ln].decode()); o += 2 + ln
                    out.append(f"{port}:{f[0]}{'' if flags & 1 else '(away)'}{'(always)' if flags & 2 else ''}")
                ev.write("usb " + " ".join(out) + "\n")
            elif t == 10:
                ev.write(f"camera {pl[0]}\n")
            elif t == 12:
                ev.write(f"agent features {struct.unpack('<Q', pl)[0]:#x}\n")
            elif t == 13:
                n, o, out = struct.unpack("<I", pl[:4])[0], 4, []
                for _ in range(n):
                    i, x, y, w, h, f, tl = struct.unpack("<IiiIIIH", pl[o:o + 26])
                    out.append(f"{i:#x} {x},{y} {w}x{h} f{f} {pl[o + 26:o + 26 + tl].decode(errors='replace')!r}")
                    o += 26 + tl
                ev.write(f"windows {n}: " + " | ".join(out) + "\n")
            elif t == 14:
                i, = struct.unpack("<I", pl[:4]); items, o = [], 4
                while o < len(pl):
                    d, n = pl[o], struct.unpack("<H", pl[o + 1:o + 3])[0]
                    items.append((pl[o + 3:o + 3 + n].decode(), bool(d))); o += 3 + n
                ev.write(f"dragout {i}: {items}\n")
                drag_ev.append(("out", i, items)); drag_cv.set()
            elif t == 18:
                i, w, h = struct.unpack("<IHH", pl[:8])
                png = f"{state}.icon-{i:x}.png"
                if w:
                    write_png(png, w, h, pl[8:])
                ev.write(f"icon {i:#x} {w}x{h} {png if w else 'none'}\n")
            elif t == 19:
                ev.write(f"no coherence: {pl.decode(errors='replace')}\n")
            elif t == 7:
                if pl[0] != 2:
                    ev.write("cursor " + ("unset", "hidden")[pl[0]] + "\n"); continue
                w, h, hx, hy = struct.unpack("<IIII", pl[1:17])
                cursors[0] += 1
                png = f"{state}.cursor-{cursors[0]}.png"
                write_png(png, w, h, pl[17:])
                ev.write(f"cursor {w}x{h} hot {hx},{hy} {png}\n")
drag_ev, drag_cv = [], threading.Event()
threading.Thread(target=reader, daemon=True).start()

def send(t, payload):
    a.sendall(struct.pack("<BI", t, len(payload)) + payload)
def u32s(*v):
    return b"".join(struct.pack("<I", x & 0xffffffff) for x in v)
def key(code, down):
    send(1, u32s(code, int(down))); time.sleep(0.03)
def move(x, y):
    send(2, u32s(int(x * 32767 / max(mode[0] - 1, 1)), int(y * 32767 / max(mode[1] - 1, 1)))); time.sleep(0.05)
def hmove(n, x, y):
    w, h = hmodes.get(n - 1, (800, 600))
    send(19, u32s(n - 1, int(x * 32767 / max(w - 1, 1)), int(y * 32767 / max(h - 1, 1)))); time.sleep(0.05)
def button(code, down):
    send(3, u32s(code, int(down))); time.sleep(0.05)
def click(x, y, btn=0x110, n=1):
    move(x, y)
    for _ in range(n):
        button(btn, True); button(btn, False)

ROWS = {"1234567890-=": 2, "qwertyuiop[]": 16, "asdfghjkl;'": 30, "zxcvbnm,./": 44}
SHIFT_ROWS = {"!@#$%^&*()_+": 2, "QWERTYUIOP{}": 16, 'ASDFGHJKL:"': 30, "ZXCVBNM<>?": 44}
KEYS = {" ": (57, False), "\n": (28, False), "\t": (15, False), "`": (41, False), "~": (41, True),
        "\\": (43, False), "|": (43, True)}
for rows, sh in ((ROWS, False), (SHIFT_ROWS, True)):
    for row, base in rows.items():
        for i, c in enumerate(row):
            KEYS[c] = (base + i, sh)

def type_text(s):
    for c in s:
        code, sh = KEYS[c]
        if sh: key(42, True)
        key(code, True); key(code, False)
        if sh: key(42, False)

def agent_frame(t, payload):
    return b"VR\x01" + bytes([t]) + struct.pack("<I", len(payload)) + payload

def wait_drag(kind, i, secs):
    end = time.time() + secs
    while time.time() < end:
        for e in list(drag_ev):
            if e[1] == i and e[0] in kind:
                drag_ev.remove(e); return e
        drag_cv.wait(0.1); drag_cv.clear()
    return None

drop_id = [100]
def drop(x, y, x2, y2, paths):
    drop_id[0] += 1; i = drop_id[0]
    move(x, y)
    items = b"".join(bytes([os.path.isdir(q)]) + struct.pack("<H", len(os.path.basename(q).encode())) + os.path.basename(q).encode() for q in paths)
    send(9, u32s(i) + items)
    e = wait_drag(("ready", "done"), i, 10)
    ev.write(f"drop {i}: guest says {e}\n")
    if e is None or e[0] == "done":
        return
    live = e[2]
    if live:
        button(0x110, True)
    for k in range(1, 11):
        move(x + (x2 - x) * k // 10, y + (y2 - y) * k // 10); time.sleep(0.05)
    if os.path.exists(state + ".dropshot"):  # screenshot mid-drag (tests)
        p.stdin.write(b"\x01g"); p.stdin.flush(); time.sleep(1.5)
    r, w = socket.socketpair()
    socket.send_fds(a, [struct.pack("<BI", 11, 4) + u32s(i)], [r.fileno()]); r.close()
    def writer():
        def walk(path, rel):
            if os.path.isdir(path):
                w.sendall(agent_frame(13, u32s(i) + b"\x01" + struct.pack("<Q", 0) + rel.encode()))
                for n in sorted(os.listdir(path)):
                    walk(os.path.join(path, n), rel + "/" + n)
            elif os.path.isfile(path):
                size = os.path.getsize(path)
                w.sendall(agent_frame(13, u32s(i) + b"\x00" + struct.pack("<Q", size) + rel.encode()))
                with open(path, "rb") as f:
                    while True:
                        d = f.read(256 << 10)
                        if not d: break
                        w.sendall(agent_frame(14, u32s(i) + d))
        t0 = time.time()
        for q in paths:
            walk(q, os.path.basename(q))
        w.sendall(agent_frame(15, u32s(i))); w.close()
        ev.write(f"drop {i}: sent in {time.time() - t0:.1f}s\n")
    threading.Thread(target=writer, daemon=True).start()
    e = wait_drag(("done",), i, 600)
    ev.write(f"drop {i}: done {e}\n")
    if live:
        button(0x110, False)

helpers = []
def usb_attach(id_, serial=""):
    r, w = socket.socketpair()
    args = [vroom, "usb-host", "--id", id_] + (["--serial", serial] if serial else [])
    helpers.append(subprocess.Popen(args, stdin=w.fileno(), stdout=subprocess.DEVNULL,
                                    stderr=open(state + ".log", "ab")))
    w.close()
    pl = b"".join(struct.pack("<H", len(x.encode())) + x.encode() for x in (id_, serial, "test " + id_))
    socket.send_fds(a, [struct.pack("<BI", 12, len(pl)) + pl], [r.fileno()]); r.close()
def fetch(i, idx, into):
    r, w = socket.socketpair()
    socket.send_fds(a, [struct.pack("<BI", 17, 8) + u32s(i, idx)], [w.fileno()]); w.close()
    buf, f, t0, n = b"", None, time.time(), 0
    r.settimeout(60)
    while True:
        try:
            d = r.recv(1 << 20)
        except socket.timeout:
            return "timeout"
        if not d:
            return "pipe closed"
        buf += d
        while len(buf) >= 8:
            t, ln = buf[3], struct.unpack("<I", buf[4:8])[0]
            if len(buf) < 8 + ln:
                break
            pl, buf = buf[8:8 + ln], buf[8 + ln:]
            if t == 13:
                isdir, size, path = pl[4], struct.unpack("<Q", pl[5:13])[0], pl[13:].decode()
                if ".." in path.split("/") or path.startswith("/"):
                    return "bad path " + path
                dst = os.path.join(into, path)
                if f: f.close(); f = None
                if isdir: os.makedirs(dst, exist_ok=True)
                else: f = open(dst, "xb")
            elif t == 14:
                f.write(pl[4:]); n += len(pl) - 4
            elif t == 15:
                if f: f.close()
                return f"ok {n} bytes in {time.time() - t0:.1f}s"
            elif t == 24:
                return "failed: " + pl[4:].decode(errors="replace")

out_id = [500]
def dragout(x, y, x2, y2, into):
    out_id[0] += 1; i = out_id[0]
    move(x, y); button(0x110, True)
    for k in range(1, 21):
        move(x + (x2 - x) * k // 20, y + (y2 - y) * k // 20); time.sleep(0.03)
    time.sleep(0.3)
    t0 = time.time()
    send(16, u32s(i) + b"\x00")
    e = wait_drag(("out",), i, 10)
    ev.write(f"dragout {i}: answer after {time.time() - t0:.2f}s: {e}\n")
    if os.path.exists(state + ".dropshot"):  # screenshot mid-drag (tests)
        p.stdin.write(b"\x01g"); p.stdin.flush(); time.sleep(1.5)
    items = e[2] if e else []
    if items:
        key(1, True); key(1, False)
    button(0x110, False)
    os.makedirs(into, exist_ok=True)
    for idx, (name, d) in enumerate(items):
        ev.write(f"dragout {i}: {name}: {fetch(i, idx, into)}\n")
    send(16, u32s(i) + b"\x01")

send(6, b"\x01")  # focused: lets clipboard flow
import select
def lines():
    # Non-blocking fifo: never outlive the VM (a stale reader would eat
    # commands meant for the next run).
    fd = os.open(cmdp, os.O_RDONLY | os.O_NONBLOCK)
    buf = b""
    while p.poll() is None:
        r, _, _ = select.select([fd], [], [], 0.5)
        if not r:
            continue
        d = os.read(fd, 65536)
        if not d:
            os.close(fd)
            fd = os.open(cmdp, os.O_RDONLY | os.O_NONBLOCK)
            continue
        buf += d
        while b"\n" in buf:
            l, buf = buf.split(b"\n", 1)
            yield l.decode() + "\n"

if True:
    if True:
        for line in lines():
            w = line.rstrip("\n").split(" ", 1)
            c, arg = w[0], (w[1] if len(w) > 1 else "")
            try:
                if c in ("click", "rclick", "dclick", "move"):
                    x, y = map(int, arg.split())
                    if c == "move": move(x, y)
                    else: click(x, y, 0x111 if c == "rclick" else 0x110, 2 if c == "dclick" else 1)
                elif c in ("hmove", "hclick"):
                    n, x, y = map(int, arg.split())
                    hmove(n, x, y)
                    if c == "hclick":
                        button(0x110, True); button(0x110, False)
                elif c == "hresize":
                    n, w, h = map(int, arg.split())
                    send(18, u32s(n - 1, w, h, 0))
                elif c == "key":
                    codes = [int(k) for k in arg.split("+")]
                    for k in codes: key(k, True)
                    for k in reversed(codes): key(k, False)
                elif c == "type": type_text(arg.replace("\\n", "\n"))
                elif c == "shot": p.stdin.write(b"\x01g"); p.stdin.flush()
                elif c == "snapshot": p.stdin.write(b"\x01s"); p.stdin.flush()
                elif c == "suspend": p.stdin.write(b"\x01z"); p.stdin.flush()
                elif c == "resize": send(5, u32s(*map(int, arg.split())))
                elif c == "focus": send(6, bytes([int(arg)]))
                elif c == "clip": send(7, arg.encode())
                elif c == "cmd": send(8, bytes([int(arg)]))
                elif c == "serial": p.stdin.write(arg.replace("\\n", "\r").encode()); p.stdin.flush()
                elif c == "drop":
                    v = arg.split(" ", 4)
                    drop(int(v[0]), int(v[1]), int(v[2]), int(v[3]), v[4].split("|"))
                elif c == "usb": usb_attach(*arg.split())
                elif c == "usbdetach": send(13, bytes([int(arg)]))
                elif c == "dragout":
                    v = arg.split(" ", 4)
                    dragout(int(v[0]), int(v[1]), int(v[2]), int(v[3]), v[4])
                elif c == "quit": p.stdin.write(b"\x01x"); p.stdin.flush()
                elif c == "coherence": send(14, bytes([int(arg)]))
                elif c == "startapp": send(20, arg.strip().encode())
                elif c == "win":
                    v = arg.split()
                    ops = {"activate": 1, "minimize": 2, "close": 3, "move": 4}
                    pl = u32s(int(v[0], 0)) + bytes([ops[v[1]]])
                    if v[1] == "move":
                        pl += u32s(*[int(x) for x in v[2:6]])
                    send(15, pl)
            except Exception as e:
                ev.write(f"error {line.strip()}: {e}\n")
p.wait()
