"""Coverage records shared by the fuzzer and the reverse-engineering TUI."""

from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class FunctionCoverage:
    name: str
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
        rows.append(FunctionCoverage(name, total, covered, hits))
    return sorted(rows, key=lambda row: (-row.percent, row.name))


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
