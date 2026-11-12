"""Offline archive/ELF checks; bounded execution only on the matching Linux ABI."""
import hashlib
import importlib.util
import json
import platform
import stat
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "make-linux-heartbeat.py"
SPEC = importlib.util.spec_from_file_location("heartbeat", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
TOKEN = "0123456789abcdef0123456789abcdef"


def entries(archive):
    result, offset = {}, 0
    while offset < len(archive):
        assert archive[offset:offset + 6] == b"070701"
        fields = [int(archive[offset + 6 + 8*i:offset + 14 + 8*i], 16) for i in range(13)]
        start = offset + 110
        name = archive[start:start + fields[11]]
        assert name[-1:] == b"\0"
        start = (start + fields[11] + 3) & ~3
        contents = archive[start:start + fields[6]]
        assert len(contents) == fields[6]
        result[name[:-1].decode()] = (fields, contents)
        offset = (start + fields[6] + 3) & ~3
    return result


class HeartbeatTests(unittest.TestCase):
    def test_x64_archive_bytes_are_unchanged(self):
        self.assertEqual(hashlib.sha256(MODULE.initramfs(TOKEN)).hexdigest(),
                         'e9808d3db06e1921c2bbb7fa16709011d512587a0f049321239310d8bf69d3be')

    def test_arm64_elf_and_archive_support_every_arm_kernel_page_size(self):
        archive = MODULE.initramfs(TOKEN, 'arm64')
        self.assertEqual(archive, MODULE.initramfs(TOKEN, 'arm64'))
        self.assertLessEqual(len(archive), MODULE.ARM_MAX_ARCHIVE_SIZE)
        elf = entries(archive)['init'][1]
        header = struct.unpack_from('<16sHHIQQQIHHHHHH', elf)
        self.assertEqual(header[1:4], (2, 183, 1))
        segments = [struct.unpack_from('<IIQQQQQQ', elf, header[5] + i * header[9]) for i in range(header[10])]
        self.assertEqual([p[0] for p in segments], [1, 1, 0x6474e551])
        self.assertEqual([p[1] for p in segments], [5, 6, 6])
        self.assertEqual([p[7] for p in segments[:2]], [65536, 65536])
        for page in [4096, 16384, 65536]:
            self.assertEqual(segments[1][2] % page, 0)
            self.assertEqual(segments[1][3] % page, 0)
            self.assertLessEqual((segments[0][3] + segments[0][6] + page - 1) // page * page, segments[1][3])
        self.assertIn(struct.pack('<QQ', 0, 100_000_000), elf)
        # Every PC-relative data address lands within the RW segment.
        start = header[4] - segments[0][3]
        code = list(struct.unpack('<' + 'I' * ((segments[0][5] - start) // 4), elf[start:segments[0][5]]))
        targets = []
        for i, word in enumerate(code):
            if word & 0x9f000000 == 0x10000000:  # ADR, signed21 byte offset
                offset = ((word >> 29) & 3) | (((word >> 5) & 0x7ffff) << 2)
                if offset & (1 << 20): offset -= 1 << 21
                address = header[4] + i * 4 + offset
                self.assertGreaterEqual(address, segments[1][3])
                self.assertLess(address, segments[1][3] + segments[1][6])
                targets.append(address)
        self.assertEqual(len(targets), 5)
        self.assertEqual(targets[3], targets[4])  # interrupted nanosleep reuses remaining

    def test_unknown_architecture_is_rejected(self):
        for architecture in ['aarch64', 'x86', None]:
            with self.assertRaises(ValueError):
                MODULE.initramfs(TOKEN, architecture)

    def test_minimal_archive_and_determinism(self):
        archive = MODULE.initramfs(TOKEN)
        self.assertEqual(archive, MODULE.initramfs(TOKEN))
        self.assertLessEqual(len(archive), MODULE.MAX_ARCHIVE_SIZE)
        files = entries(archive)
        self.assertEqual(list(files), [".", "dev", "dev/console", "init", "TRAILER!!!"])
        console = files["dev/console"][0]
        self.assertTrue(stat.S_ISCHR(console[1]))
        self.assertEqual(console[9:11], [5, 1])
        self.assertEqual(files["init"][0][1], stat.S_IFREG | 0o555)
        self.assertEqual(files["init"][1], MODULE.init_elf(TOKEN))
        self.assertNotEqual(archive, MODULE.initramfs("f" * 32))

    def test_static_elf_permissions_and_entry(self):
        elf = MODULE.init_elf(TOKEN)
        header = struct.unpack_from("<16sHHIQQQIHHHHHH", elf)
        self.assertEqual(header[0][:7], b"\x7fELF\x02\x01\x01")
        self.assertEqual(header[1:4], (2, 62, 1))
        self.assertEqual(header[10], 3)
        segments = [struct.unpack_from("<IIQQQQQQ", elf, header[5] + i * header[9]) for i in range(header[10])]
        self.assertEqual([p[0] for p in segments], [1, 1, 0x6474e551])
        self.assertEqual([p[1] for p in segments], [5, 6, 6])
        self.assertGreaterEqual(header[4], segments[0][3])
        self.assertLess(header[4], segments[0][3] + segments[0][6])
        for p in segments[:2]:
            self.assertEqual(p[2] % p[7], p[3] % p[7])
            self.assertLessEqual(p[2] + p[5], len(elf))
            self.assertEqual(p[5], p[6])
        self.assertIn(("VROOM_HEARTBEAT:" + TOKEN + ":").encode(), elf)
        self.assertIn(struct.pack("<QQ", 0, 100_000_000), elf)

    def test_invalid_tokens_rejected(self):
        for token in ["", "a" * 31, "a" * 33, "A" * 32, "g" * 32, "0" * 31 + "\n", None]:
            with self.assertRaises(ValueError):
                MODULE.initramfs(token)

    def test_cli_never_overwrites_and_invalid_input_creates_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "ramfs.cpio"
            command = [sys.executable, str(SCRIPT), "--output", str(output), "--token"]
            self.assertNotEqual(subprocess.run(command + ["invalid"], capture_output=True).returncode, 0)
            self.assertFalse(output.exists())
            result = subprocess.run(command + [TOKEN], capture_output=True, check=True)
            self.assertFalse(json.loads(result.stdout)["guest_poweroff"])
            original = output.read_bytes()
            self.assertNotEqual(subprocess.run(command + ["f" * 32], capture_output=True).returncode, 0)
            self.assertEqual(output.read_bytes(), original)

    @unittest.skipUnless(platform.system() == "Linux" and platform.machine() in ("x86_64", "aarch64"), "requires native Linux syscall ABI")
    def test_actual_binary_advances_across_decimal_carry_at_bounded_rate(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "init"
            architecture = 'arm64' if platform.machine() == 'aarch64' else 'x86_64'
            binary.write_bytes(MODULE.init_elf(TOKEN, architecture))
            binary.chmod(0o700)
            process = subprocess.Popen([str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                with self.assertRaises(subprocess.TimeoutExpired):
                    process.communicate(timeout=2)
            finally:
                process.kill()
                stdout, stderr = process.communicate(timeout=2)
            lines = stdout.decode("ascii").splitlines()
            self.assertEqual(stderr, b"")
            self.assertGreaterEqual(len(lines), 11)
            self.assertLessEqual(len(lines), 22)
            self.assertEqual(lines, [f"VROOM_HEARTBEAT:{TOKEN}:{i}" for i in range(len(lines))])


if __name__ == "__main__":
    unittest.main()
