#!/usr/bin/env python3
"""Generate a static x64/ARM64 Linux PID1 heartbeat/newc initramfs, without a compiler.

PID1 writes VROOM_HEARTBEAT:<token>:<sequence> starting at zero, then sleeps
100 ms. No reboot, files, network, interpreter or libraries. Caller supplies
a fresh token; identical inputs produce identical bytes.
"""
import argparse
import hashlib
import json
import re
import stat
import struct
from pathlib import Path

MAX_ARCHIVE_SIZE = 16 * 1024
ARM_MAX_ARCHIVE_SIZE = 128 * 1024


def validate_token(token):
    if not isinstance(token, str) or re.fullmatch(r"[0-9a-f]{32}", token) is None:
        raise ValueError("token must contain exactly 32 lowercase hexadecimal characters")
    return token


def arm64_init_elf(token):
    """AArch64 syscall-only ELF; separate 64KiB RX/RW mappings suit all ARM page sizes."""
    prefix = ("VROOM_HEARTBEAT:" + validate_token(token) + ":").encode("ascii")
    base, entry_offset, data_offset = 0x400000, 64 + 3 * 56, 65536
    data = bytearray(prefix)
    data += bytes(-len(data) % 16)
    buffer_offset = len(data)
    data += bytes(len(prefix) + 20 + 1)
    data += bytes(-len(data) % 8)
    delay_offset = len(data)
    data += struct.pack("<QQ", 0, 100_000_000)
    remaining_offset = len(data)
    data += bytes(16)
    # x19=sequence, x20=line_end, x21=line_start. Decimal conversion uses
    # udiv/msub, then copies the fixed 49-byte prefix before the digits.
    words = [
        0xd2800013, 0x10000014, 0x52800149, 0x39000289, 0xaa1403f5, 0xaa1303f6, 0xd2800157,
        0x9ad70ad8, 0x9b17db19, 0xd10006b5, 0x1100c339, 0x390002b9, 0xaa1803f6, 0xb5ffff56,
        0xd100c6b5, 0x10000009, 0xaa1503ea, 0xd280062b, 0x3840152c, 0x3800154c, 0xf100056b,
        0x54ffffa1, 0xaa1503fa, 0xcb15029b, 0x9100077b,
        # write(1, x26, x27), syscall64; EINTR retries, partial writes advance.
        0xd2800020, 0xaa1a03e1, 0xaa1b03e2, 0xd2800808, 0xd4000001, 0xb100101f, 0x54ffff40,
        0xf100001f, 0x5400020d, 0x8b00035a, 0xeb00037b, 0x54fffea1, 0x1000001a,
        # nanosleep(request, remaining), syscall101; EINTR uses remaining time.
        0xaa1a03e0, 0x10000001, 0xd2800ca8, 0xd4000001, 0xb40000a0, 0xb100101f, 0x540000a1,
        0x1000001a, 0x17fffff8, 0xb1000673, 0x54fffa41,
        # Wrap, zero/failed write or failed sleep: exit(2), never reboot.
        0xd2800040, 0xd2800ba8, 0xd4000001, 0xd4200000,
    ]
    targets = {1: data_offset + buffer_offset + len(prefix) + 20, 15: data_offset,
               37: data_offset + delay_offset, 39: data_offset + remaining_offset,
               45: data_offset + remaining_offset}
    for index, target in targets.items():
        offset = target - (entry_offset + index * 4)
        if not -(1 << 20) <= offset < (1 << 20):
            raise ValueError("ARM heartbeat address exceeds ADR range")
        words[index] |= ((offset & 3) << 29) | (((offset >> 2) & 0x7ffff) << 5)
    code = struct.pack('<' + 'I' * len(words), *words)
    code_end = entry_offset + len(code)
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    header = struct.pack("<16sHHIQQQIHHHHHH", ident, 2, 183, 1, base + entry_offset,
                         64, 0, 0, 64, 56, 3, 0, 0, 0)
    code_segment = struct.pack("<IIQQQQQQ", 1, 5, 0, base, base, code_end, code_end, 65536)
    data_segment = struct.pack("<IIQQQQQQ", 1, 6, data_offset, base + data_offset,
                               base + data_offset, len(data), len(data), 65536)
    stack = struct.pack("<IIQQQQQQ", 0x6474e551, 6, 0, 0, 0, 0, 0, 16)
    return header + code_segment + data_segment + stack + code + bytes(data_offset - code_end) + data


