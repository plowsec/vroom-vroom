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
        settings_owned = True
        call("kernel.info")
        call("kernel.symbols")
        address = call("kernel.resolve", {"name": "KeQueryPerformanceCounter"})["address"]
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
        print(json.dumps({"ok": True,
                          "stack_frames": len(stack["frames"]),
                          "step_over": over, "step_out": out}))
    finally:
        call("vm.pause")
        if settings_owned:
            call("debug.breakpoints", {"addresses": []})
            call("debug.watchpoints", {"watches": []})
        if running:
            call("vm.resume")


if __name__ == "__main__":
    main()
