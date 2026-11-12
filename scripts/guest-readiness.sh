#!/bin/sh
# Run inside a Linux guest. Read-only JSON report; needs Python 3.
#   sh guest-readiness.sh --expected-arch arm64 --require-uefi
#   sudo sh guest-readiness.sh --require-secure-boot --require-tpm2
# Exit 0: requested observations pass; 1: unmet; 2: unable to verify.
set -eu
exec python3 - "$@" <<'PY'
import argparse
import json
import platform
import sys
from pathlib import Path


def inspect_guest(root, system, machine, expected, require_uefi, require_secure_boot, require_tpm2):
    errors = []
    architecture = {"x86_64": "x64", "amd64": "x64", "aarch64": "arm64", "arm64": "arm64"}.get(machine.lower(), "unsupported")
    efi = root / "sys/firmware/efi"
    uefi = efi.is_dir()

    def variable(name):
        path = efi / "efivars" / (name + "-8be4df61-93ca-11d2-aa0d-00e098032b8c")
        try:
            data = path.read_bytes()
            # efivarfs prepends a four-byte attribute word to the one-byte value.
            if len(data) != 5 or data[4] not in (0, 1):
                raise ValueError("invalid EFI boolean variable")
            return bool(data[4])
        except (OSError, ValueError) as exc:
            errors.append({"check": name, "message": str(exc)})
            return None

    secure_boot = variable("SecureBoot") if uefi else False
    setup_mode = variable("SetupMode") if uefi else None
    # Missing/inaccessible variables must never look like a verified secure boot.
    secure_boot_active = (
        None if uefi and (secure_boot is None or setup_mode is None)
        else bool(secure_boot and not setup_mode)
    )
    tpm_devices = []
    tpm_unknown = False
    for device in sorted((root / "sys/class/tpm").glob("tpm[0-9]*")):
        try:
            version = int((device / "tpm_version_major").read_text().strip())
            if version not in (1, 2):
                raise ValueError("unknown TPM major version")
        except (OSError, ValueError) as exc:
            errors.append({"check": device.name, "message": str(exc)})
            version = None
            tpm_unknown = True
        tpm_devices.append({"device": device.name, "version_major": version})
    tpm2 = any(device["version_major"] == 2 for device in tpm_devices)
    if not tpm2 and tpm_unknown:
        tpm2 = None

    checks = {
        "linux": system == "Linux",
        "architecture_supported": architecture in ("x64", "arm64"),
        "architecture_matches": expected == "any" or architecture == expected,
    }
    if require_uefi or require_secure_boot:
        checks["uefi"] = uefi
    if require_secure_boot:
        checks["secure_boot_enabled"] = secure_boot_active
    if require_tpm2:
        checks["tpm2_present"] = tpm2
    unknown = any(value is None for value in checks.values())
    failed = any(value is False for value in checks.values())
    status = "unknown" if unknown else "fail" if failed else "pass"
    return {
        "schema_version": 1,
        "platform": system.lower(),
        "evidence_scope": "Guest-reported configuration; firmware variable protection and boot enforcement require separate validation.",
        "expected_architecture": expected,
        "architecture": architecture,
        "uefi": uefi,
        "secure_boot_enabled": secure_boot,
        "setup_mode": setup_mode,
        "tpm_devices": tpm_devices,
        "requirements": {"uefi": require_uefi or require_secure_boot, "secure_boot": require_secure_boot, "tpm2": require_tpm2},
        "checks": checks,
        "errors": errors,
        "status": status,
    }, 2 if unknown else 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description="Read-only Linux guest feature report (no configuration changes).")
    parser.add_argument("--expected-arch", choices=("any", "x64", "arm64"), default="any")
    parser.add_argument("--require-uefi", action="store_true")
    parser.add_argument("--require-secure-boot", action="store_true")
    parser.add_argument("--require-tpm2", action="store_true")
    args = parser.parse_args()
    report, code = inspect_guest(Path("/"), platform.system(), platform.machine(), args.expected_arch,
                                 args.require_uefi, args.require_secure_boot, args.require_tpm2)
    print(json.dumps(report, indent=2))
    return code


if __name__ == "__main__":
    sys.exit(main())
PY
