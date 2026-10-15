"""Drive a vroom VM through its --ui-fd viewer socket.

usage: vmdrive.py <vroom binary> <cfg> <state-dir> [--fresh]
Commands, one per line, on <state-dir>.cmd (a fifo):
  click X Y | rclick X Y | dclick X Y | move X Y   (guest pixels)
  key CODE[+CODE...]     press in order, release in reverse (evdev codes)
  type TEXT              US layout
  shot                   screendump (path shows in <state-dir>.log)
  resize W H | focus 0|1 | clip TEXT | cmd N | quit
  serial TEXT            to the serial console (\\n for Enter)
Viewer events (mode, clipboard, status, mic) go to <state-dir>.events.
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
ev = open(evp, "a", buffering=1)

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
            elif t == 3:
                ev.write("clipboard " + pl.decode(errors="replace").replace("\n", "\\n") + "\n")
            elif t == 4:
                ev.write("status " + pl.decode(errors="replace") + "\n")
            elif t == 5:
                ev.write("closed\n")
            elif t == 6:
                ev.write(f"mic {pl[0]}\n")
threading.Thread(target=reader, daemon=True).start()

def send(t, payload):
    a.sendall(struct.pack("<BI", t, len(payload)) + payload)
def u32s(*v):
    return b"".join(struct.pack("<I", x & 0xffffffff) for x in v)
def key(code, down):
    send(1, u32s(code, int(down))); time.sleep(0.03)
def move(x, y):
    send(2, u32s(int(x * 32767 / max(mode[0] - 1, 1)), int(y * 32767 / max(mode[1] - 1, 1)))); time.sleep(0.05)
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
                elif c == "key":
                    codes = [int(k) for k in arg.split("+")]
                    for k in codes: key(k, True)
                    for k in reversed(codes): key(k, False)
                elif c == "type": type_text(arg.replace("\\n", "\n"))
                elif c == "shot": p.stdin.write(b"\x01g"); p.stdin.flush()
                elif c == "resize": send(5, u32s(*map(int, arg.split())))
                elif c == "focus": send(6, bytes([int(arg)]))
                elif c == "clip": send(7, arg.encode())
                elif c == "cmd": send(8, bytes([int(arg)]))
                elif c == "serial": p.stdin.write(arg.replace("\\n", "\r").encode()); p.stdin.flush()
                elif c == "quit": p.stdin.write(b"\x01x"); p.stdin.flush()
            except Exception as e:
                ev.write(f"error {line.strip()}: {e}\n")
p.wait()
