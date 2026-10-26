# kernel debugger

Windows ARM64 · macOS/HVF · Linux ARM64/KVM · experimental. No KDNET or guest driver.
Optional; excluded from normal vroom builds.

## fuzz lab

Snapshot fuzzer, optional package: [fuzz/README.md](fuzz/README.md), walkthrough [fuzz/DEMO.md](fuzz/DEMO.md).

```sh
cargo check -p vroom-vmm                        # product build
cargo check -p vroom-vmm --features research    # + file-backed RAM baselines, paused restore API
```

## demo

Paused on an EDR's process-notify callback, with the arguments it was
called with: [DEMO.md](DEMO.md), which also covers making the snapshot
(the demo refuses any other).

## targets

`hevd/`: HackSys Extreme Vulnerable Driver ([hevd/README.md](hevd/README.md)). Target binaries, snapshots and coverage stay outside the repo.

TUI: `:vroom.coverage path=RUN/coverage.json [baseline=BASE/coverage.json]` (same block manifest), `:pane vroom-coverage`: blocks and hit changes per function.

## run

```sh
research/run.sh /path/to/lab.toml                              # boots paused; angr_tui on the module at pc
research/run.sh /path/to/lab.toml --restore /path/to/snapshot
research/run.sh /path/to/lab.toml --keep                       # lab outlives angr_tui
research/run.sh path/to/lab.sock                               # attach a running lab; it keeps running
research/run.sh path/to/lab.sock --binary driver.sys           # open a file instead, lab attached
research/run.sh work.atui                                      # resume: re-attach, restore checkpoint, re-arm
research/demo.sh CONFIG SNAPSHOT       # paused Ex callback: DEMO.md
research/build.sh                        # build + sign
uv run --project research/tui vroom-atui LAB.toml -- ATUI_OPTIONS
```

One lab per config: its socket is `~/.cache/vroom-atui/<hash>.sock`; a second
`run.sh` attaches to it (`--restore`: restores it). From angr_tui:
`:attach vroom LAB.toml[,restore=SNAPSHOT][,keep]` or `:attach vroom lab.sock[,stop]`.
A lab angr_tui started stops with it, crash or signal included, unless `keep`.
Without `--binary` the lab must be paused (it starts paused).

Checkpoints: `:checkpoint NAME [work.atui]` = `vm.snapshot NAME` + project save (targets,
breakpoints and watchpoints as module + offset, view). `:resume` / `run.sh work.atui`
re-attaches, `vm.restore`s it, re-arms. `:resume here`: no restore; addresses follow
the modules' current bases.

Requires `uv` and `../angr_tui` (path dependency). UEFI: QEMU firmware; TPM: swtpm. Nested virtualization off.
Linux: `/dev/kvm` access. HVF snapshots cannot restore on KVM.
Use a disposable overlay. Separate state directory. Starts paused.

## agents

JSON-RPC 2.0, one object per line, owner-only Unix socket. Same protocol as angr_tui's.

```sh
research/build.sh --release                                                # signed: target/research/release/vroom-lab
research/run.sh LAB.toml --restore SNAPSHOT --keep                      # beside the TUI: ~/.cache/vroom-atui/*.sock
target/research/release/vroom-lab LAB.toml --restore SNAPSHOT --agent-socket lab.sock --daemon   # headless
kill PID                                                                   # stops the VM, removes the socket
vroom-rpc --socket lab.sock describe
vroom-rpc --socket lab.sock commands.list '{"category":"kernel"}'
vroom-rpc --socket lab.sock run memory.read_virtual '{"cpu":1,"address":"0xfffff80275ff70a8","length":16}'
vroom-rpc --socket lab.sock events.poll '{"wait_ms":10000}'
```

Methods: `initialize {actor}` first, `describe`, `commands.list {category?}`,
`commands.run {command, args, limit?}`, `results.page {handle, offset?, limit?}`,
`events.poll {after?, wait_ms?, limit?}`, `jobs.start {command, args}`,
`jobs.status|result|cancel {handle}`.

```text
vm        status pause resume step step_over step_out snapshot snapshots restore screenshot key shutdown
guest     exec upload download update_tools                       VM running
cpu       registers write_register
memory    regions read_virtual read_physical translate write_virtual write_physical dump search
debug     breakpoints watchpoints tracepoints hwatch traces faults
kernel    info symbols functions resolve processes process peb user_modules modules
          callbacks stack image export process_event cache
fuzz      check run* probe* coverage blocks diff crashes          * jobs.start only
```

