#!/usr/bin/env python3
"""A campaign's series.jsonl (one row a minute, written by the master) as a
table: coverage and its growth, throughput, and each worker's input and
reset times, restores, timeouts and comparison logs.

  series.py RUN/series.jsonl [--every N]   one line per N minutes (default 5)
"""
import argparse
import datetime
import json


def main():
    p = argparse.ArgumentParser()
    p.add_argument("series")
    p.add_argument("--every", type=int, default=5, help="minutes per line")
    a = p.parse_args()
    rows = [json.loads(l) for l in open(a.series) if l.strip()]
    if not rows:
        raise SystemExit("no rows yet")
    print(f"{'time':>8} {'blocks':>6} {'+':>4} {'execs':>9} {'/s':>6} {'corpus':>6} {'find':>4}  workers: exec ms, reset ms, restores, timeouts, cmplog, live")
    last = None
    for i, r in enumerate(rows):
        if i % a.every and i != len(rows) - 1:
            continue
        grew = r["blocks"] - last["blocks"] if last else 0
        rate = (r["execs"] - last["execs"]) / max(r["t"] - last["t"], 1) if last else r["execs_per_sec"]
        workers = "  ".join(
            f"{w['exec_us'] / 1000:.0f}/{w['reset_us'] / 1000:.1f}/{w['restores']}/{w['timeouts']}/{w['cmplog']}{'' if w['live'] else ' dead'}"
            for w in sorted(r["instances"], key=lambda w: w["name"])
        )
        clock = datetime.datetime.fromtimestamp(r["t"]).strftime("%H:%M")
        print(f"{clock:>8} {r['blocks']:>6} {grew:>4} {r['execs']:>9} {rate:>6.1f} {r['corpus']:>6} {r['findings']:>4}  {workers}")
        last = r


if __name__ == "__main__":
    main()
