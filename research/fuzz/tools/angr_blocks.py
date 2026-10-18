#!/usr/bin/env python3
"""Export a function and its in-image direct callees from angr_tui.

The analysis file supplies static virtual addresses. `runtime_base` is the
module base from the paused guest; `static_base` is the PE image base reported
by the analysis tool. The vroom runner translates the resulting VAs through
the restored guest page tables, so the manifest does not contain stale GPAs.
"""

import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path


def integer(text: str) -> int:
    return int(text, 0)


CALL_TARGET = re.compile(r"\bbl\s+#?(0x[0-9a-f]+)\b", re.IGNORECASE)


def run_atui(atui: str, binary: Path, function: str) -> dict | None:
    result = subprocess.run(
        [atui, str(binary), "--pd", function, "--json"],
        check=False, capture_output=True, text=True,
    )
    if result.returncode:
        return None
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="driver or executable to analyze")
    parser.add_argument("function", help="function name or static virtual address")
    parser.add_argument("--static-base", required=True, type=integer,
                        help="image base used by angr_tui for the file")
    parser.add_argument("--runtime-base", required=True, type=integer,
                        help="module base in the paused guest")
    parser.add_argument("--module", required=True, help="module label in coverage views")
    parser.add_argument("--name", help="function label; defaults to function argument")
    parser.add_argument("--follow-calls", action=argparse.BooleanOptionalAction, default=True,
                        help="include direct callees inside the driver image (default: on)")
    parser.add_argument("--max-functions", type=int, default=256,
                        help="cap recursive analysis work (default: 256)")
    parser.add_argument("--output", type=Path, required=True, help="write block JSON here")
    args = parser.parse_args()
    if args.max_functions < 1:
        parser.error("--max-functions must be at least 1")

    atui = shutil.which("atui")
    if not atui:
        parser.error("atui not found; install the optional angr extra first")
    try:
        import cle
        image = cle.Loader(str(args.binary), auto_load_libs=False).main_object
    except Exception as error:
        raise SystemExit(f"could not read driver image bounds: {error}") from error

    cfgs: dict[int, dict] = {}
    root = run_atui(atui, args.binary, args.function)
    if root is None:
        raise SystemExit(f"angr_tui could not analyze {args.function!r}")
    cfgs[int(root["addr"])] = root
    queue = [root]
    seen_call_targets: set[int] = set()
    while queue and args.follow_calls and len(cfgs) < args.max_functions:
        current = queue.pop(0)
        for block in current.get("blocks", []):
            for insn in block.get("insns", []):
                match = CALL_TARGET.search(insn.get("text", ""))
                if not match:
                    continue
                target = int(match.group(1), 16)
                if not image.min_addr <= target <= image.max_addr:
                    continue
                if target in seen_call_targets:
                    continue
                seen_call_targets.add(target)
                child = run_atui(atui, args.binary, hex(target))
                if child is None or not child.get("blocks"):
                    continue
                entry = int(child["addr"])
                if entry not in cfgs:
                    cfgs[entry] = child
                    queue.append(child)
    if queue:
        print(f"warning: stopped call traversal at --max-functions={args.max_functions}",
              file=sys.stderr)

    all_blocks: dict[int, dict] = {}
    edges: dict[int, set[int]] = {}
    missing_entries: dict[int, int] = {}
    for cfg in cfgs.values():
        entry = int(cfg["addr"])
        function_blocks = cfg.get("blocks", [])
        block_addresses = {int(block["addr"]) for block in function_blocks}
        if function_blocks and entry not in block_addresses:
            # Some recovered functions begin at an entry thunk or an opaque
            # prologue that angr_tui does not lift into its block list. Keep
            # the real call target in the coverage map and connect it to the
            # first recovered block so the runtime can still instrument it.
            first_block = min(block_addresses)
            missing_entries[entry] = first_block
            all_blocks[entry] = {
                "addr": entry,
                "size": 4,
                "function": cfg.get("name") or hex(entry),
            }
        for block in cfg.get("blocks", []):
            address = int(block["addr"])
            all_blocks[address] = {**block, "function": cfg.get("name") or hex(cfg["addr"])}
        for edge in cfg.get("edges", []):
            source, target = int(edge["src"]), int(edge["dst"])
            edges.setdefault(source, set()).add(target)
    # angr_tui's function graph keeps call targets separately from intrafunction
    # CFG edges. Add direct BL edges when the callee was included in the image.
    for cfg in cfgs.values():
        for block in cfg.get("blocks", []):
            source = int(block["addr"])
            for insn in block.get("insns", []):
                match = CALL_TARGET.search(insn.get("text", ""))
                if match:
                    target = int(match.group(1), 16)
                    if target in all_blocks:
                        edges.setdefault(source, set()).add(target)
    for entry, first_block in missing_entries.items():
        edges.setdefault(entry, set()).add(first_block)

    rows = []
    for index, block in enumerate(sorted(all_blocks.values(), key=lambda row: int(row["addr"])), 1):
        static_va = int(block["addr"])
        size = int(block["size"])
        if static_va < args.static_base or size <= 0:
            raise SystemExit(f"invalid block address or size: {block!r}")
        rows.append({
            "id": index,
            "gpa": None,
            "va": args.runtime_base + static_va - args.static_base,
            "size": size,
            "successors": [
                args.runtime_base + target - args.static_base
                for target in sorted(edges.get(static_va, ()))
            ],
            "module": args.module,
            "function": args.name or block["function"],
            "source": None,
        })
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(rows, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {len(rows)} blocks from {len(cfgs)} functions to {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
