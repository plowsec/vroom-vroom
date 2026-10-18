# HackSys Extreme Vulnerable Driver

First Windows kernel target. Disposable VM only. HEVD source, driver,
checkpoints and guest state stay outside the repo. Pinned upstream:
`b1cc756e55f69530f6468a6c3c310ed7b967bada`. Walkthrough:
[../fuzz/DEMO.md](../fuzz/DEMO.md).

## Build (Windows host, WDK)

```bat
Builder\Build_HEVD_Vulnerable_arm64.bat
```

Install in the guest. [client.c](client.c) opens
`\\.\HackSysExtremeVulnerableDriver` and calls `DeviceIoControl`.

## Checkpoint

Two places to stop; fill the addresses into
[harness.example.json](harness.example.json).

| | call site (before `DeviceIoControl`) | handler entry |
|---|---|---|
| IOCTL | `ioctl_register` (e.g. `x1`) | `ioctl_va` |
| input pointer | input register | `input_pointer_va` |
| input length | length register | `input_length_va` (optional) |
| `stop_pc` | instruction after the call | saved return address |
| always | `input_gpa`, `input_va` | `input_gpa`, `input_va`; clear call-site fields |

Testcase: `u32 LE IOCTL | input bytes`. Block VAs are translated through
the restored page tables, so a harness survives a new checkpoint.

## Blocks and IOCTLs (angr)

```sh
uv sync --project research/tui --extra angr
uv run --project research/tui --extra angr python research/fuzz/tools/angr_blocks.py \
  /path/to/HEVD.sys 0x140001234 \
  --static-base 0x140000000 --runtime-base 0xfffff80612340000 \
  --module HEVD.sys --output research/hevd/blocks.json
uv run --project research/tui --extra angr python research/fuzz/tools/angr_ioctls.py \
  /path/to/HEVD.sys 0x1400870a8 --device-type 0x22 \
  --function-start 0x800 --function-end 0x8ff
```

- `angr_blocks`: from the dispatch function, follows direct in-driver
  calls. `static-base` = file image base, `runtime-base` = module base in
  the guest. Paste rows into `coverage`. No source or symbols needed.
- `angr_ioctls`: static literals only (`0x0022201f`, `0x0022203b`,
  `0x0022205b` here); `probe` finds what reaches new blocks.

## Run

```sh
cargo run -p vroom-fuzz -- check research/hevd/harness.example.json
cargo run -p vroom-fuzz -- run research/hevd/harness.example.json seed.bin --iterations 1000
cargo run -p vroom-fuzz -- probe research/hevd/harness.example.json payload.bin --output runs/ioctl-probe
```

- Coverage: only blocks in the map. Unresolved indirect branches: one HW
  breakpoint rotates over function entries across inputs.
- Bugcheck or timeout: input saved, checkpoint restored.
- `reopen_after: 2` if in-place resets misbehave. `--no-boundary-seeds`
  skips the size seeds.
- `probe`: every IOCTL in range from the checkpoint; `results.jsonl` plus
  coverage for those that differ from the invalid-code baseline.

Harness fields: [../fuzz/README.md](../fuzz/README.md).
