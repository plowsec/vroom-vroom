# kernel debugger

Windows ARM64 (macOS/HVF, Linux/KVM) and AMD64 (Linux/KVM), experimental.
No KDNET or guest kernel driver is required.
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

TUI: `coverage RUN/coverage.json`; `coverage-diff BASE/coverage.json CUR/coverage.json` (same block manifest): new blocks and hit changes per function.

## run

```sh
research/run.sh /path/to/lab.toml
research/run.sh /path/to/lab.toml --restore /path/to/snapshot
research/run.sh /path/to/lab.toml --atui /path/to/run-atui-macos.sh
research/demo.sh CONFIG SNAPSHOT       # paused Ex callback: DEMO.md
research/build.sh                        # build + sign
```

Requires `uv`. UEFI: QEMU firmware; TPM: swtpm. Nested virtualization off.
Linux: `/dev/kvm` access. HVF snapshots cannot restore on KVM.
Use a disposable overlay. Separate state directory. Starts paused.

## agents

### Linux AMD64 backend

The x64 backend boots the real VM on the **Linux host**, not inside an agent's
Coop VM. It starts paused and owns that VM exclusively (the GUI/CLI cannot
run a second copy against the same state directory).

```sh
cargo build -p vroom-kernel --bin vroom-lab --bin vroom-rpc
target/debug/vroom-lab /absolute/path/windows11.toml \
  --agent-socket /run/user/1000/vroom-agent.sock --daemon
```

`commands.list` is architecture-specific. AMD64 currently implements:

- VM status, pause/resume, hard restart (optionally paused at the reset vector),
  snapshot/restore, single-step, screenshot, keyboard and shutdown.
- Snapshots save RAM, CPU, device and disk state; restore replaces the live VM,
  paused by default, on the same KVM host/backend.
- `vm.step_over` decodes native AMD64 near calls; other instructions single-step.
  `vm.step_out` recovers a return target from Windows unwind metadata. Return
  steps run all CPUs/devices and match CPU, stack pointer and CR3; `vm.pause`
  cancels them. There is no automatic timeout, and thread migration can leave a
  return step pending. Firmware/WOW64 contexts and SVE `vector_length` are refused.
- General/control registers, physical and virtual memory reads/writes,
  memory regions, and four-level x64 page translation (4 KiB/2 MiB/1 GiB).
- `memory.dump` writes up to 64 MiB under `exports`; optional holes are listed
  and zero-filled. `memory.search` supports wildcard hex, signed integers,
  UTF-8/UTF-16LE, cross-page matches, alignment and resumable hit limits. Ranges
  are capped at 1 GiB and a deterministic work budget; expensive searches fail
  explicitly and must be split or given a larger alignment.
- Four shared hardware execution/data breakpoint slots. Data watches require
  aligned 1/2/4/8-byte ranges, write or read+write; read-only is unsupported.
- Kernel discovery from live CPU contexts, exact-build Microsoft PDBs,
  symbol lookup, process selection, PEBs, and loaded kernel/user modules.
- `kernel.callbacks` lists process-notify callbacks using matching PDB layouts
  or strictly validated AMD64 instruction evidence when public PDBs omit them;
  `kernel.process_event` decodes an Ex/Ex2 callback at entry. `kernel.image` and
  `kernel.export` produce AMD64-validated static/live PE analysis artifacts.
  `kernel.stack` performs bounded v1/v2 metadata unwinding with partial prologues,
  epilogues, chained records and machine frames, using the actual CPU's CR3.
  Version-2 epilog recovery requires canonical mirrored layouts.
  Unsupported metadata/dynamic code stops explicitly; no guessed frame-pointer
  chain or automatic KPTI address-space substitution is used. `kernel.cache`
  reports real sizes (the x64 debugger has no physical-page cache).
- Guest command execution when guest tools are installed (not needed for
  hypervisor-side memory/kernel access).
