# HackSys Extreme Vulnerable Driver

HEVD is the first Windows kernel target for the vroom research runner. The
target is intentionally vulnerable and belongs in an isolated, disposable VM.
Keep its source, signed driver and guest state outside this repository.
The manifest example is pinned to upstream revision
`b1cc756e55f69530f6468a6c3c310ed7b967bada`.

The current upstream tree has a Windows ARM64 build. Build the vulnerable
configuration on a Windows host with the WDK:

```bat
Builder\Build_HEVD_Vulnerable_arm64.bat
```

Install the resulting driver in the guest, run a small client that opens
`\\.\HackSysExtremeVulnerableDriver`, and stop the VM immediately before the
`DeviceIoControl` call. The checkpoint must record the paused CPU, the physical
input page (`input_gpa`), the user virtual input pointer (`input_va`) and the
instruction after the call (`stop_pc`). Those three addresses are specific to
the guest and must be filled into [harness.example.json](harness.example.json).
The small [client.c](client.c) is provided to make that call-site setup
repeatable.

The harness can start in either of two useful places. A call-site checkpoint
uses `ioctl_register` (for example `x1`) and places the testcase buffer in the
configured input register. A handler-entry checkpoint uses `ioctl_va` for the
IOCTL field and `input_pointer_va` for the saved buffer pointer. Set
`input_length_va` if the saved state has an input-length field to update. These
are guest virtual addresses in the saved IRP or other target state. The runner
patches them for each testcase; `input_va` is the address of the buffer they
point to. Clear the call-site register fields in this mode. Use the saved
handler return address as `stop_pc`. Keep these addresses in the harness file.

The testcase format is:

```text
u32 little-endian IOCTL | arbitrary input bytes
```

The runner writes the payload to the saved input page. For a call-site
checkpoint it also writes the IOCTL, input pointer and input length to the
configured ARM64 registers. For a handler-entry checkpoint, configure the
saved IOCTL and input field addresses in the harness. Basic-block
addresses come from angr and are translated through the restored guest page
tables when the campaign opens. The same harness can therefore be reused
with another checkpoint even if the guest's physical page placement changes.

Install the optional reverse-engineering dependency once:

```sh
uv sync --project research/tui --extra angr
```

Export blocks from the dispatch function identified during analysis. The
exporter also follows direct calls that stay inside the driver image, so the
map can include handler code. Indirect targets depend on what angr_tui can
resolve. `static-base` is the image base in the driver file and `runtime-base`
is the module base recorded from the paused guest:

```sh
uv run --project research/tui --extra angr python research/fuzz/tools/angr_blocks.py \
  /path/to/HEVD.sys 0x140001234 \
  --static-base 0x140000000 --runtime-base 0xfffff80612340000 \
  --module HEVD.sys --output research/hevd/blocks.json
```

Paste the generated rows into the harness's `coverage` array. The exporter
uses angr_tui's JSON CFG output; vroom resolves runtime VAs to guest physical
addresses from the checkpoint. No HEVD source or debug symbols are required.
The driver binary and dispatch-function address are sufficient.

Find IOCTL candidates referenced by the dispatch function:

```sh
uv run --project research/tui --extra angr python research/fuzz/tools/angr_ioctls.py \
  /path/to/HEVD.sys 0x1400870a8 --device-type 0x22 \
  --function-start 0x800 --function-end 0x8ff
```

On the installed HEVD binary, this found `0x0022201f`, `0x0022203b`, and
`0x0022205b`. These are static candidates; `probe` checks which ones reach
different blocks from the saved guest state.

```sh
cargo run -p vroom-fuzz -- check research/hevd/harness.example.json
cargo run -p vroom-fuzz -- run research/hevd/harness.example.json seed.bin --iterations 1000
cargo run -p vroom-fuzz -- probe research/hevd/harness.example.json payload.bin --output runs/ioctl-probe
```

Coverage is limited to blocks in the exported map. Direct in-driver calls are
followed by default. Because drivers often dispatch through unresolved
indirect branches, the runner also rotates one hardware breakpoint across
function entries on successive inputs. Set `reopen_after` to a small number
such as `2` if repeated copy-on-write resets become unreliable on the host.
After a bugcheck or timeout, it records the input and restores the clean
checkpoint before continuing. Use
`--no-boundary-seeds` to skip the default payload-size test inputs.
The probe command tries the configured IOCTL range from the same checkpoint
for every value. It writes `results.jsonl` and separate coverage files for
values that differ from the configured invalid-code baseline.
