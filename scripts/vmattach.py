"""Attach to a running vroom VM's viewer socket.

Usage: vmattach.py STATE_DIR

Commands are read from STATE_DIR.attach.cmd (a FIFO):
  shot [PATH] | click X Y | dclick X Y | rclick X Y | move X Y
  key CODE[+CODE...] | type TEXT | alt N | clip64 BASE64
  focus 0|1 | resize W H | cmd N | quit

The framebuffer is written to STATE_DIR.attach.png by default and viewer
events are appended to STATE_DIR.attach.events.
"""

import base64
import mmap
import os
import select
import socket
import struct
import sys
import threading
import time
import zlib


state = sys.argv[1]
cmdp = state + ".attach.cmd"
evp = state + ".attach.events"
default_shot = state + ".attach.png"
if not os.path.exists(cmdp):
    os.mkfifo(cmdp)

with open(os.path.join(state, "viewer"), encoding="utf-8") as f:
    viewer = f.read().strip()

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(viewer)
_, fds, _, _ = socket.recv_fds(s, 1, 1)
if not fds:
    raise RuntimeError("viewer did not send a framebuffer")
fbfd = fds[0]
fb = mmap.mmap(fbfd, 7680 * 4320 * 4, access=mmap.ACCESS_READ)
os.close(fbfd)

mode = [0, 0]
ev = open(evp, "a", buffering=1, encoding="utf-8")


def write_png(path, width, height):
    raw = fb[: width * height * 4]
    rgb = bytearray(width * height * 3)
    rgb[0::3] = raw[2::4]
    rgb[1::3] = raw[1::4]
    rgb[2::3] = raw[0::4]
    stride = width * 3
    rows = b"".join(b"\0" + rgb[y * stride : (y + 1) * stride] for y in range(height))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    with open(path, "wb") as out:
        out.write(
            b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows))
            + chunk(b"IEND", b"")
        )


def reader():
    buf = b""
    while True:
        data = s.recv(65536)
        if not data:
            ev.write("closed\n")
            return
        buf += data
        while len(buf) >= 5:
            kind, size = struct.unpack("<BI", buf[:5])
            if len(buf) < 5 + size:
                break
            payload, buf = buf[5 : 5 + size], buf[5 + size :]
            if kind == 1:
                mode[:] = struct.unpack("<II", payload)
                ev.write(f"mode {mode[0]}x{mode[1]}\n")
            elif kind == 3:
                ev.write("clipboard " + payload.decode(errors="replace").replace("\n", "\\n") + "\n")
            elif kind == 4:
                ev.write("status " + payload.decode(errors="replace") + "\n")
            elif kind == 5:
                ev.write("closed\n")
                return


threading.Thread(target=reader, daemon=True).start()


def send(kind, payload):
    s.sendall(struct.pack("<BI", kind, len(payload)) + payload)


def u32s(*values):
    return b"".join(struct.pack("<I", value & 0xFFFFFFFF) for value in values)


def key(code, down):
    send(1, u32s(code, int(down)))
    time.sleep(0.02)


def move(x, y):
    width, height = mode
    send(2, u32s(int(x * 32767 / max(width - 1, 1)), int(y * 32767 / max(height - 1, 1))))
    time.sleep(0.03)


def button(code, down):
    send(3, u32s(code, int(down)))
    time.sleep(0.03)


def click(x, y, btn=0x110, count=1):
    move(x, y)
    for _ in range(count):
        button(btn, True)
        button(btn, False)


rows = {"1234567890-=": 2, "qwertyuiop[]": 16, "asdfghjkl;'": 30, "zxcvbnm,./": 44}
shift_rows = {"!@#$%^&*()_+": 2, "QWERTYUIOP{}": 16, 'ASDFGHJKL:"': 30, "ZXCVBNM<>?": 44}
keys = {
    " ": (57, False),
    "\n": (28, False),
    "\t": (15, False),
    "`": (41, False),
    "~": (41, True),
    "\\": (43, False),
    "|": (43, True),
}
for table, shifted in ((rows, False), (shift_rows, True)):
    for row, base in table.items():
        for index, char in enumerate(row):
            keys[char] = (base + index, shifted)


def type_text(text):
    for char in text:
        code, shifted = keys[char]
        if shifted:
            key(42, True)
        key(code, True)
        key(code, False)
        if shifted:
            key(42, False)


keypad = {"0": 82, "1": 79, "2": 80, "3": 81, "4": 75, "5": 76, "6": 77, "7": 71, "8": 72, "9": 73}


def alt_code(value):
    key(56, True)
    for digit in str(value):
        key(keypad[digit], True)
        key(keypad[digit], False)
    key(56, False)


send(6, b"\x01")
ev.write(f"attached {viewer}\n")

fd = os.open(cmdp, os.O_RDONLY | os.O_NONBLOCK)
buf = b""
while True:
    ready, _, _ = select.select([fd], [], [], 0.5)
    if not ready:
        continue
    data = os.read(fd, 65536)
    if not data:
        os.close(fd)
        fd = os.open(cmdp, os.O_RDONLY | os.O_NONBLOCK)
        continue
    buf += data
    while b"\n" in buf:
        raw, buf = buf.split(b"\n", 1)
        line = raw.decode()
        command, _, arg = line.partition(" ")
        try:
            if command in ("click", "dclick", "rclick", "move"):
                x, y = map(int, arg.split())
                if command == "move":
                    move(x, y)
                else:
                    click(x, y, 0x111 if command == "rclick" else 0x110, 2 if command == "dclick" else 1)
            elif command == "key":
                codes = [int(value) for value in arg.split("+")]
                for code in codes:
                    key(code, True)
                for code in reversed(codes):
                    key(code, False)
            elif command == "type":
                type_text(arg.replace("\\n", "\n"))
            elif command == "alt":
                alt_code(int(arg))
            elif command == "clip64":
                send(7, base64.b64decode(arg))
            elif command == "shot":
                while mode == [0, 0]:
                    time.sleep(0.05)
                path = arg or default_shot
                write_png(path, *mode)
                ev.write(f"shot {path} {mode[0]}x{mode[1]}\n")
            elif command == "resize":
                send(5, u32s(*map(int, arg.split()), 0))
            elif command == "focus":
                send(6, bytes([int(arg)]))
            elif command == "cmd":
                send(8, bytes([int(arg)]))
            elif command == "quit":
                s.close()
                sys.exit(0)
        except Exception as exc:
            ev.write(f"error {line}: {exc}\n")