- `guest.upload {host_path, guest_path}` copies a host file into the guest:
  `{guest_path, size, sha256}`. `host_path` is absolute or under `exports`, the
  same as ARM64, and is resolved on the lab host, not on the RPC client.
- `guest.download {guest_path, offset?, length?}` returns file bytes directly to
  the RPC client: `{guest_path, offset, length, file_size, eof, hex, sha256}`.
  Offset defaults to 0; length defaults to and cannot exceed 65536 bytes.
  Advance offset by the returned length until eof. Offset beyond EOF returns an
  empty chunk. SHA-256 covers the **whole file**, on every response; compare it
  between chunks to detect changes during retrieval. No host files are created.
  Requires a running VM and connected guest tools, not paused/agentless access.
  For compatibility with installed agents each call streams the full file and
  retains only the requested range (bounded memory, but repeated transfer cost).
  Running reads are live views, not atomic filesystem snapshots.
  `python3 research/guest_download.py --socket SOCKET 'C:\\path\\file' ./file`
  assembles chunks on the RPC client and verifies size and SHA-256. It refuses
  existing output files; failures may leave a partial output for diagnosis.

Capabilities are architecture-specific; unsupported commands are omitted from
discovery and refused through direct calls, command aliases and jobs.
Five-level paging and paged-out memory are rejected rather than guessed.
Use lossless uint64 handling for addresses and response values.

Inspection requires an all-CPU pause. Resume afterwards: while paused, Windows,
its network and Falcon cannot make progress. `vm.shutdown` ends the VM immediately;
prefer Windows' normal shutdown when preserving a running installation.

Opt-in live feature checks: `python3 research/tests/x64_features_live.py SOCKET`.

#### Exposing only this socket to a Coop VM

For this installed Windows lab, after rebooting the Linux host, run as the normal
host user (not with sudo):

```sh
python3 scripts/start-windows-lab.py
```

This starts the existing `images/windows11-x64.KNZ4j8/windows11.toml` with
`vroom-lab --daemon`, resumes a newly started VM, waits up to 240 seconds for
guest tools and an actual Windows command response, verifies `guest.download`
discovery, applies `scripts/set-windows-isolation.ps1` (routing-only isolation;
undo with `-Remove` and renew DHCP), and checks/re-establishes the private SSH
forward to `coop-crowdstrike`. It checks actual RPC responses, not just socket
existence.
Both host and Coop checks execute a fresh random challenge inside Windows and
require its exact echo, successful exit, and no timeout/truncation. Merely
reporting the agent as connected is insufficient. Local guest-command checks
retry during the boot timeout; a failed Coop guest-command check exits nonzero
without replacing a functioning tunnel or resetting Windows.
An existing running lab is reused; an existing paused debugger session is left
alone unless `--resume-existing` is specified. No disks are created or reset.
`--no-coop` checks only the host lab; `--no-isolate` leaves the guest's default
routes alone; `--config`, `--lab`, `--socket`, `--ssh-host`, `--coop-socket`,
and `--timeout` override defaults. Requires Python
3, a built lab binary, host KVM access, and noninteractive SSH access to the
already-running Coop VM. Rebuild if needed with
`cargo build -p vroom-kernel --bins`.

Success prints `READY`; failure exits nonzero with a diagnostic. It does not
install a boot service or continuously monitor/restart the VM. A stale Coop
socket is removed only if owned by the remote user and connection-refused;
a live but broken listener is reported, never replaced automatically.

On the host, with the corresponding `coop-<name>` SSH alias configured:

```sh
ssh -NT -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
  -R /home/ubuntu/vroom-agent.sock:/run/user/1000/vroom-agent.sock \
  coop-crowdstrike
```

Keep this tunnel alive. Inside Coop:

