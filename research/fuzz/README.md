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
vroom-fuzz coverage runs/a/coverage.json runs/a/blocks.json     # per-function summary
```

| `run` flag | |
|---|---|
| `--ioctl X` | fixed IOCTL; seed = payload only |
| `--iterations N` / `--duration-secs S` | stop condition (default 100 inputs) |
| `--no-boundary-seeds` | skip the payload-size seeds |
| `--repeat-seed` | replay the seed, rotating function-entry breakpoints |
| `--tui` | dashboard instead of JSON lines; `q` stops |
| `--master ADDR` `--name` `--state-dir` `--salt` | worker of a master |

Output dir: `inputs/`, `crashes/<hash>-<class>.{bin,json}`, `coverage.json`, `blocks.json`, `stats.json` (every 5 s).
Kernel TUI: `coverage runs/a/coverage.json`, `coverage-diff runs/a/coverage.json runs/b/coverage.json`.

## Harness fields

| field | |
|---|---|
| `config`, `snapshot`, `state_dir` | VM config, snapshot dir, VM state dir (one per instance) |
| `cpu`, `isolate_cpu` | CPU of the target call; run it alone, IRQs masked, devices paused (~150x faster) |
| `input_gpa`, `input_capacity`, `input_va`, `max_input` | input buffer |
| `ioctl_prefix`, `ioctl_register` / `ioctl_va` | first 4 input bytes = IOCTL, and where it goes |
| `input_register` / `input_pointer_va`, `input_length_register` / `input_length_va`, `output_length_va` | saved fields patched per input |
| `entry_pc`, `stop_pc` | first block; return address that ends an input (testcase thread only) |
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

## Blocks (angr)

```sh
uv sync --project research/tui --extra angr
uv run --project research/tui --extra angr python research/fuzz/tools/angr_blocks.py \
  HEVD.sys 0x140001234 --static-base 0x140000000 \
  --runtime-base 0xfffff80612340000 --module HEVD.sys --output blocks.json
```