def init_elf(token, architecture="x86_64"):
    """Static ELF64: RX code, RW data, non-executable stack; syscall ABI only."""
    if architecture == "arm64":
        return arm64_init_elf(token)
    if architecture != "x86_64":
        raise ValueError("unsupported heartbeat architecture")
    prefix = ("VROOM_HEARTBEAT:" + validate_token(token) + ":").encode("ascii")
    base, entry_offset, data_offset = 0x400000, 64 + 3 * 56, 4096
    data = bytearray(prefix)
    data += bytes(-len(data) % 16)
    buffer_offset = len(data)
    data += bytes(len(prefix) + 20 + 1)
    data += bytes(-len(data) % 8)
    delay_offset = len(data)
    data += struct.pack("<QQ", 0, 100_000_000)
    remaining_offset = len(data)
    data += bytes(16)
    labels = {"prefix": data_offset, "line_end": data_offset + buffer_offset + len(prefix) + 20,
              "delay": data_offset + delay_offset, "remaining": data_offset + remaining_offset}
    code, fixes = bytearray(), []

    def emit(hex_bytes):
        code.extend(bytes.fromhex(hex_bytes))

    def label(name):
        labels[name] = entry_offset + len(code)

    def relative(opcode, target):
        emit(opcode)
        fixes.append((len(code), target))
        code.extend(bytes(4))

    emit("fc 45 31 e4")                     # cld; xor r12d,r12d (sequence)
    relative("4c 8d 2d", "line_end")
    label("next")
    emit("41 c6 45 00 0a 4c 89 ee 4c 89 e0")  # newline; rsi=end; rax=sequence
    emit("bb 0a 00 00 00")                  # divisor10
    label("digit")
    emit("31 d2 48 f7 f3 48 ff ce 80 c2 30 88 16 48 85 c0")
    relative("0f 85", "digit")              # prepend decimal remainders
    emit("48 8d 7e")
    code.extend(struct.pack("b", -len(prefix)))
    emit("49 89 fe b9")                     # r14=line_start; ecx=prefix_length
    code.extend(struct.pack("<I", len(prefix)))
    relative("48 8d 35", "prefix")
    emit("f3 a4 4c 89 f6 4c 89 ea 4c 29 f2 48 ff c2")  # copy prefix, set buffer/length
    label("write")
    emit("b8 01 00 00 00 bf 01 00 00 00 0f 05 48 83 f8 fc")
    relative("0f 84", "write")              # write EINTR: retry
    emit("48 85 c0")
    relative("0f 8e", "failed")             # zero/negative write fails
    emit("48 01 c6 48 29 c2")               # advance after partial write
    relative("0f 85", "write")
    relative("48 8d 3d", "delay")
    label("sleep")
    relative("48 8d 35", "remaining")
    emit("b8 23 00 00 00 0f 05 48 85 c0")   # nanosleep(request,remaining)
    relative("0f 84", "slept")
    emit("48 83 f8 fc")
    relative("0f 85", "failed")
    relative("48 8d 3d", "remaining")       # EINTR: sleep remaining time
    relative("e9", "sleep")
    label("slept")
    emit("49 ff c4")                        # increment, reject uint64 wrap
    relative("0f 85", "next")
    label("failed")
    emit("b8 3c 00 00 00 bf 02 00 00 00 0f 05 0f 0b")  # exit(2), never poweroff
    for offset, target in fixes:
        struct.pack_into("<i", code, offset, labels[target] - (entry_offset + offset + 4))
    code_end = entry_offset + len(code)
    if code_end > data_offset:
        raise ValueError("heartbeat code exceeds its reserved page")
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    header = struct.pack("<16sHHIQQQIHHHHHH", ident, 2, 62, 1, base + entry_offset,
                         64, 0, 0, 64, 56, 3, 0, 0, 0)
    code_segment = struct.pack("<IIQQQQQQ", 1, 5, 0, base, base, code_end, code_end, 4096)
    data_segment = struct.pack("<IIQQQQQQ", 1, 6, data_offset, base + data_offset,
                               base + data_offset, len(data), len(data), 4096)
    stack = struct.pack("<IIQQQQQQ", 0x6474e551, 6, 0, 0, 0, 0, 0, 16)
    return header + code_segment + data_segment + stack + code + bytes(data_offset - code_end) + data


def initramfs(token, architecture="x86_64"):
    archive = bytearray()
    entries = [(".", stat.S_IFDIR | 0o755, b"", 0, 0),
               ("dev", stat.S_IFDIR | 0o755, b"", 0, 0),
               ("dev/console", stat.S_IFCHR | 0o600, b"", 5, 1),
               ("init", stat.S_IFREG | 0o555, init_elf(token, architecture), 0, 0),
               ("TRAILER!!!", 0, b"", 0, 0)]
    for inode, (name, mode, contents, major, minor) in enumerate(entries, 1):
        encoded_name = name.encode("ascii") + b"\0"
        fields = (inode, mode, 0, 0, 2 if stat.S_ISDIR(mode) else 1, 0, len(contents),
                  0, 0, major, minor, len(encoded_name), 0)
        archive += b"070701" + b"".join(f"{value:08x}".encode("ascii") for value in fields) + encoded_name
        archive += bytes(-len(archive) % 4)
        archive += contents
        archive += bytes(-len(archive) % 4)
    if len(archive) > (ARM_MAX_ARCHIVE_SIZE if architecture == "arm64" else MAX_ARCHIVE_SIZE):
        raise ValueError("heartbeat initramfs exceeds its footprint limit")
    return bytes(archive)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--architecture", choices=("x86_64", "arm64"), default="x86_64")
    args = parser.parse_args()
    try:
        data = initramfs(args.token, args.architecture)
        # Never overwrite an existing artifact or follow an existing symlink.
        with args.output.open("xb") as output:
            output.write(data)
    except (OSError, ValueError) as error:
        parser.exit(2, f"heartbeat initramfs: {error}\n")
    print(json.dumps({"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
                      "architecture": args.architecture, "sequence_start": 0, "interval_ms": 100, "guest_poweroff": False}))


if __name__ == "__main__":
    main()
