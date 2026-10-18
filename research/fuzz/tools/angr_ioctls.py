#!/usr/bin/env python3
"""Find CTL_CODE values loaded by one ARM64 dispatch function.

angr_tui recovers the function CFG and instruction addresses. This helper
checks ARM64 literal-load references in that CFG and reads their 32-bit values
from CLE's mapped image. Results are candidates for the snapshot probe, not a
replacement for testing them in the guest.
"""

import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path


LITERAL_LOAD = re.compile(r"\bldr\s+[wx]\d+\s*,\s*#?(0x[0-9a-f]+)", re.IGNORECASE)


def integer(text: str) -> int:
    return int(text, 0)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="driver or executable to analyze")
    parser.add_argument("function", help="dispatch function name or mapped address")
    parser.add_argument("--device-type", required=True, type=integer,
                        help="CTL_CODE device type to keep")
    parser.add_argument("--function-start", required=True, type=integer,
                        help="first CTL_CODE function value to keep")
    parser.add_argument("--function-end", required=True, type=integer,
                        help="last CTL_CODE function value to keep")
    parser.add_argument("--output", type=Path, help="write JSON here instead of stdout")
    args = parser.parse_args()

    if args.device_type > 0xffff or not 0 <= args.function_start <= args.function_end <= 0xfff:
        parser.error("device type must fit 16 bits and function range must be within 0..0xfff")
    atui = shutil.which("atui")
    if not atui:
        parser.error("atui not found; install the optional angr extra first")

    result = subprocess.run(
        [atui, str(args.binary), "--pd", args.function, "--json"],
        check=False, capture_output=True, text=True,
    )
    if result.returncode:
        sys.stderr.write(result.stderr or result.stdout)
        return result.returncode or 1
    try:
        cfg = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise SystemExit(f"angr_tui returned invalid CFG JSON: {error}") from error

    # CLE is part of angr and maps PE/ELF images at the same addresses used by
    # angr_tui's CFG output. Only load the already-analyzed binary.
    import cle

    loader = cle.Loader(str(args.binary), auto_load_libs=False)
    literals = set()
    for block in cfg.get("blocks", []):
        for insn in block.get("insns", []):
            match = LITERAL_LOAD.search(insn.get("text", ""))
            if match:
                literals.add(int(match.group(1), 0))

    candidates: dict[int, list[int]] = {}
    for address in sorted(literals):
        try:
            value = int.from_bytes(loader.memory.load(address, 4), "little")
        except (KeyError, ValueError):
            continue
        device_type = (value >> 16) & 0xffff
        function = (value >> 2) & 0xfff
        if device_type == args.device_type and args.function_start <= function <= args.function_end:
            candidates.setdefault(value, []).append(address)

    document = {
        "binary": str(args.binary),
        "function": cfg.get("name") or args.function,
        "function_address": cfg.get("addr"),
        "device_type": args.device_type,
        "function_range": [args.function_start, args.function_end],
        "candidates": [
            {
                "code": f"0x{code:08x}",
                "function": (code >> 2) & 0xfff,
                "method": code & 3,
                "access": (code >> 14) & 3,
                "literal_addresses": [f"0x{address:x}" for address in addresses],
            }
            for code, addresses in sorted(candidates.items())
        ],
    }
    output = json.dumps(document, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output, encoding="utf-8")
        print(f"found {len(candidates)} candidate codes; wrote {args.output}")
    else:
        print(output, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