- Commands run on the lab's thread, in order with the TUI's. The TUI sees the agent's stops too.
- The lab starts paused: `vm.resume` runs the guest (network, agent, screen). `vm.status` shows `guest_agent`.
- `guest.exec {command, powershell?, timeout?, max_output?}`: the guest agent runs it (cmd.exe as SYSTEM); answers `{exit_code, timed_out, truncated, stdout, stderr}` when it ends. Older agent: `guest.update_tools` first.
- `guest.upload {host_path, guest_path}`, `guest.download {guest_path, host_path?}`: host paths absolute or under exports/; answer `{size, sha256}`.
- `memory.read_virtual`/`memory.dump` `skip_unmapped: true` read what translates and list the holes. `memory.search {address, length, hex (?? wildcards) | int+width | text+utf16, align?, limit?}` finds a pattern in up to 1 GiB, skipping holes; `next` resumes a truncated search.
- Each command lists params, `mutates` and `risk` (read, control, write, destructive): gate writes on these.
- Addresses: integers or `"0x..."`. Inspection (`kernel.*`, `memory.*`, `cpu.*`, `debug.*`) needs a paused VM: `vm.pause`, inspect, `vm.resume`. A guest left paused answers nothing, so network probes against it time out with no error.
- Methods and commands share one namespace: `run commands.list` works, so does `{"method":"kernel.functions"}`. `-32601` means a wrong name (the error names the nearest ones), not a missing feature.
- Memory read/write watches without slots: `debug.hwatch [{address, length, on, kind: "software", stop?}]`, any number, any address, up to 16 MiB each. The hypervisor protects their pages, so every access to a watched page exits; `memory_watch {stops, passes}` shows the cost. Hardware hwatches stay 2 slots of 8 bytes, cheaper on hot pages.
- Guest faults: `debug.faults {enable: true, mode?, pid?, stop?}` traces `nt!KiDispatchException`, so every exception is recorded at first chance with its faulting address (`access violation: read of 0x8 at app.dll+0x1234 (...), app.exe pid 1234, user mode, first chance`), trap-frame registers and stack. They never exit to the hypervisor (the guest's EL1 takes them), so this is where they are seen. `debug.faults {}` reads them while the VM runs; `vm.restore` keeps it armed.
- Tracepoints and hwatches record and continue. `pid` pins one to a process; `record_if {register, equals}` keeps only matching hits (`filtered` counts the rest, and hits in other processes). Records carry `cntvct` (guest CNTVCT_EL0) and `time_ns`. `debug.traces {memory: {at, offset, length}}` adds bytes read now at a capture's address. `debug.tracepoints {}` reports planted/pending without pausing.
- `events.poll` without `after` waits for the next event (stop, fault, shutdown). Events carry `seq`.
- Arrays over `limit` (200) return `{items, total, handle, next_offset}`: page with `results.page`.
- `fuzz.run`/`fuzz.probe`/`fuzz.trace` start `vroom-fuzz` (its own VM, signed; `$VROOM_FUZZ` or `target/release/vroom-fuzz`). Output and progress: `OUTPUT.agent.jsonl`, `OUTPUT.agent.log`. `fuzz.trace`'s OUTPUT is the trace file.
- `fuzz.coverage|blocks|diff|crashes` read a run directory; no VM.
- Results and jobs belong to the actor that made them.
- Socket paths: under 104 bytes. A path in a deep temp dir fails: use a relative one.
- `--daemon`: own session, returns when the socket accepts (prints pid, log `PATH.log`). Use it from agents: their shells often kill what a command started, which leaves a dead socket (`Connection refused`).
- `target/debug/vroom-lab` is unsigned (cargo tests relink it): run the `target/research/` builds.
- Snapshots are one format everywhere (app, `vroom`, lab, fuzzer). `vm.snapshot` saves into the VM's own snapshots (the app lists them) or `path`; `vm.restore` restarts the lab's VM from one (`name` or `path`), paused (`resume: true`: running); `--restore` does it at start. Refused when a disk layer under the snapshot changed since. One VM runs once: the lab and the app can't both run it.

Function-calling adapter (one tool):

```python
import json, socket
s = socket.socket(socket.AF_UNIX); s.connect("lab.sock"); f = s.makefile("rw")
def vroom(method, params=None, _id=[0]):
    _id[0] += 1
    f.write(json.dumps({"jsonrpc": "2.0", "id": _id[0], "method": method, "params": params or {}}) + "\n"); f.flush()
    r = json.loads(f.readline())
    if "error" in r: raise RuntimeError(r["error"])
    return r["result"]
vroom("initialize", {"actor": "pentest-llm"})
```

## keys

angr_tui with target `vroom` attached ([angr_tui](../../angr_tui) README for its own keys).

| Key | Action |
|---|---|
| F5 / F12 | Run / pause |
| F7 / F8 | Step into / over |
| F2 | Toggle execution breakpoint (4 slots) |
| F6 | Code / run view |
| Ctrl+T | Next target: vroom, angr emulator |
| F3 / F4 | Modules / processes (Enter: select context) |
| F10 / F11 | Stack / callback event |
| Enter on a pane row | Go to its address |

## commands

```text
:pane vroom-callbacks          callbacks; also -peb -faults -traces -snapshots -coverage
:vroom.cpu cpu=1               registers, stepping, memory on this CPU
:vroom.process pid=1234        user addresses + modules through this process
:vroom.process                 back to the CPU's context
:vroom.discover                kernel + verified PDB
:vroom.vm.step_out             step out
:vroom.kernel.resolve name=nt!KeBugCheckEx
:vroom.kernel.functions prefix=Ps
:vroom.memory.read_virtual address=0x... length=64
:vroom.memory.write_virtual address=0x... hex=41424300
:vroom.memory.dump address=0x... length=4096
:vroom.debug.tracepoints {"tracepoints": [...]}
:vroom.debug.faults enable=true
:vroom.guest.download guest_path=C:\\x.txt
:vroom.vm.snapshot             (snapshots pane: s save, r restore)
:vroom.coverage path=RUN/coverage.json baseline=BASE/coverage.json
:vroom.coverage_blocks function=module!function
:wp ADDRESS 8 w                data watchpoint (2 slots)
:bp ADDRESS if CONDITION       condition: angr_tui shell vocabulary (reg, u32, arg)
:attach vroom LAB.toml / :detach vroom
:checkpoint NAME [work.atui]   lab snapshot + project save
:resume [here]                 re-attach at the checkpoint (here: as it is)
```

Every lab command is `:vroom.<command>` (palette, `:` line, angr_tui's agent socket).
Modules open for analysis as RVA copies (`kernel.export`); runtime address = base + RVA.

## semantics

- Process context: inspection; no guest injection. CPU registers stay CPU-local.
- User execution breakpoints: selected address space. Kernel breakpoints/watchpoints: all CPUs.
- Watch ranges: 1–8 bytes within one aligned 8-byte block.
- Stack: packed/xdata, partial prologs/epilogs, links, save_any, trap/machine/context frames.
- SVE: saved VG or explicit vector length. EC context records supported; mixed x64 runtime tables stop.
- PAC stripped, not authenticated. Missing state or unknown records: stop with reason.
- PDBs: GUID/age checked. Binaries: ARM64, timestamp, image size, GUID/age checked.
- File analysis: original code/data; no live patches, resolved imports, or heap state.
- Partial live images: absent/discarded pages. Full binaries may exist on Microsoft's server; local files work too.
- Decompilation: angr recovery limits apply; some ARM64 syscall stubs remain assembly-only.
- Guest paging: resident pages only. No page-in injection, WOW64, KDNET, or WinDbg dump reader.
- Step-over/out: one free breakpoint slot; SP/address-space checked. Pause cancels. Interrupts stay enabled.
- Edits: paused only; CPU registers or selected-process memory. Entire write validated before mutation.
- KVM: ARM64 implementation cross-checked; live hardware validation pending.

## speed

32 MiB paused-state page cache. Invalidated before execution and after edits.
Parsed PDBs cached. Function lookup: `O(log n + page)`.
Unwind tables cached; binary search. Module exports reused per boot and process.

## layout

```text
research/kernel/     backend, PE/PDB, process inspection, unwind
research/hevd/       HEVD ARM64 target notes and coverage audit tools
research/tui/        angr_tui plugin: target vroom, panes, launcher
research/tests/      diskless integration checks
experiments/         ignored local scripts/captures
*.state/             ignored symbols, exports, VM state
```

## checks

```sh
cargo test -p vroom-hv -p vroom-vmm -p vroom-kernel -p vroom-cli
uv run --project research/tui pytest research/tui/tests
research/build.sh
python3 research/tests/control_smoke.py
cargo check -p vroom-kernel --target aarch64-unknown-linux-gnu
cargo build -p vroom-kernel --example debug_smoke
scripts/sign.sh target/debug/examples/debug_smoke
target/debug/examples/debug_smoke
```