```sh
vroom-rpc --socket /home/ubuntu/vroom-agent.sock describe
vroom-rpc --socket /home/ubuntu/vroom-agent.sock commands.list
vroom-rpc --socket /home/ubuntu/vroom-agent.sock run vm.resume
vroom-rpc --socket /home/ubuntu/vroom-agent.sock run vm.pause
vroom-rpc --socket /home/ubuntu/vroom-agent.sock run kernel.info
vroom-rpc --socket /home/ubuntu/vroom-agent.sock run kernel.symbols
vroom-rpc --socket /home/ubuntu/vroom-agent.sock run kernel.processes
vroom-rpc --socket /home/ubuntu/vroom-agent.sock run kernel.modules
```

Coop needs neither `/dev/kvm` nor a copy of the Windows disk. A listening SSH
socket alone does not prove the backend is alive: `describe` must succeed.
Connection reset commonly means the host lab/socket has stopped. The host socket
is mode 0600 and verifies the peer UID (`SO_PEERCRED` on Linux); SSH must run as
the lab's host user. The endpoint is custom newline-delimited JSON-RPC, **not MCP**.

Treat socket access as debugger authority, not read-only access. Risk metadata
is descriptive, not an authorization policy. The lab runs without the production
VMM sandbox; expose it only to agents you authorize to control this guest, or
place a separately enforced allowlist proxy in front of it. The x64 allowlist
excludes host-side download paths and fuzzer subprocesses. `guest.upload`
follows ARM64 semantics and can read any host file the lab user can read.


### RPC reference (architecture-dependent capabilities)

The catalog below includes ARM64-only operations. On every connection,
`commands.list` is the authoritative list for the running backend.

JSON-RPC 2.0, one object per line, owner-only Unix socket. Same protocol as angr_tui's.

```sh
research/build.sh --release                                                # signed: target/research/release/vroom-lab
research/run.sh LAB.toml --restore SNAPSHOT --agent-socket lab.sock     # beside the TUI
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
- `vm.restart {paused?}` hard-resets the live VM from the reset vector (devices and RAM reset); breakpoints, watches, tracepoints and caches from the previous boot are cleared. With `paused: true`, it stops before the first guest instruction: arm breakpoints, then `vm.resume`. For an early-boot target, use `vm.restart {paused: true}` -> `debug.breakpoints` -> `vm.resume`, and repeat per boot. Disks and the VM config are kept; use a snapshot restore or a new lab for fresh disks/config.
- `guest.exec {command, powershell?, timeout?, max_output?}`: the guest agent runs it (cmd.exe as SYSTEM); answers `{exit_code, timed_out, truncated, stdout, stderr}` when it ends. Older agent: `guest.update_tools` first.
- `guest.upload {host_path, guest_path}`: copy a host file into the guest; answer `{guest_path, size, sha256}`. ARM64 and AMD64 accept an absolute path or one under exports; the path is on the lab host, not the RPC client.
- `guest.download {guest_path, offset?, length?}`: return at most 65536 bytes as hex over RPC, plus whole-file size and SHA-256; see AMD64 notes above. ARM64 additionally accepts explicit `host_path` to save the whole file on the host instead; cannot combine with offset/length. Omitting `host_path` now returns bytes rather than creating an exports file.
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
- macOS/HVF: `target/debug/vroom-lab` is unsigned (cargo tests relink it); use
  the signed `target/research/` builds. Linux/KVM needs no Apple code signature.
- Snapshots are one format everywhere (app, `vroom`, lab, fuzzer), but a snapshot is tied to its architecture and hypervisor backend. `vm.snapshot` saves into the VM's own snapshots (the app lists them) or `path`; `vm.restore` restarts the lab's VM from one (`name` or `path`), paused (`resume: true`: running); `--restore` does it at start. Refused when a disk layer under the snapshot changed since, or when the snapshot backend or saved CPU state does not match. One VM runs once: the lab and the app can't both run it.

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
snapshot [/new/directory]     save the VM (default: its snapshots)
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
