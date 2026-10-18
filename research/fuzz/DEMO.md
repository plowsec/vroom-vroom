# HEVD demo

vroom-fuzz against HEVD on Windows 11 ARM64 (HVF, M3 Max). One checkpoint at the IOCTL handler, every input starts from it.
TUI commands: `:` opens the command bar; they take literal paths (no `$VM`).

## 0. Every terminal

```bash
cd /Users/vladimir/ctf/vroom-vroom
VM=/Users/vladimir/VMs/hevd-windows-arm64
CK=$VM/checkpoints                  # read only: never write here
OUT=$VM/demo                        # all demo output
F=target/release/vroom-fuzz
mkdir -p $OUT
```

## 1. Build + sign

```bash
cargo build --release -p vroom-fuzz && scripts/sign.sh target/release/vroom-fuzz   # fuzzer, HVF entitlement
research/build.sh                                  # lab backend: target/research/debug/vroom-lab (signed)
scripts/build.sh                                   # vroom CLI (+ app, netd): target/debug/vroom (signed)
uv sync --project research/tui --extra angr        # kernel TUI, angr_tui (atui)
```

`research/run.sh` runs `research/build.sh` itself; it refuses while a lab runs from `target/`.

## 2. Checkpoint (prep, already done: `hevd-dispatch-buffer`)

1. VM from VMware (`stage_vm.py`: plan only without `--copy`; external disk to internal):

```bash
python3 research/kernel/tools/stage_vm.py SRC.vmwarevm /Users/vladimir/VMs/staged.vmwarevm --copy
target/debug/vroom import "/Users/vladimir/Virtual Machines.localized/Windows 11 64-bit Arm template.vmwarevm" \
  --dir /Users/vladimir/VMs --name hevd-windows-arm64
```

2. Then in `hevd-windows-arm64.toml`: tools disk (`readonly = true`), `[[share]]` `$VM/shared` tag `hevd-lab` (Z: in the guest, HEVD source at `Z:\HEVD`), NAT port forward 2222 to 22.
3. Tools disk (agent, ARM64 drivers, WinFsp for the share):

```bash
WINFSP_MSI=target/research/hevd-stage/winfsp-2.2.26286.msi scripts/make-windows-tools.sh \
  target/research/hevd-stage/windows-tools.raw target/research/hevd-stage/virtio-win-0.1.302.iso
```

4. Guest prep, normal boot, once (`vmdrive.py`: commands on the `.cmd` FIFO, `shot` PNG path in `.log`):

```bash
python3 scripts/vmdrive.py target/debug/vroom $VM/hevd-windows-arm64.toml $VM/hevd-windows-arm64.state &
printf '%s\n' shot > $VM/hevd-windows-arm64.state.cmd
printf '%s\n' 'type D:\vroom-setup.cmd\n' > $VM/hevd-windows-arm64.state.cmd     # \n = Enter
printf '%s\n' quit > $VM/hevd-windows-arm64.state.cmd
```

```bat
D:\vroom-setup.cmd
bcdedit /set testsigning on
sc.exe create HEVD type= kernel start= demand binPath= C:\HEVD-Lab\HEVD.sys
Z:\HEVD\Tools\Windows\Build_HEVD_Fuzz_Client_arm64.bat
```

- `bcdedit` and `sc.exe create` lines, and where `HEVD.sys` sits in the guest: (check). Driver build: `Z:\HEVD\Builder\Build_HEVD_Vulnerable_arm64.bat` (WDK). Host copy for angr: `$VM/HEVD.sys`.
- The client .bat writes `C:\HEVD-Lab\HevdFuzzClient.exe` (needs VS ARM64 tools in the guest).

5. Lab boot (boots paused) + viewer (second terminal, after the lab is up):

```bash
research/run.sh $VM/hevd-windows-arm64.toml                     # F5 boots Windows
python3 scripts/vmattach.py $VM/hevd-windows-arm64.state &      # guest screen + keyboard
A=$VM/hevd-windows-arm64.state.attach.cmd
printf '%s\n' shot > $A && open $VM/hevd-windows-arm64.state.attach.png
printf '%s\n' 'click X Y' > $A                                  # X Y from the shot: an admin cmd window
printf '%s\n' 'type sc.exe start HEVD\n' > $A
```

6. Break at the IOCTL handler entry (`sub_1400870a8`, RVA 0x870a8):

```text
F6                                                                  pause
F4                                                                  kernel, PDB symbols, modules
image HEVD.sys /Users/vladimir/VMs/hevd-windows-arm64/HEVD.sys      log: runtime_base (decimal) = HEVD base
bp 0xfffff80275ff70a8                                               base + 0x870a8 (this boot's value)
F5
```

