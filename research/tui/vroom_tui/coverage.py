"""Coverage records shared by the fuzzer and the reverse-engineering TUI."""

from dataclasses import dataclass
import json
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class FunctionCoverage:
    name: str
    module: str
    total: int
    covered: int
    hits: int

    @property
    def percent(self) -> float:
        return 100.0 * self.covered / self.total if self.total else 0.0


def summarize(summary: dict[str, Any]) -> list[FunctionCoverage]:
    rows = []
    for name, value in summary.get("functions", {}).items():
        total, covered, hits = value
        rows.append(FunctionCoverage(name, "", total, covered, hits))
    return sorted(rows, key=lambda row: (-row.percent, row.name))


def load_artifacts(coverage_path: str | Path, blocks_path: str | Path | None = None):
    """Load vroom-fuzz output and aggregate hit counts by module/function."""
    coverage_path = Path(coverage_path)
    blocks_path = Path(blocks_path) if blocks_path else coverage_path.with_name("blocks.json")
    coverage = json.loads(coverage_path.read_text(encoding="utf-8"))
    blocks = json.loads(blocks_path.read_text(encoding="utf-8"))
    hits_by_id = coverage.get("blocks", {})
    functions: dict[tuple[str, str], list[int]] = {}
    block_rows = []
    for block in blocks:
        ident = str(block["id"])
        details = hits_by_id.get(ident, {})
        hits = int(details.get("hits", 0))
        module = block.get("module") or "<unknown>"
        name = block.get("function") or "<unknown>"
        total, covered, count = functions.setdefault((module, name), [0, 0, 0])
        functions[(module, name)][0] = total + 1
        functions[(module, name)][1] = covered + (hits > 0)
        functions[(module, name)][2] = count + hits
        block_rows.append({**block, "hits": hits, "first_input": details.get("first_input")})

    rows = [
        FunctionCoverage(name, module, *values)
        for (module, name), values in functions.items()
    ]
    rows.sort(key=lambda row: (row.percent, row.module, row.name))
    summary = {
        "executions": int(coverage.get("executions", 0)),
        "new_edges": int(coverage.get("new_edges", 0)),
        "covered_blocks": sum(row["hits"] > 0 for row in block_rows),
        "total_blocks": len(block_rows),
    }
    return summary, rows, block_rows


def render(summary: dict[str, Any]) -> str:
    """Compact table for a TUI panel or an agent response."""
    rows = summarize(summary)
    head = (
        f"blocks {summary.get('covered_blocks', 0)}/{summary.get('total_blocks', 0)}  "
        f"runs {summary.get('executions', 0)}  new edges {summary.get('new_edges', 0)}"
    )
    body = [head, "function                         blocks       hits"]
    body.extend(
        f"{row.name[:32]:32} {row.covered:4}/{row.total:<4} {row.hits:10}"
        for row in rows
    )
    return "\n".join(body)
