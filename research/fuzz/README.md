# Fuzz lab

Snapshot fuzzing: every input starts from the same snapshot.

## Commands

```sh
cargo build --release -p vroom-fuzz && scripts/sign.sh target/release/vroom-fuzz   # HVF entitlement
vroom-fuzz check harness.json                                   # validate, list blocks
vroom-fuzz probe harness.json payload.bin --output runs/probe   # IOCTL range vs baseline
vroom-fuzz run harness.json payload.bin --ioctl 0x222003 --output runs/a --duration-secs 3600 --tui
vroom-fuzz fleet harness.json payload.bin --ioctl 0x222003 --workers 4 --output runs/f    # local master + 4 workers
vroom-fuzz serve harness.json --listen 0.0.0.0:7979 --output runs/shared --tui            # master for other machines
vroom-fuzz fleet harness.json payload.bin --ioctl 0x222003 --workers 6 --output runs/l --master host:7979
vroom-fuzz run harness.json payload.bin --master host:7979 --state-dir w0.state --output runs/w0   # one worker
vroom-fuzz run harness.json seed.bin --output runs/d --duration-secs 60           # no --ioctl: the seed's code is mutated too
vroom-fuzz cmplog harness.json input.bin                        # one input's comparisons, candidates
vroom-fuzz coverage runs/a/coverage.json runs/a/blocks.json     # per-function summary
vroom-fuzz trace harness.json runs/a/crashes/<hash>-BugCheck.bin --blocks all-blocks.json   # replay, symbolized
vroom-fuzz triage harness.json runs/a/crashes/<hash>-BugCheck.bin   # reproduce + minimize
vroom-fuzz blocks clfs.sys --base 0xfffff804000d0000 --output blocks.json   # block list from the driver file
```

| `triage` flag | |
|---|---|
| `--runs N` | runs of the input as given (3) |
| `--budget N` | executions for minimizing (2000); `0` doesn't |
| `--out FILE`, `--min FILE` | default `<input>.triage.json`, `<input>.min.bin` |

Triage: the most frequent first finding over the runs is the one; minimizing keeps each change that still makes its signature: shortest prefix, zeroed spans (64, 8 bytes), dropped chunks, zeroed bytes. The IOCTL prefix stays. Candidates time out at 20× the input's run (≥250 ms). Report: `signature`, `reproduced`/`runs`, every signature seen, `finding`, lengths, executions. HEVD stack overflow: 2308 → 2096 bytes, 4 non-zero, ~11 s.