```bat
C:\HEVD-Lab\HevdFuzzClient.exe 0x222003 1
```

7. Stops `Breakpoint / CPU 1`. Read the fields, save:

```text
stack                                         frame 0 HEVD.sys+0x870a8
x nt!KeBugCheckEx                             bugcheck_pc
mem 0xffffc50aa8c7dd10 48                     IrpSp = X1 + 0xd0: +0x10 length, +0x18 IOCTL, +0x20 buffer
checkpoint /Users/vladimir/VMs/hevd-windows-arm64/checkpoints/hevd-dispatch-NEW
```

8. Harness fields (JSON wants decimal: `python3 -c 'print(0xfffff80275ff70a8)'`):

| field | from | this checkpoint |
|---|---|---|
| `cpu` | CPU in the stop status | `1` |
| `entry_pc` | breakpoint = checkpoint PC | `0xfffff80275ff70a8` |
| `stop_pc` | X30 at entry (return into nt) | `0xfffff80278446948` |
| `bugcheck_pc` | `x nt!KeBugCheckEx` | `0xfffff8027845bb70` |
| `ioctl_va` | IrpSp+0x18 IoControlCode (X1+0xe8) | `0xffffc50aa8c7dd28` |
| `input_pointer_va` | IrpSp+0x20 Type3InputBuffer (X1+0xf0) | `0xffffc50aa8c7dd30` |
| `input_length_va` | IrpSp+0x10 InputBufferLength (X1+0xe0): the size HEVD copies | `0xffffc50aa8c7dd20` |
| `input_va` | value at `input_pointer_va` (client heap) | `0x1f93110d020` |
| `input_capacity` | `0x1000 - (input_va & 0xfff)`: one page | `4064` |
| `input_gpa` | run once with a wrong one, the error prints it | `0x159df5020` |
| `max_input` | 4-byte IOCTL + payload | `5` safe, `4068` overflow |
| `coverage_file` | `angr_blocks.py` | `$VM/hevd-all-driver-blocks.json` |

- IrpSp: +0x08 OutputBufferLength, +0x10 InputBufferLength, +0x18 IoControlCode, +0x20 Type3InputBuffer; +0x14 is padding.
- `input_gpa`: any RAM address (0x40000000 = RAM base) as placeholder:

```bash
jq '. + {input_gpa: 1073741824}' $CK/hevd-harness-all-safe.json > $OUT/gpa.json
$F run $OUT/gpa.json $CK/one-byte-payload.bin --ioctl 0x222003 --iterations 1 --no-boundary-seeds \
  --state-dir $OUT/state-x --output $OUT/gpa
# Error: input_va 0x1f93110d020 resolves to 0x159df5020, not input_gpa 0x40000000
```

9. Blocks + IOCTLs (angr). Same 600 blocks / 49 functions as `$VM/hevd-all-driver-blocks.json` (~20 s):

```bash
uv run --project research/tui --extra angr python research/fuzz/tools/angr_blocks.py \
  $VM/HEVD.sys 0x1400870a8 --static-base 0x140000000 \
  --runtime-base 0xfffff80275f70000 --module HEVD.sys --output $OUT/blocks.json
uv run --project research/tui --extra angr python research/fuzz/tools/angr_ioctls.py \
  $VM/HEVD.sys 0x1400870a8 --device-type 0x22 --function-start 0x800 --function-end 0x8ff
```

`angr_ioctls`: static literals only (`0x22201f`, `0x22203b`, `0x22205b`); `probe` finds the rest.

## 3. Fuzz (live)

| harness | |
|---|---|
| `$CK/hevd-harness-all-safe.json` | 5-byte inputs, no crash, fastest |
| `$CK/hevd-harness-all-30s.json` | 4068-byte inputs, finds the BugCheck (`state_dir`: `$VM/fuzz.state`) |

Seed `$CK/one-byte-payload.bin` (one `00`), IOCTL `0x222003` (stack overflow).

1. Check:

```bash
$F check $CK/hevd-harness-all-safe.json
# hevd-arm64-dispatch-all-safe: 600 blocks, input GPA 0x159df5020, 3 detectors
# IOCTL probe: 116 candidates, baseline 0xffffffff
```

2. Probe IOCTLs 0x800..0x873, METHOD_NEITHER (116 in 0.2 s; 29 differ from the baseline, `0x222003`..`0x222073`):

