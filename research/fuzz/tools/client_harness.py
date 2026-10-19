#!/usr/bin/env python3
"""Snapshot a parked fuzzing client and write its harness.

A client (research/fuzz/clients/*) runs in the guest, parked in a loop,
and reports `input_va= input_pointer_va= input_length_va= entry_pc=
stop_pc= capacity=` (one line). Against a running vroom-lab: break at
entry_pc in the client's process, snapshot there, find the input page's
physical address, the target module's base and the bugcheck entry, and
write the harness (blocks mode, the whole guest running).

  client_harness.py --socket lab.sock --pid PID --report "input_va=0x..." \\
      --target CLFS.SYS --snapshot clfs-client-park --blocks clfs-blocks.json \\
      --output clfs-harness.json [--sequence] [--entry LogIoDispatch@CClfsDriver]
"""
import argparse
import json
import socket
import sys


class Lab:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(path)
        self.f = self.s.makefile("rw")
        self.n = 0
        self.call("initialize", {"actor": "client_harness"})

    def call(self, method, params):
        self.n += 1
        self.f.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n")
        self.f.flush()
        while True:
            r = json.loads(self.f.readline())
            if r.get("id") == self.n:
                if "error" in r:
                    raise SystemExit(f"{method}: {r['error'].get('message')}")
                return r.get("result")

    def run(self, command, args=None):
        return self.call("commands.run", {"command": command, "args": args or {}})


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--socket", required=True)
    p.add_argument("--pid", type=int, required=True, help="the client's process")
    p.add_argument("--report", required=True, help="the client's address line")
    p.add_argument("--target", required=True, help="module under test, as the kernel lists it")
    p.add_argument("--snapshot", required=True, help="snapshot name (the VM's snapshots)")
    p.add_argument("--blocks", required=True, help="block list, relative to the harness")
    p.add_argument("--output", required=True)
    p.add_argument("--sequence", action="store_true", help="inputs are call sequences")
    p.add_argument("--timeout-ms", type=int, default=3000)
    p.add_argument("--entry", action="append", help="a target function the client's calls enter by (comparison logging steps from there)")
    a = p.parse_args()

    report = dict(kv.split("=", 1) for kv in a.report.split())
    lab = Lab(a.socket)
    status = lab.run("vm.status")
    if status.get("state") != "Paused":
        lab.run("vm.pause")
    lab.run("kernel.info", {"cpu": 0})
    lab.run("kernel.symbols", {"cpu": 0})
    lab.run("kernel.process", {"cpu": 0, "pid": a.pid})
    lab.run("debug.breakpoints", {"addresses": [report["entry_pc"]]})
    lab.run("vm.resume")
    stop = None
    for _ in range(10):
        events = lab.call("events.poll", {"wait_ms": 3000})["events"]
        stop = next((e for e in events if e.get("event") == "stopped"), None)
        if stop:
            break
    if not stop or stop["pc"] != int(report["entry_pc"], 0):
        raise SystemExit(f"no stop at entry_pc: {stop}")
    cpu = stop["cpu"]
    lab.run("debug.breakpoints", {"addresses": []})
    gpa = lab.run("memory.translate", {"cpu": cpu, "address": report["input_va"]})["physical"]
    modules = lab.run("kernel.modules", {"cpu": cpu})
    modules = modules if isinstance(modules, list) else modules.get("items", [])
    module = next((m for m in modules if m["name"].lower() == a.target.lower()), None)
    if module is None:
        raise SystemExit(f"{a.target} isn't loaded")
    bugcheck = lab.run("kernel.resolve", {"name": "KeBugCheckEx"})
    snap = lab.run("vm.snapshot", {"name": a.snapshot, "description": f"{a.target} client parked (pid {a.pid})"})
    status = lab.run("vm.status")
    config = status.get("config") or None
    harness = {
        "name": a.snapshot,
        "snapshot": snap["path"],
        "cpu": cpu,
        "input_gpa": gpa,
        "input_capacity": int(report["capacity"], 0),
        "input_va": int(report["input_va"], 0),
        "input_pointer_va": int(report["input_pointer_va"], 0),
        "input_length_va": int(report["input_length_va"], 0),
        "ioctl_prefix": False,
        "entry_pc": int(report["entry_pc"], 0),
        "stop_pc": int(report["stop_pc"], 0),
        "bugcheck_pc": bugcheck,
        "isolate_cpu": False,
        "timeout_ms": a.timeout_ms,
        "max_input": int(report["capacity"], 0),
        "coverage_mode": "blocks",
        "coverage_file": a.blocks,
        "target": module["name"],
        "sequence": a.sequence,
        "detectors": ["exceptions", "bugcheck", "timeout"],
    }
    if a.entry:
        harness["entries"] = a.entry
    try:
        old = json.load(open(a.output))
        harness = {**old, **harness}
    except FileNotFoundError:
        pass
    if config:
        harness.setdefault("config", config)
    json.dump(harness, open(a.output, "w"), indent=2)
    print(json.dumps({"snapshot": snap["path"], "cpu": cpu, "input_gpa": hex(gpa), "target_base": hex(module["base"]), "harness": a.output}))
    if "config" not in harness or "state_dir" not in harness:
        print("add config (the VM's .toml) and state_dir to the harness", file=sys.stderr)


if __name__ == "__main__":
    main()
