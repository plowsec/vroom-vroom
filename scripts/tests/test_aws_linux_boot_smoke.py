"""Offline acceptance checks for the Linux reference; no QEMU/KVM execution."""

import json
import os
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "aws-linux-boot-smoke.sh"
SOURCE = SCRIPT.read_text().split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
MODULE = {"__name__": "linux_boot_fixture"}
exec(compile(SOURCE, str(SCRIPT), "exec"), MODULE)
MARKER = "VROOM_LINUX_BOOT_OK:0123456789abcdef"
KVM = {"enabled": True, "present": True}
SHUTDOWN = {"event": "SHUTDOWN", "data": {"guest": True, "reason": "guest-shutdown"}}


class LinuxBootTests(unittest.TestCase):
    def accepts(self, kvm=KVM, output=None, events=None, returncode=0):
        return MODULE["accepted"](kvm, output if output is not None else (MARKER + "\n").encode(),
                                  MARKER, events if events is not None else [SHUTDOWN], returncode)

    def test_requires_active_acceleration(self):
        self.assertTrue(self.accepts())
        for value in ({}, {"enabled": False, "present": True}, {"enabled": 1, "present": True}):
            self.assertFalse(self.accepts(kvm=value))

    def test_kernel_commandline_echo_is_not_pid1_marker(self):
        self.assertFalse(self.accepts(output=("Kernel command line: " + MARKER + "\n").encode()))
        self.assertFalse(self.accepts(output=b"Linux version 6.1\n"))

    def test_reset_panic_host_quit_and_nonzero_exit_cannot_pass(self):
        for reason in ("guest-reset", "guest-panic", "host-qmp-quit"):
            self.assertFalse(self.accepts(events=[{"event": "SHUTDOWN", "data": {"guest": True, "reason": reason}}]))
        self.assertFalse(self.accepts(events=[]))
        self.assertFalse(self.accepts(events=[{"event": "SHUTDOWN", "data": {"guest": False, "reason": "guest-shutdown"}}]))
        self.assertFalse(self.accepts(returncode=1))

    def test_command_has_no_fallback_network_disk_or_host_mount(self):
        command = MODULE["command"]("/usr/bin/qemu-system-x86_64", "/boot/kernel", "/tmp/ramfs", "/tmp/qmp")
        self.assertEqual(command[command.index("-accel") + 1], "kvm")
        self.assertEqual(command[command.index("-nic") + 1], "none")
        self.assertEqual(command[command.index("-machine") + 1], "q35")
        self.assertIn("-nodefaults", command)
        self.assertIn("-S", command)
        for forbidden in ("-drive", "-blockdev", "-hda", "-netdev", "-virtfs", "-fsdev", "-daemonize"):
            self.assertNotIn(forbidden, command)
        self.assertNotIn(MARKER, " ".join(command))

    def test_init_is_static_64_bit_elf_with_marker_in_its_load_segment(self):
        data = MODULE["init_elf"](MARKER)
        header = struct.unpack_from("<16sHHIQQQIHHHHHH", data)
        self.assertEqual(header[0][:7], b"\x7fELF\x02\x01\x01")
        self.assertEqual(header[2], 62)  # EM_X86_64
        self.assertEqual(header[10], 1)  # exactly one program header, no interpreter
        segment = struct.unpack_from("<IIQQQQQQ", data, header[5])
        self.assertEqual(segment[0], 1)  # PT_LOAD
        self.assertEqual(segment[5], len(data))
        self.assertLess(header[4] - segment[3], segment[5])
        marker_address = struct.unpack_from("<Q", data, 120 + 12)[0] - segment[3]
        self.assertEqual(data[marker_address:], ("\n" + MARKER + "\n").encode())

    def test_archive_contains_no_host_files_and_has_a_guest_console(self):
        archive = MODULE["initramfs"](MARKER)
        offset, files = 0, {}
        while offset < len(archive):
            self.assertEqual(archive[offset:offset + 6], b"070701")
            fields = [int(archive[offset + 6 + 8 * i:offset + 14 + 8 * i], 16) for i in range(13)]
            name_end = offset + 110 + fields[11]
            name = archive[offset + 110:name_end - 1].decode()
            data_start = (name_end + 3) & ~3
            data_end = data_start + fields[6]
            files[name] = (fields, archive[data_start:data_end])
            offset = (data_end + 3) & ~3
        self.assertEqual(set(files), {".", "dev", "dev/console", "init", "TRAILER!!!"})
        self.assertEqual(files["dev/console"][0][9:11], [5, 1])
        self.assertEqual(files["init"][1], MODULE["init_elf"](MARKER))

    def test_wrong_host_never_starts_a_process(self):
        with patch.object(MODULE["platform"], "system", return_value="Darwin"), \
             patch.object(MODULE["subprocess"], "Popen") as popen:
            report, code = MODULE["run_boot"]()
        self.assertEqual(code, 1)
        self.assertFalse(report["os_boot_verified"])
        popen.assert_not_called()

    def test_outer_timeout_discards_early_output(self):
        with tempfile.TemporaryDirectory() as directory:
            timeout = Path(directory) / "timeout"
            timeout.write_text("#!/bin/sh\nprintf '%s\\n' '{\"status\":\"pass\"}'\nexit 124\n")
            timeout.chmod(0o700)
            env = dict(os.environ, PATH=directory + os.pathsep + os.environ.get("PATH", ""))
            done = subprocess.run(["sh", str(SCRIPT)], env=env, capture_output=True, text=True, timeout=5)
        self.assertEqual(done.returncode, 2)
        self.assertEqual(json.loads(done.stdout)["status"], "unknown")
        self.assertFalse(json.loads(done.stdout)["os_boot_verified"])


if __name__ == "__main__":
    unittest.main()