| `trace` flag | |
|---|---|
| `--out FILE` | default `<input>.trace.txt` |
| `--max N` | instructions at most (200000; ~0.5 ms each) |
| `--registers` | x0–x30, sp, pstate on every line |
| `--blocks FILE` | function names from this block list too (no PDB: Ghidra's `sub_…`) |
| `--state-dir DIR` | VM state dir (default the harness's) |

Trace file: `N  PC  module!function+0xoff` per instruction from the snapshot to `stop_pc`, the bugcheck or `--max`; then `# end`, `# calls` (indented by depth, repeats `×N`, exceptions inline), `# stack`. Symbols: PDBs from `<vm>.state/symbols` (the lab's cache; the symbol server fills it), else the harness's block names, else exports.

| `run` flag | |
|---|---|
| `--ioctl X` | fixed IOCTL; seed = payload only |
| `--iterations N` / `--duration-secs S` | stop condition (default 100 inputs) |
| `--no-boundary-seeds` | skip the payload-size seeds |
| `--repeat-seed` | replay the seed, rotating function-entry breakpoints |
| `--no-cmplog` | no comparison logging |
| `--tui` | dashboard instead of JSON lines; `q` stops |
| `--master ADDR` `--name` `--state-dir` `--salt` | worker of a master |

Comparison logging (Redqueen-style): each new corpus input runs once stepped through the target's image (≤200 ms); its comparisons (`cmp`, `cmn`, `ccmp`, `cbz`, `tbz`) become inputs that turn them the other way (operand found in the input → the other operand ±1; IOCTL and first payload words shifted by the operands' difference; a sequence: each call's code and first two payload words, only by comparisons whose register side holds the field's word, low half or byte) and dictionary words. Client: the guest runs at full speed to each entry into the target (`entries`, client's address space), 300 target instructions stepped from there, calls out at full speed (≤40k steps, ≤3 s); ≤128 inputs per log. CLFS: 4748 → 5738 blocks in 10 min. Without `--ioctl`, one mutation in four changes the code (dictionary, function ±1–4, method). Parents: novelty × speed (median-relative). HEVD: 29/29 IOCTLs from a zero code in ~0.1 s (blocks mode).

Output dir: `inputs/`, `crashes/<hash>-<class>.{bin,json}` (first input per signature; `stack`: symbolized), `coverage.json`, `blocks.json`, `stats.json` (every 5 s).

Signature (dedupe): class + the 3 innermost functions of the faulting code (offsets dropped; frames above the kernel's exception entry and `KeBugCheck*` skipped); `<wild jump>` for a PC outside every image; else class + PC. Functions without a PDB: the block list's names, else `.pdata` starts as `sub_<address at 0x140000000>`.

## Exceptions

Detector `exceptions` (or `access-violation`): a breakpoint on `nt!KiDispatchException` sees every exception at first chance, before an `__except` swallows it. Reported when the faulting PC or a frame of its stack (unwound from the trap frame) is in the target, or the PC is in no image:

| class | |
|---|---|
| `AccessViolation` | `0xc0000005` on a kernel address or the NULL page (`reading`/`writing`/`executing ADDR`) |
| `ExceptionDispatch` | stack overrun `0xc0000409`, illegal/privileged instruction, misalignment, stack overflow, divide, the trap handlers' other `0x100000xx` codes (`0x10000011`: non-canonical address) |

- User-address faults inside `__try` (probing user buffers) aren't findings.
- Bugchecks stop at `KeBugCheck2` (every bugcheck goes through it; `bugcheck_pc` = `KeBugCheckEx` is moved there) with the exception's stack.
- The same exception (code, address) 9 times in one input ends it: a fault in its own dispatch.
Kernel TUI: `coverage runs/a/coverage.json`, `coverage-diff runs/a/coverage.json runs/b/coverage.json`.

## Harness fields

| field | |
|---|---|
| `config`, `snapshot`, `state_dir` | VM config, snapshot dir, VM state dir (one per instance) |
| `cpu`, `isolate_cpu` | CPU of the target call; run it alone, IRQs masked, devices paused (~150x faster) |
| `input_gpa`, `input_capacity`, `input_va`, `max_input` | input buffer |
| `ioctl_prefix`, `ioctl_register` / `ioctl_va` | first 4 input bytes = IOCTL, and where it goes |
| `input_register` / `input_pointer_va`, `input_length_register` / `input_length_va`, `output_length_va` | saved fields patched per input |
| `entry_pc`, `stop_pc` | first block; return address that ends an input (testcase thread only). User-mode `entry_pc`: a client harness (needs `blocks`, no `isolate_cpu`, `target`, `stop_pc` in the client) |
| `target` | module under test (`clfs.sys`); default the image of `entry_pc` |
| `entries` | client: ≤3 target functions its calls enter by (IRP dispatch), names from the block list (substring); cmplog steps from there. None: from the first new block the client's thread reaches |
| `sequence` | input = calls `u32 code \| u16 length \| payload` for a client; mutations insert, drop, repeat, swap, splice calls |
| `bugcheck_pc` | `KeBugCheckEx` VA: crash = finding + in-place reset (~1 ms, not ~11 s); 5th HW breakpoint |
| `reset_is_bugcheck` | guest reset counts as a bugcheck |
| `reopen_after` | `0` reset in place, `1` restore from disk every input |
| `timeout_ms` | per input; a timeout restores from disk |
| `coverage` / `coverage_file` | block manifest (`angr_blocks.py`); ≤4 successors per block |
| `coverage_mode` | `edges` (default): HW breakpoints follow the path, edges + hit counts, one exit per block. `blocks`: one-shot BRK per uncovered block, known paths run exit-free, new blocks only (~1.8x faster; not for `probe`) |
| `exception_sites` | ≤4 exception-dispatch PCs, reported when reached |
| `watchpoints` | 1 range ≤8 bytes: `address`/`input_end_offset`, `read`, `write`, `poisoned`, `poison_value`, `double_fetch` |
| `ioctl_probe` | `device_type`, `function_start..end`, `methods`, `access`, `baseline_ioctl` |

## Notes

- `VROOM_COW_VERIFY=1`: after each reset, check RAM, untouched devices and GIC registers against the snapshot (~1 s each).
- `VROOM_FUZZ_TRACE=1`: per-stop trace on stderr.
- `VROOM_FUZZ_TOKEN`: master/worker shared secret. Plaintext protocol: trusted network or SSH tunnel.
- Distributed: every machine needs the snapshot, its disk layers and the VM config at the same paths; the master refuses other harness names or block counts.
- Workers import a shared input (with its coverage, not re-run) only if it is new to them.
- Reset with `isolate_cpu` (HEVD): dirty pages only, pages written every input stay writable (hot), device restore skipped if no MMIO, GIC restored per register: ~0.5 ms.
- `blocks` mode: BRK `#0x766d` in RAM and the snapshot baseline (other BRKs still reach the guest; HVF only). Pageable code not mapped at the snapshot is planted once an input maps it. Shared inputs remove their blocks' BRKs.
- HEVD, M3 Max, edges: ~750 inputs/s per worker, ~545/s each with 3 (1640/s). Blocks: ~1360/s, ~950/s each with 3 (2840/s). ~9 GB RAM and ~10 s start per worker.

## HEVD

`0x222003` = stack overflow; payloads >2048 bytes bugcheck `0x1e` at `0x4141414141414141`.

```bat
C:\HEVD-Lab\DebugView\dbgviewcli64a.exe --accepteula --kernel --verbose-kernel --no-banner --duration 30 --log C:\HEVD-Lab\hevd-stack.log
sc.exe start HEVD
C:\HEVD-Lab\HevdFuzzClient.exe 0x222003 1
```

## Clients, sequences

Stateful drivers (CLFS): the snapshot stands in a user-mode client parked in a loop; each input is a program of calls it makes (`sequence`), the whole guest running; the input ends at the client's `stop_pc`. Coverage is the target's blocks (`blocks` mode, any thread); exceptions are the target's, any thread. Cmplog: from each entry into the target (`entries`). Example: [CLFS.md](CLFS.md).

## Blocks

`vroom-fuzz blocks FILE --base RUNTIME_BASE [--module NAME] [--symbols DIR] --output blocks.json`: every `.pdata` function swept; blocks start at the function, branch targets, after conditional branches (not after `b`/`br`/`ret`: switch tables live in code, a BRK there corrupts them); names from the PDB (symbol server), else `sub_<VA>`. clfs.sys: 18003 blocks, 1305 functions, ~2 s.

angr (one function and its callees, CFG successors for `edges` mode):

```sh
uv sync --project research/tui --extra angr
uv run --project research/tui --extra angr python research/fuzz/tools/angr_blocks.py \
  HEVD.sys 0x140001234 --static-base 0x140000000 \
  --runtime-base 0xfffff80612340000 --module HEVD.sys --output blocks.json
```
