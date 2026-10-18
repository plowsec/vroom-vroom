# kernel debugger

Windows ARM64 · macOS/HVF · Linux ARM64/KVM · experimental. No KDNET or guest driver.
Optional; excluded from normal vroom builds.

## fuzz lab

Snapshot fuzzer, optional package: [fuzz/README.md](fuzz/README.md), walkthrough [fuzz/DEMO.md](fuzz/DEMO.md).

```sh
cargo check -p vroom-vmm                        # product build
cargo check -p vroom-vmm --features research    # + file-backed RAM baselines, paused restore API
```

## targets

`hevd/`: HackSys Extreme Vulnerable Driver ([hevd/README.md](hevd/README.md)). Target binaries, checkpoints and coverage stay outside the repo.

TUI: `coverage RUN/coverage.json`; `coverage-diff BASE/coverage.json CUR/coverage.json` (same block manifest): new blocks and hit changes per function.

## run

```sh
research/run.sh /path/to/lab.toml
research/run.sh /path/to/lab.toml --restore /path/to/checkpoint
research/run.sh /path/to/lab.toml --atui /path/to/run-atui-macos.sh
research/demo.sh CONFIG CHECKPOINT       # paused Ex callback
research/build.sh                        # build + sign
```

Requires `uv`. UEFI: QEMU firmware; TPM: swtpm. Nested virtualization off.
Linux: `/dev/kvm` access. HVF checkpoints cannot restore on KVM.
Use a disposable overlay. Separate state directory. Starts paused.

## agents

JSON-RPC 2.0, one object per line, owner-only Unix socket. Same protocol as angr_tui's.

```sh
research/build.sh --release                                                # signed: target/research/release/vroom-lab
research/run.sh LAB.toml --restore CHECKPOINT --agent-socket lab.sock     # beside the TUI
target/research/release/vroom-lab LAB.toml --restore CHECKPOINT --agent-socket lab.sock --daemon   # headless
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
vm        status pause resume step step_over step_out checkpoint screenshot key shutdown
guest     exec upload download update_tools                       VM running
cpu       registers write_register
memory    regions read_virtual read_physical translate write_virtual write_physical dump
debug     breakpoints watchpoints
kernel    info symbols functions resolve processes process peb user_modules modules
          callbacks stack image export process_event cache
fuzz      check run* probe* coverage blocks diff crashes          * jobs.start only
```

- Commands run on the lab's thread, in order with the TUI's. The TUI sees the agent's stops too.
- The lab starts paused: `vm.resume` runs the guest (network, agent, screen). `vm.status` shows `guest_agent`.
- `guest.exec {command, powershell?, timeout?, max_output?}`: the guest agent runs it (cmd.exe as SYSTEM); answers `{exit_code, timed_out, truncated, stdout, stderr}` when it ends. Older agent: `guest.update_tools` first.
- `guest.upload {host_path, guest_path}`, `guest.download {guest_path, host_path?}`: host paths absolute or under exports/; answer `{size, sha256}`.
- Each command lists params, `mutates` and `risk` (read, control, write, destructive): gate writes on these.
- Addresses: integers or `"0x..."`. Inspection needs a paused VM.
- `events.poll` without `after` waits for the next event (stop, fault, shutdown). Events carry `seq`.
- Arrays over `limit` (200) return `{items, total, handle, next_offset}`: page with `results.page`.
- `fuzz.run`/`fuzz.probe` start `vroom-fuzz` (its own VM, signed; `$VROOM_FUZZ` or `target/release/vroom-fuzz`). Output and progress: `OUTPUT.agent.jsonl`, `OUTPUT.agent.log`.
- `fuzz.coverage|blocks|diff|crashes` read a run directory; no VM.
- Results and jobs belong to the actor that made them.
- Socket paths: under 104 bytes. A path in a deep temp dir fails: use a relative one.
- `--daemon`: own session, returns when the socket accepts (prints pid, log `PATH.log`). Use it from agents: their shells often kill what a command started, which leaves a dead socket (`Connection refused`).
- `target/debug/vroom-lab` is unsigned (cargo tests relink it): run the `target/research/` builds.
- `--restore` takes a lab checkpoint or a GUI snapshot (same format): its folder, or just its name (`snap-...`) for the VM's own snapshots. One VM runs once: the lab and the app can't both run it.

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

| Key | Action |
|---|---|
| Enter | Module → functions; callback/function → assembly + C |
| Enter on register/memory | Prepare edit in command bar |
| F2 | Toggle execution breakpoint |
| F3 | Analyze current address |
| F4 | Kernel + symbols + modules |
| F5 / F6 / F7 | Run / pause / step into |
| F8 / Shift+F8 | Step over / out |
| F9 | Side panels |
| F10 | Expand Code view |
| u / U | Address back / forward |
| x / Ctrl+G | Xrefs / CFG |
| : / ↑ / ↓ | Command / history |
| Ctrl+Q | Power off + quit |

Code: assembly left, angr C right. Requires `--atui`.
Live/File button: paused memory or matching binary. Cached while paused.
`PARTIAL`: missing pages. `reloc?`: unresolved absolute internal pointers.

## commands

```text
functions ntoskrnl Ps           prefix filter; Enter opens function
next / prev                    function pages (200 rows)
symbols ntdll                   download/cache matching PDB
x nt!KeBugCheckEx               resolve
dis nt!KeBugCheckEx             assembly + C
bp nt!KeBugCheckEx              execution breakpoint; 4 slots
bc [ADDRESS]                   clear one/all execution breakpoints
watch ADDRESS 8 w              data watchpoint; r/w/rw; 2 slots
wc [ADDRESS]                   clear one/all watchpoints
ps                             processes; Enter selects context
process 1234                   select PID for memory/symbols
process                        return to CPU memory context
peb                            PEB, image path, command line
modules                        kernel modules
process 1234                   kernel + selected process modules
functions ntdll NtCreate        user functions
bp ntdll!NtCreateFile           process-scoped user breakpoint
cpu 1                          select CPU registers/stepping
reg x0 0x1234                  edit CPU register; fp/lr aliases
reg pc 0xfffff80000001000       edit instruction pointer
into / over                    step into / over
out [VL_BYTES]                 step out; optional SVE length
stack [VL_BYTES]               CPU stack + symbols; optional SVE length
mem ADDRESS 256                virtual memory + ASCII
mem-phys ADDRESS 256           physical RAM
write ADDRESS 41424300          edit virtual memory; hex bytes
write-phys ADDRESS 41424300     edit physical RAM; hex bytes
image ntoskrnl                  fetch verified full binary
image example.sys /path/file    use matching local binary
dec-file nt!KeGetCurrentIrql    full binary → angr_tui
dec ADDRESS                    live image → angr_tui; strict
dec-partial ADDRESS            allow missing pages; labelled
cfg [SYMBOL]                   function blocks + edges
xrefs [SYMBOL]                 incoming CFG edges
back / forward                 address history
dump ADDRESS 4096              raw memory file in state/exports
callbacks                      process callback registrations
event                          Ex/Ex2 arguments at entry
checkpoint /new/directory       save paused VM
```

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
Unwind tables cached; binary search. Exports and decompilation reused while paused.
Function filtering debounced; 200-row pages. Downloads/angr run outside TUI rendering.

## layout

```text
research/kernel/     backend, PE/PDB, process inspection, unwind
research/hevd/       HEVD ARM64 target notes and coverage audit tools
research/tui/        Textual, Capstone, optional angr_tui
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