```bash
$F probe $CK/hevd-harness-all-safe.json $CK/one-byte-payload.bin --output $OUT/probe
head -3 $OUT/probe/results.jsonl
```

- New `--output` each time; edges mode only; `--state-dir` optional.

3. Run, edges (default), dashboard:

```bash
$F run $CK/hevd-harness-all-safe.json $CK/one-byte-payload.bin --ioctl 0x222003 \
  --duration-secs 60 --tui --state-dir $OUT/state-a --output $OUT/edges
```

- Dashboard: progress, coverage (blocks/600, functions/49), throughput (exec/s, exec ms, reset ms), findings, per-function bars. `q` / Esc / Ctrl-C: stop and save.
- ~10 s start (2.4 GB checkpoint), then inputs.

4. Blocks mode (one-shot BRK per uncovered block):

```bash
jq '. + {coverage_mode: "blocks"}' $CK/hevd-harness-all-safe.json > $OUT/blocks-safe.json
$F run $OUT/blocks-safe.json $CK/one-byte-payload.bin --ioctl 0x222003 \
  --duration-secs 60 --tui --state-dir $OUT/state-b --output $OUT/blocks
```

| speed field | |
|---|---|
| `reopen_after: 0` | reset in place (dirty pages only), never restore from disk |
| `isolate_cpu: true` | target CPU alone, IRQs masked, devices paused: reset ~0.5 ms |
| `coverage_mode` | `edges`: path + hit counts, one exit per block. `blocks`: known paths run exit-free, new blocks only |
| `bugcheck_pc` | `KeBugCheckEx` breakpoint: crash = finding + in-place reset |

