"""Acceptance fixtures for the read-only Linux guest report; no host changes.

Run: python3 -m unittest discover -s scripts/tests -p test_guest_readiness.py
Windows checks still require an elevated Windows guest; these fixtures don't
validate PowerShell or prove Secure Boot enforcement on either platform.
"""

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "guest-readiness.sh"
SOURCE = SCRIPT.read_text().split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
MODULE = {"__name__": "guest_readiness_fixture"}
exec(compile(SOURCE, str(SCRIPT), "exec"), MODULE)


class GuestReadinessTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def report(self, machine="aarch64", expected="arm64", strict=True, system="Linux"):
        report, code = MODULE["inspect_guest"](
            self.root, system, machine, expected, strict, strict, strict
        )
        # Every observation must be JSON-serializable, including unknown states.
        json.dumps(report)
        return report, code

    def firmware(self, secure=1, setup=0):
        efi = self.root / "sys/firmware/efi/efivars"
        efi.mkdir(parents=True, exist_ok=True)
        guid = "-8be4df61-93ca-11d2-aa0d-00e098032b8c"
        for name, value in (("SecureBoot", secure), ("SetupMode", setup)):
            (efi / (name + guid)).write_bytes(bytes((7, 0, 0, 0, value)))

    def tpm(self, version="2"):
        tpm = self.root / "sys/class/tpm/tpm0"
        tpm.mkdir(parents=True, exist_ok=True)
        (tpm / "tpm_version_major").write_text(version + "\n")

    def test_direct_kernel_boot_only_passes_without_uefi_requirement(self):
        report, code = self.report(strict=False)
        self.assertEqual(code, 0)
        self.assertFalse(report["uefi"])
        report, code = self.report()
        self.assertEqual(code, 1)
        self.assertFalse(report["checks"]["uefi"])

    def test_required_features_pass_on_both_native_architectures(self):
        self.firmware()
        self.tpm()
        for machine, expected in (("aarch64", "arm64"), ("x86_64", "x64")):
            with self.subTest(architecture=expected):
                report, code = self.report(machine=machine, expected=expected)
                self.assertEqual(code, 0)
                self.assertEqual(report["status"], "pass")

    def test_missing_efivars_is_unknown(self):
        (self.root / "sys/firmware/efi").mkdir(parents=True)
        self.tpm()
        report, code = self.report()
        self.assertEqual(code, 2)
        self.assertIsNone(report["checks"]["secure_boot_enabled"])

    def test_unprivileged_efivars_is_unknown(self):
        self.firmware()
        self.tpm()
        with patch.object(Path, "read_bytes", side_effect=PermissionError("permission denied")):
            report, code = self.report()
        self.assertEqual(code, 2)
        self.assertIsNone(report["secure_boot_enabled"])
        self.assertTrue(report["errors"])

    def test_malformed_efivar_is_unknown(self):
        self.firmware(secure=2)
        self.tpm()
        report, code = self.report()
        self.assertEqual(code, 2)
        self.assertIsNone(report["checks"]["secure_boot_enabled"])

    def test_setup_mode_never_passes_secure_boot(self):
        self.firmware(setup=1)
        self.tpm()
        report, code = self.report()
        self.assertEqual(code, 1)
        self.assertFalse(report["checks"]["secure_boot_enabled"])

    def test_secure_boot_disabled_fails(self):
        self.firmware(secure=0)
        self.tpm()
        report, code = self.report()
        self.assertEqual(code, 1)
        self.assertFalse(report["checks"]["secure_boot_enabled"])

    def test_tpm_absent_and_tpm1_fail(self):
        self.firmware()
        report, code = self.report()
        self.assertEqual(code, 1)
        self.assertFalse(report["checks"]["tpm2_present"])
        self.tpm("1")
        report, code = self.report()
        self.assertEqual(code, 1)
        self.assertFalse(report["checks"]["tpm2_present"])

    def test_unknown_tpm_version_does_not_pass(self):
        self.firmware()
        self.tpm("not-a-version")
        report, code = self.report()
        self.assertEqual(code, 2)
        self.assertIsNone(report["checks"]["tpm2_present"])

    def test_wrong_architecture_fails(self):
        report, code = self.report(expected="x64", strict=False)
        self.assertEqual(code, 1)
        self.assertFalse(report["checks"]["architecture_matches"])

    def test_non_linux_host_fails(self):
        report, code = self.report(strict=False, system="Darwin")
        self.assertEqual(code, 1)
        self.assertFalse(report["checks"]["linux"])


if __name__ == "__main__":
    unittest.main()
