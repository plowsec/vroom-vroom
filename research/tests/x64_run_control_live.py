#!/usr/bin/env python3
"""Opt-in x64 tracing/stepping checks; uses hardware slots, never patches code.

Requires an idle debugging session and matching kernel symbols. Restores the
original running/paused state and clears only the debug settings this test arms.
"""
import argparse
import json
import time

from x64_features_live import Rpc


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    args = parser.parse_args()
    call = Rpc(args.socket).call
    status = call("vm.status")
    assert not status["breakpoints"] and not status["watchpoints"], status
    running = status["state"] == "Running"
    settings_owned = False

    def wait_stop(after):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            events = call("events.poll", {"after": after, "wait_ms": 1000})
            for event in events["events"]:
                if event.get("event") == "stopped":
                    return event
            after = events["latest"]
        call("vm.pause")
        raise AssertionError("debug run did not stop within 10 seconds")

    try:
        call("vm.pause")
        prior = call("debug.traces")
        assert not prior["records"], "existing trace records; use a clean session"
        assert not prior.get("tracepoints") and not prior.get("hwatch"), prior
        faults = call("debug.faults")
        assert not faults.get("catching"), faults
        settings_owned = True
        call("kernel.info")
        call("kernel.symbols")
        address = call("kernel.resolve", {"name": "KeQueryPerformanceCounter"})["address"]
        call("debug.tracepoints", {"tracepoints": [{"address": hex(address),
             "kind": "hardware", "register": "rax", "max_hits": 1}]})
        call("vm.resume")
        deadline = time.monotonic() + 10
        trace = None
        while time.monotonic() < deadline:
            records = call("debug.traces")["records"]
            if records:
                trace = records[0]
                break
            time.sleep(.05)
        assert trace is not None, "hardware tracepoint never recorded"
        assert call("vm.status")["state"] == "Running", "tracepoint failed to auto-continue"
        call("vm.pause")
        call("debug.tracepoints", {"tracepoints": []})
        call("debug.traces", {"clear": True})
        # A rejected overcommit must not silently discard the existing slots.
        points = [hex(address + n) for n in range(4)]
        call("debug.breakpoints", {"addresses": points})
        call("debug.tracepoints", {"tracepoints": [{"address": hex(address + 8),
             "kind": "hardware"}]}, error=True)
        assert call("vm.status")["breakpoints"] == [address + n for n in range(4)]
        call("debug.breakpoints", {"addresses": []})
        # Recent Windows builds update KUSER_SHARED_DATA through a private
        # writable alias. InterruptTime is at public AMD64 ABI offset 8.
        alias = call("kernel.resolve", {"name": "MmWriteableSharedUserData"})["address"]
        pointer = call("memory.read_virtual", {"address": hex(alias), "length": 8})["hex"]
        tick = int.from_bytes(bytes.fromhex(pointer), "little") + 8
        assert tick % 4 == 0, hex(tick)
        call("debug.hwatch", {"watches": [{"address": hex(tick),
             "length": 4, "on": "write", "kind": "hardware"}]})
        call("vm.resume")
        deadline = time.monotonic() + 10
        watch = None
        while time.monotonic() < deadline:
            watch = next((r for r in call("debug.traces")["records"]
                          if r.get("kind") == "hwatch"), None)
            if watch:
                break
            time.sleep(.05)
        assert watch is not None, "timer-data watch never recorded"
        call("vm.pause")
        call("debug.hwatch", {"watches": []})
        call("debug.traces", {"clear": True})
        call("debug.faults", {"enable": True, "kind": "hardware", "mode": "kernel"})
        call("debug.faults")
        call("debug.faults", {"enable": False})
        after = call("events.poll")["latest"]
        call("debug.breakpoints", {"addresses": [hex(address)]})
        call("vm.resume")
        stopped = wait_stop(after)
        assert stopped["pc"] == address, stopped
        cpu = stopped["cpu"]
        stack = call("kernel.stack", {"cpu": cpu})
        assert stack["frames"][0]["pc"] == address, stack
        assert len(stack["frames"]) >= 2, stack
        call("debug.breakpoints", {"addresses": []})
        after = call("events.poll")["latest"]
        call("vm.step_over", {"cpu": cpu})
        over = wait_stop(after)
        assert over["cpu"] == cpu, over
        after = call("events.poll")["latest"]
        call("vm.step_out", {"cpu": cpu})
        out = wait_stop(after)
        assert out["cpu"] == cpu, out
        print(json.dumps({"ok": True, "trace": trace, "hwatch": watch,
                          "stack_frames": len(stack["frames"]),
                          "step_over": over, "step_out": out}))
    finally:
        call("vm.pause")
        if settings_owned:
            call("debug.faults", {"enable": False})
            call("debug.tracepoints", {"tracepoints": []})
            call("debug.hwatch", {"watches": []})
            call("debug.breakpoints", {"addresses": []})
            call("debug.watchpoints", {"watches": []})
        if running:
            call("vm.resume")


if __name__ == "__main__":
    main()