| HEVD 0x222003 | |
|---|---|
| baseline (restore from disk per input) | 416 exec/**hour** |
| edges | ~730 exec/s, 20 blocks |
| blocks | ~1,330 exec/s, 26 blocks (6 edges misses) |
| 3 workers, blocks | ~2,800 exec/s |
| crash | <1 ms to the bugcheck, ~0.5 ms reset (was ~11 s) |

5. Show a crash:

```bash
$F run $CK/hevd-harness-all-30s.json $CK/one-byte-payload.bin --ioctl 0x222003 \
  --duration-secs 30 --tui --state-dir $OUT/state-c --output $OUT/crash
ls $OUT/crash/crashes
cat $(ls $OUT/crash/crashes/*-BugCheck.json | head -1)
xxd $(ls $OUT/crash/crashes/*-BugCheck.bin | head -1) | head -2
```

- Boundary seeds 3072 and 4064 (`AAAA…`) overflow the 0x800 stack buffer within the first inputs: `bugcheck 0x1e (0xffffffffc00000aa, 0x4141414141414141, …)`.
- `.bin` = 4-byte IOCTL `03 20 22 00` + payload. One unique finding, many crashing inputs.
- In the guest the same request (`HevdFuzzClient.exe 0x222003 4096`) resets Windows: ~3 s to the reset + ~8 s disk restore per crash without `bugcheck_pc`.
- Replay (no `--ioctl`: the file carries it):

```bash
$F run $CK/hevd-harness-all-30s.json $(ls $OUT/crash/crashes/*-BugCheck.bin | head -1) \
  --no-boundary-seeds --iterations 1 --state-dir $OUT/state-c --output $OUT/replay
```

6. Correctness: RAM, untouched devices and GIC registers checked against the checkpoint after every reset (~1 s per input). Silent = pass; a mismatch stops the run and lists the pages.

```bash
VROOM_COW_VERIFY=1 $F run $CK/hevd-harness-all-30s.json $CK/one-byte-payload.bin --ioctl 0x222003 \
  --iterations 20 --state-dir $OUT/state-c --output $OUT/verify
```

7. Workers (one VM each, ~9 GB). Fuzz VMs drop host port forwards and keep their own EFI vars, so they run beside each other and beside the lab.

```bash
jq '. + {coverage_mode: "blocks"}' $CK/hevd-harness-all-safe.json > $OUT/fleet.json
$F fleet $OUT/fleet.json $CK/one-byte-payload.bin --ioctl 0x222003 --workers 3 \
  --duration-secs 60 --output $OUT/fleet                                       # local master + dashboard
```

```bash
$F serve $OUT/fleet.json --listen 0.0.0.0:7979 --output $OUT/shared --tui     # master
$F run $OUT/fleet.json $CK/one-byte-payload.bin --ioctl 0x222003 \
  --master HOST:7979 --name w0 --state-dir $OUT/state-w0 --output $OUT/w0     # worker, any machine
```

- Other machines: same checkpoint, disk layers and config at the same paths; `VROOM_FUZZ_TOKEN` on both.

8. Output (`--output`):

| | |
|---|---|
| `inputs/<sha256>` | corpus (IOCTL prefix included) |
| `crashes/<hash>-BugCheck.{bin,json}` | input + bugcheck code, args, caller |
| `coverage.json` | block hits, edges, first input per block |
| `blocks.json` | block manifest with resolved GPAs |
| `stats.json` | every 5 s |
| `workers/<i>/worker.log` | `fleet` only |

## 4. Coverage TUI (Lighthouse-style)

1. Start on the checkpoint (paused, HEVD mapped: live disassembly). Don't F5.

```bash
research/run.sh $VM/hevd-windows-arm64.toml --restore $CK/hevd-dispatch-buffer
```

- The lab uses `$VM/hevd-windows-arm64.state`; the harnesses use `fuzz.state` / `fuzz-coverage-run2.state`.
- No VM (tables only, no disassembly): `uv run --project research/tui vroom-kernel $VM/hevd-windows-arm64.toml --backend /usr/bin/false`, ignore `FAILED`.

2. One run:

```text
coverage /Users/vladimir/VMs/hevd-windows-arm64/demo/edges/coverage.json
```

3. Diff, one IOCTL vs all 29 (`20/600 → 388/600 blocks · +368 new · -0 lost`):

```text
coverage-diff /Users/vladimir/VMs/hevd-windows-arm64/demo/edges/coverage.json /Users/vladimir/VMs/hevd-windows-arm64/demo/probe/coverage.json
```

4. Diff, edges vs blocks mode (`20/600 → 26/600 · +6 new`):

```text
coverage-diff /Users/vladimir/VMs/hevd-windows-arm64/demo/edges/coverage.json /Users/vladimir/VMs/hevd-windows-arm64/demo/blocks/coverage.json
```

| key | |
|---|---|
| `:` | command bar; ↑ / ↓ history |
| ↑ ↓ PgUp PgDn | move in a table |
| Enter on a function | its blocks in the lower table |
| Enter on a block | its disassembly, in the Code tab |
| `dis 0xVA` | same, switches to Code |
| F3 | angr C beside the assembly (Live/File button) |
| F10 | code full width |
| Ctrl+G / `x` | CFG / xrefs of the current function |
| `u` / `U` | back / forward |
| Ctrl+Q | quit (powers the lab VM off) |

| point at | |
|---|---|
| title | covered/600 blocks, runs, edges; diff: `a → b · +new · -lost` |
| functions table | bar + %, blocks, hits; least covered first, hot ones at the bottom |
| `sub_1400870a8` | IOCTL dispatch: 8/125 with one IOCTL |
| `sub_140088a30` → `sub_140088a60` | 0x222003 handler → overflow trigger (0x800 stack buffer, user-sized copy): 10/10 |
| blocks table | Hits 0 = uncovered; Change `new`/`lost`; First input = `inputs/<hash>` prefix |
| blocks mode | hits are first-hit only; compare blocks, not counts |

Optional, path inspection in angr_tui (`a` goto, space graph, `d` decompile, `/` search, `q` quit):

```bash
uv run --project research/tui --extra angr atui $VM/HEVD.sys -s 0x140088a60
```

## 5. Agents (optional)

Same lab, piloted over JSON-RPC (angr_tui's protocol). Full list: `research/README.md` → agents.

```bash
research/run.sh $VM/hevd-windows-arm64.toml --restore $CK/hevd-dispatch-buffer --agent-socket lab.sock
# second terminal, same directory:
cargo build --release -p vroom-kernel --bin vroom-rpc; R() { target/release/vroom-rpc --socket lab.sock "$@"; }
R describe
R run kernel.info '{"cpu":1}'; R run kernel.symbols '{"cpu":1}'  # once
R run kernel.stack '{"cpu":1}'                                  # HEVD handler ← IofCallDriver
R run debug.breakpoints '{"addresses":["0xfffff80275ff70c8"]}'
R run vm.resume; R events.poll '{"wait_ms":10000}'             # the TUI stops too
R jobs.start '{"command":"fuzz.run","args":{"harness":"'$CK'/hevd-harness-all-safe.json","seed":"'$CK'/one-byte-payload.bin","output":"'$OUT'/agent","ioctl":"0x222003","iterations":2000}}'
R jobs.status '{"handle":"job-..."}'
R run fuzz.coverage '{"run":"'$OUT'/agent"}'
```
