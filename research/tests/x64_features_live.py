#!/usr/bin/env python3
"""Opt-in x64 feature regression checks against a running Windows lab.

Pauses Windows for inspection and restores its original running/paused state.
Requires a local lab socket, kernel PDB access, and no active debugging session.
Only the files created by this test's dump/export calls are removed.
"""
import argparse
import json
from pathlib import Path
import socket


class Rpc:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(240)
        self.socket.connect(path)
        self.file = self.socket.makefile("rw")
        self.seq = 0
        self.call("initialize", {"actor": "x64-features-live"})

    def call(self, method, params=None, error=False):
        self.seq += 1
        self.file.write(json.dumps({"jsonrpc": "2.0", "id": self.seq,
                                    "method": method, "params": params or {}}) + "\n")
        self.file.flush()
        line = self.file.readline()
        assert line, "lab disconnected"
        reply = json.loads(line)
        assert reply["id"] == self.seq, reply
        if error:
            assert "error" in reply, reply
            return reply["error"]
        assert "error" not in reply, reply
        return reply["result"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    args = parser.parse_args()
    rpc = Rpc(args.socket)
    call = rpc.call
    status = call("vm.status")
    assert not status["breakpoints"] and not status["watchpoints"], status
    running = status["state"] == "Running"
    artifacts = []
    try:
        if running:
            call("memory.dump", {"address": "0x1000", "length": 64}, error=True)
            call("memory.search", {"address": "0x1000", "length": 64, "text": "MZ"}, error=True)
        call("vm.pause")
        commands = {c["name"] for c in call("commands.list")}
        required = {"memory.dump", "memory.search"}
        assert required <= commands, sorted(required - commands)
        kernel = call("kernel.info")
        base = kernel["base"]
        params = {"address": hex(base), "length": 4096}
        for pattern in ({"hex": "4d 5a"}, {"text": "MZ"},
                        {"int": 0x5A4D, "width": 2}, {"hex": "4d ??"}):
            result = call("memory.search", params | pattern)
            assert base in result["hits"], result
        for bad in ({"hex": "zz"}, {"text": ""}, {"hex": "4d", "align": 3},
                    {"hex": "4d", "limit": 0}):
            call("memory.search", params | bad, error=True)
        call("memory.dump", {"address": hex(base), "length": 0}, error=True)
        header = call("memory.read_virtual", {"address": hex(base), "length": 4096})
        dump = call("memory.dump", params)
        path = Path(dump["path"])
        artifacts.append(path)
        assert path.read_bytes() == bytes.fromhex(header["hex"]), dump
        print(json.dumps({"ok": True, "kernel": kernel["name"],
                          "commands": len(commands), "dump_bytes": path.stat().st_size}))
    finally:
        for path in artifacts:
            path.unlink(missing_ok=True)
        if running:
            call("vm.resume")


if __name__ == "__main__":
    main()
