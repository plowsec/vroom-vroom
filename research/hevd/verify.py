#!/usr/bin/env python3
"""Check an HEVD source checkout and vroom harness manifest."""

import argparse
import json
import re
from pathlib import Path


REQUIRED_DETECTORS = {
    "access-violation",
    "bugcheck",
    "pool-corruption",
    "double-free",
    "use-after-free",
    "out-of-bounds",
    "integer-overflow",
    "uninitialized",
    "timeout",
}


def ioctl_surface(header: Path) -> dict[str, int]:
    pattern = re.compile(
        r"^#define\s+(HEVD_IOCTL_[A-Z0-9_]+)\s+IOCTL\((0x[0-9a-fA-F]+|[0-9]+)\)"
    )
    found = {}
    for line in header.read_text(encoding="utf-8").splitlines():
        match = pattern.match(line.strip())
        if match:
            found[match.group(1)] = int(match.group(2), 0)
    if len(found) < 20:
        raise ValueError(f"expected the HEVD Windows IOCTL surface, found {len(found)}")
    if len(found) != len(set(found.values())):
        raise ValueError("HEVD IOCTL values are not unique")
    return found


def verify(header: Path, harness_path: Path) -> dict:
    ioctls = ioctl_surface(header)
    harness = json.loads(harness_path.read_text(encoding="utf-8"))
    if not harness.get("source_revision"):
        raise ValueError("HEVD harness must record the source revision")
    required = {
        "x1": harness.get("ioctl_register"),
        "x2": harness.get("input_register"),
        "x3": harness.get("input_length_register"),
    }
    if not harness.get("ioctl_prefix"):
        raise ValueError("HEVD harness must fuzz the four-byte IOCTL prefix")
    if set(required.values()) != set(required):
        raise ValueError("HEVD harness must use x1, x2 and x3 for the ARM64 call")
    if harness.get("input_va") in (None, 0) or harness.get("stop_pc") in (None, 0):
        raise ValueError("replace input_va and stop_pc with addresses from the snapshot")
    if not harness.get("reset_is_bugcheck"):
        raise ValueError("HEVD harness must classify a guest reset as a bugcheck")
    missing = REQUIRED_DETECTORS - set(harness.get("detectors", []))
    if missing:
        raise ValueError(f"missing detectors: {', '.join(sorted(missing))}")
    return {
        "ioctls": len(ioctls),
        "first": min(ioctls.values()),
        "last": max(ioctls.values()),
        "coverage_blocks": len(harness.get("coverage", [])),
        "detectors": len(harness.get("detectors", [])),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--header", type=Path, required=True)
    parser.add_argument("--harness", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(verify(args.header, args.harness), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
