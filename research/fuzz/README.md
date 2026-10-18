# Fuzz lab

Use saved VM states to test drivers. Each input starts from the same checkpoint.

## Run

Set the VM, checkpoint, input buffer and block list in the harness JSON. For a handler checkpoint, set the saved IOCTL and buffer-pointer fields, plus every length field the target reads.

```sh
cargo run -p vroom-fuzz -- check harness.json
cargo run -p vroom-fuzz -- probe harness.json payload.bin --output runs/probe
cargo run -p vroom-fuzz -- run harness.json payload.bin --ioctl 0x222003 --iterations 100 --output runs/test
```

`probe` tries the IOCTL range in the harness and compares blocks with its invalid-code baseline. `run --ioctl` fixes the code and mutates only the payload. Runs try common payload sizes first; `--no-boundary-seeds` skips them. `--repeat-seed` replays the seed while rotating hardware breakpoints across function entries. The mutator favors corpus inputs that recently add blocks, edges, or findings, then spends that score over time. `--duration-secs 3600` runs for an hour instead of a fixed input count. Set `reopen_after` in the harness to restore the checkpoint after a fixed number of inputs; `1` restores it from disk for every input (seconds each), `0` keeps one VM and resets it in place. After a timeout or guest reset, the runner saves the finding and restores the checkpoint before continuing (`reopen` in the result). Results and completion counts are JSON under the output directory. Use a new output directory for each run.

For extra signals, add driver-specific `exception_sites` addresses to stop when the kernel dispatches an exception. This only confirms that dispatch was reached; it does not prove the exception was caught. Add a `watchpoints` entry with `address`, `size`, `read`, and `write` to watch memory. Set `input_end_offset: 0`, `poison_value: 241`, and `poisoned: true` to poison and watch the first byte after each input. Set `double_fetch: true` with `read: true` to report its second read. Hardware watches cover at most 8 bytes in one aligned 8-byte range. This does not discover allocators or maintain a full shadow map automatically.

The runner records blocks and edges for each input. It steps over short direct-call blocks when hardware breakpoints miss a callee entry.

Open results in the kernel TUI:

```text
coverage runs/test/coverage.json
coverage-diff runs/a/coverage.json runs/b/coverage.json
```

## Dashboard

`run --tui` (and `serve --tui`, `fleet`) replaces the JSON lines with a live screen: run time, time since the last new block or edge and since the last new finding, corpus size, blocks reached out of the manifest, edges, execs/s now and on average with a 10-minute history, guest and reset time per input, checkpoint restores from disk, timeouts, threads stepped past shared sites, the unique findings (bugcheck code and parameters), one row per instance, and how many blocks of each function were reached. `q`, Esc or Ctrl-C stop the campaign cleanly. Every run also writes `stats.json` in its output directory every 5 s, for scripts or a dashboard elsewhere.

## Parallel and distributed

HVF runs one VM per process, so parallel fuzzing means processes. One master holds the shared corpus; workers fuzz and synchronize with it about twice a second over TCP:

```sh
# This machine: a master and four workers, with the dashboard.
vroom-fuzz fleet harness.json payload.bin --ioctl 0x222003 --workers 4 --output runs/fleet --duration-secs 3600

# Several machines: one master, then fleets that join it.
export VROOM_FUZZ_TOKEN=some-secret
vroom-fuzz serve harness.json --listen 0.0.0.0:7979 --output runs/shared --tui
vroom-fuzz fleet harness.json payload.bin --ioctl 0x222003 --workers 6 --output runs/local --master master-host:7979
```

A worker sends the inputs it found novel, with the blocks and edges they reached and its new findings. The master keeps those that add a block, an edge or a finding signature campaign-wide, saves them and the crashes under its output, and hands them to the other workers. A worker takes the coverage instead of running the input (every worker restores the same checkpoint, so the input would reach the same blocks), but only if it is new to that worker, and then mutates it like its own finds. Each worker has its own VM state directory (`--state-dir`), its own output under `workers/<i>` and a random mutator salt. `run --master ADDR` starts a single worker by hand. A master restarted on the same output hands out the saved inputs again, and workers run those. Workers keep fuzzing while the master is unreachable and reconnect. The master tells them to stop at `--duration-secs` or `q`.

The master binds to 127.0.0.1 unless `--listen` says otherwise. The protocol is plain JSON lines without encryption: on other machines, set the same `VROOM_FUZZ_TOKEN` on the master and the workers, and keep it to a trusted network or an SSH tunnel. Every machine needs the checkpoint, the VM config's disks and the harness at the paths the harness and the checkpoint manifest name, and the harness must have the same name and block list as the master's (the master refuses others). Each worker holds about 9 GB for the 8 GiB HEVD VM and takes about 10 s to restore the checkpoint. On an M3 Max, one worker runs about 105 inputs/s and three run about 90 each (270/s).

## Speed

The VM is reset in place between inputs: the hypervisor logs the pages the guest writes (write-protected on stage 2, HVF), RAM logs what host code writes, and a reset copies only those pages back from the frozen checkpoint. Backends without the log remap all RAM instead. `VROOM_COW_VERIFY=1` compares all RAM with the checkpoint after every reset and stops on any difference; it is slow (about a second for 8 GiB), so use it to check a new target, not to fuzz.

Set `isolate_cpu: true` for a handler checkpoint: each input then runs on `cpu` alone, with its interrupts masked and the other CPUs parked. The target thread is never preempted and the rest of Windows doesn't run meanwhile, so an input takes milliseconds instead of a scheduler quantum. A path that waits for another CPU or an interrupt times out instead. On the HEVD dispatch checkpoint (8 GiB, 2 CPUs) this runs about 100 inputs/s; without it, about 5.

Set `bugcheck_pc` to the guest VA of `KeBugCheckEx` (for example from `x nt!KeBugCheckEx` in the kernel lab). Any thread reaching it ends the input with a `BugCheck` finding whose detail holds the code, the four parameters and the caller (x0..x4, lr). The kernel hasn't started going down yet, so the VM is reset in place like after a clean input. Without it, a Windows bugcheck takes about 3 s to reset the guest, and the checkpoint is then restored from disk (about 8 s for 2.4 GB). On HEVD's stack overflow (`0x222003`, payloads past 2048 bytes) a crash now costs about 1 ms plus the usual 7 ms reset: a 30 s run did 3154 inputs (368 of them bugchecks) instead of 35. The breakpoint takes a fifth hardware slot (HVF on M3 has six); the runner refuses the harness if the backend has only four.

`stop_pc` usually sits in shared kernel code (after `IofCallDriver`). Only the testcase's thread ends an input there: the same address space, and a stack frame just above the entry frame. Other threads that pass it are stepped over and counted as `foreign_threads` in each result; breakpoints are armed on every CPU, so the thread is followed if it migrates. Exception sites are filtered the same way. `reset_us` in each result is the time the reset before that input took.

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
