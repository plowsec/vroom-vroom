# Fuzz lab

Use saved VM states to test drivers. Each input starts from the same checkpoint.

## Run

Set the VM, checkpoint, input buffer and block list in the harness JSON. For a handler checkpoint, set the saved IOCTL and buffer-pointer fields, plus every length field the target reads.

```sh
cargo run -p vroom-fuzz -- check harness.json
cargo run -p vroom-fuzz -- probe harness.json payload.bin --output runs/probe
cargo run -p vroom-fuzz -- run harness.json payload.bin --ioctl 0x222003 --iterations 100 --output runs/test
```

`probe` tries the IOCTL range in the harness and compares blocks with its invalid-code baseline. `run --ioctl` fixes the code and mutates only the payload. Runs try common payload sizes first; `--no-boundary-seeds` skips them. `--repeat-seed` replays the seed while rotating hardware breakpoints across function entries. The mutator favors corpus inputs that recently add blocks, edges, or findings, then spends that score over time. `--duration-secs 3600` runs for an hour instead of a fixed input count. Set `reopen_after` in the harness to restore the checkpoint after a fixed number of inputs; use `1` if repeated COW resets become unreliable. After a timeout or guest reset, the runner saves the finding and restores the checkpoint before continuing. Results and completion counts are JSON under the output directory. Use a new output directory for each run.

For extra signals, add driver-specific `exception_sites` addresses to stop when the kernel dispatches an exception. This only confirms that dispatch was reached; it does not prove the exception was caught. Add a `watchpoints` entry with `address`, `size`, `read`, and `write` to watch memory. Set `input_end_offset: 0`, `poison_value: 241`, and `poisoned: true` to poison and watch the first byte after each input. Set `double_fetch: true` with `read: true` to report its second read. Hardware watches cover at most 8 bytes in one aligned 8-byte range. This does not discover allocators or maintain a full shadow map automatically.

The runner records blocks and edges for each input. It steps over short direct-call blocks when hardware breakpoints miss a callee entry.

Open results in the kernel TUI:

```text
coverage runs/test/coverage.json
coverage-diff runs/a/coverage.json runs/b/coverage.json
```

## HEVD

The handler-entry harness and checkpoint stay outside the repository. The dynamic probe found `0x222003` by dispatcher coverage. A 1-byte request prints the stack-overflow handler messages and returns. A 4096-byte request resets the disposable guest.

Start capture in one Administrator Command Prompt:

```bat
C:\HEVD-Lab\DebugView\dbgviewcli64a.exe --accepteula --kernel --verbose-kernel --no-banner --duration 30 --log C:\HEVD-Lab\hevd-stack.log
```

In a second Administrator Command Prompt, start HEVD and send a 1-byte request:

```bat
sc.exe start HEVD
C:\HEVD-Lab\HevdFuzzClient.exe 0x222003 1
```

## angr_tui

Optional. Install it to export block addresses or inspect a path after a run. It does not choose IOCTLs for the dynamic probe.

```sh
uv sync --project research/tui --extra angr
uv run --project research/tui --extra angr python research/fuzz/tools/angr_blocks.py \
  HEVD.sys 0x140001234 --static-base 0x140000000 \
  --runtime-base 0xfffff80612340000 --module HEVD.sys --output blocks.json
```
