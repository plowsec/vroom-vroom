"""Bounded host reference fixtures; these do not execute KVM or validate AWS."""

import errno
import json
import mmap
import os
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "aws-kvm-smoke.sh"
SOURCE = SCRIPT.read_text().split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
MODULE = {"__name__": "kvm_smoke_fixture"}
exec(compile(SOURCE, str(SCRIPT), "exec"), MODULE)
REAL_MMAP = mmap.mmap


class FakeKvm:
    def __init__(self, api=12, exits=(2, 5), output=42):
        self.api = api
        self.exits = iter(exits)
        self.output = output
        self.run = None
        self.closed = []
        self.vm_created = False

    def mapping(self, fd, length, **kwargs):
        mapping = REAL_MMAP(-1, length)
        if fd != -1:
            self.run = mapping
        return mapping

    def ioctl(self, fd, request, argument=0, mutate=False):
        if request == MODULE["KVM_GET_API_VERSION"]:
            return self.api
        if request == MODULE["KVM_CREATE_VM"]:
            self.vm_created = True
            return 10
        if request == MODULE["KVM_CREATE_VCPU"]:
            return 11
        if request == MODULE["KVM_GET_VCPU_MMAP_SIZE"]:
            return 4096
        if request == MODULE["KVM_RUN"]:
            reason = next(self.exits)
            struct.pack_into("<I", self.run, 8, reason)
            if reason == 2:
                struct.pack_into("<BBHIQ", self.run, 32, 1, 1, 0xE9, 1, 128)
                self.run[128] = self.output
        return 0


class KvmSmokeTests(unittest.TestCase):
    def exercise(self, fake):
        with patch.object(MODULE["os"], "open", return_value=9), \
             patch.object(MODULE["os"], "close", side_effect=fake.closed.append), \
             patch.object(MODULE["fcntl"], "ioctl", side_effect=fake.ioctl), \
             patch.object(MODULE["mmap"], "mmap", side_effect=fake.mapping), \
             patch.object(MODULE["mmap"], "PAGESIZE", 4096):
            report, code = MODULE["run_smoke"]("Linux", "x86_64")
        json.dumps(report)
        return report, code

    def test_supported_api_and_exact_exit_sequence_pass(self):
        fake = FakeKvm()
        report, code = self.exercise(fake)
        self.assertEqual(code, 0)
        self.assertEqual(report["verified_exits"], [2, 5])
        self.assertEqual(report["engine"], "linux_kvm_ioctl")
        self.assertFalse(report["native_vroom_x64_tested"])
        self.assertFalse(report["os_boot_tested"])
        self.assertEqual(fake.closed, [11, 10, 9])
        self.assertTrue(fake.run.closed)

    def test_unsupported_api_never_creates_vm(self):
        fake = FakeKvm(api=11)
        report, code = self.exercise(fake)
        self.assertEqual(code, 1)
        self.assertFalse(fake.vm_created)
        self.assertEqual(report["stage"], "api_version")
        self.assertEqual(fake.closed, [9])

    def test_halt_without_output_cannot_pass(self):
        report, code = self.exercise(FakeKvm(exits=(5,)))
        self.assertEqual(code, 1)
        self.assertEqual(report["verified_exits"], [])

    def test_wrong_instruction_result_fails(self):
        fake = FakeKvm(output=41)
        report, code = self.exercise(fake)
        self.assertEqual(code, 1)
        self.assertNotIn("observed_byte", report)
        self.assertEqual(fake.closed, [11, 10, 9])

    def test_output_without_halt_cannot_pass(self):
        report, code = self.exercise(FakeKvm(exits=(2, 8)))
        self.assertEqual(code, 1)
        self.assertEqual(report["verified_exits"], [2])

    def test_out_of_bounds_io_offset_fails(self):
        run = bytearray(4096)
        struct.pack_into("<I", run, 8, 2)
        struct.pack_into("<BBHIQ", run, 32, 1, 1, 0xE9, 1, 4096)
        with self.assertRaises(MODULE["VerificationFailed"]):
            MODULE["verify_exit"](run, 2)

    def test_unexpected_io_operation_fails(self):
        for direction, size, port, count in ((0, 1, 0xE9, 1), (1, 2, 0xE9, 1), (1, 1, 0xEA, 1), (1, 1, 0xE9, 2)):
            with self.subTest(operation=(direction, size, port, count)):
                run = bytearray(4096)
                struct.pack_into("<I", run, 8, 2)
                struct.pack_into("<BBHIQ", run, 32, direction, size, port, count, 128)
                run[128] = 42
                with self.assertRaises(MODULE["VerificationFailed"]):
                    MODULE["verify_exit"](run, 2)

    def test_wrong_host_never_opens_kvm(self):
        for system, machine in (("Darwin", "arm64"), ("Linux", "aarch64")):
            with self.subTest(host=(system, machine)), patch.object(MODULE["os"], "open") as opener:
                report, code = MODULE["run_smoke"](system, machine)
                self.assertEqual(code, 1)
                opener.assert_not_called()
                self.assertFalse(report["vm_created"])

    def test_kvm_permission_failure_is_reported_without_fallback(self):
        with patch.object(MODULE["os"], "open", side_effect=PermissionError(errno.EACCES, "denied")), \
             patch.object(MODULE["fcntl"], "ioctl") as ioctl, \
             patch.object(MODULE["mmap"], "PAGESIZE", 4096):
            report, code = MODULE["run_smoke"]("Linux", "x86_64")
        self.assertEqual(code, 1)
        self.assertEqual(report["errno"], errno.EACCES)
        self.assertEqual(report["stage"], "open_kvm")
        ioctl.assert_not_called()

    def test_outer_deadline_returns_one_unknown_json_result(self):
        # Simulate timeout's documented exit statuses; never invoke /dev/kvm.
        for timeout_status in (124, 137):
            with self.subTest(exit_status=timeout_status), tempfile.TemporaryDirectory() as directory:
                timeout = Path(directory) / "timeout"
                # Discard even a complete-looking early report on timeout.
                timeout.write_text("#!/bin/sh\nprintf '%s\\n' '{\"status\":\"pass\"}'\nexit " + str(timeout_status) + "\n")
                timeout.chmod(0o700)
                env = dict(os.environ, PATH=directory + os.pathsep + os.environ.get("PATH", ""))
                completed = subprocess.run(["sh", str(SCRIPT)], env=env, text=True, capture_output=True, timeout=5)
                self.assertEqual(completed.returncode, 2)
                report = json.loads(completed.stdout)
                self.assertEqual(report["status"], "unknown")
                self.assertFalse(report["native_vroom_x64_tested"])

    def test_arguments_cannot_expand_smoke_scope(self):
        completed = subprocess.run(["sh", str(SCRIPT), "--unbounded"], text=True, capture_output=True, timeout=5)
        self.assertEqual(completed.returncode, 2)
        self.assertEqual(json.loads(completed.stdout)["status"], "unknown")


if __name__ == "__main__":
    unittest.main()
