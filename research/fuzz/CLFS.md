# CLFS demo

Sequence fuzzing of `clfs.sys` (Windows 11 ARM64, build 26300): each input is a program of CLFS calls run by a client in the guest, so a log, its containers, marshalling areas, records and cursors carry from call to call. Coverage: every block of `clfs.sys`; exceptions at first chance, `__except` or not.

```bash
cd /Users/vladimir/ctf/vroom-vroom
VM=/Users/vladimir/VMs/clfs-windows-arm64
F=target/release/vroom-fuzz
R="target/debug/vroom-rpc --socket $VM/lab.sock"
```

## 1. VM

Clone of S1 at its `checkpoint-ssh-ready` snapshot (agent with `guest.exec`): base disk, its frozen layer, EFI vars, TPM state and the snapshot as APFS clones (`cp -c -p`: snapshots check layer mtimes), names changed to `clfs-windows-arm64`, NAT, shares `$VM/shared`, `$VM/shared-rw`. Snapshots: `checkpoint` (the guest idle, agent up), `clfs-park` (the client parked); both in the state dir (`vroom snapshot list`), where pruning sees them.

```bash
T=/tmp/vroom-lab-build    # any target dir: not target/ while another lab runs from it
CARGO_TARGET_DIR=$T research/build.sh --release
VROOM_GUEST_TOOLS=$PWD/target/guest-tools VROOM_FUZZ=$PWD/$F \
  $T/research/release/vroom-lab $VM/clfs-windows-arm64.toml --restore checkpoint \
  --agent-socket $VM/lab.sock --daemon
$R run vm.resume '{}'
```

## 2. Client

`research/fuzz/clients/clfs`: `clfs-client run FILE` (one program, each call's result printed), `clfs-client wait` (parks for the snapshot; addresses in `C:\clfs\client.txt`).

```bash
(cd research/fuzz/clients/clfs && cargo xwin build --release --target aarch64-pc-windows-msvc)
cp research/fuzz/clients/clfs/target/aarch64-pc-windows-msvc/release/clfs-client.exe $VM/shared/
$R run guest.upload "{\"host_path\":\"$VM/shared/clfs-client.exe\",\"guest_path\":\"C:\\\\clfs-client.exe\"}"
$R run guest.exec '{"command":"rd /s /q C:\\clfs & start \"\" /b C:\\clfs-client.exe wait & ping -n 3 127.0.0.1 >nul & type C:\\clfs\\client.txt & tasklist /fi \"imagename eq clfs-client.exe\""}'
```

Ops 0–25 (34 calls with their variants), typed: [clients/clfs/calls.json](clients/clfs/calls.json), from `src/main.rs`'s payload reads. A client change that moves a payload changes it too.

Call: `u32 op | u16 length | payload`. An LSN: a slot byte, or bit 7 set and 8 bytes. Slots are taken modulo their count; missing ones are null handles.

## 3. Snapshot in the client, harness

```bash
python3 research/fuzz/tools/client_harness.py --socket $VM/lab.sock --pid PID \
  --report "$(cat client.txt line)" --target CLFS.SYS --snapshot clfs-park \
  --blocks clfs-blocks.json --output $VM/clfs-harness.json --sequence --entry LogIoDispatch@CClfsDriver
```

Breaks at `entry_pc` (`park`) in the client's process, snapshots there (`$VM/clfs-windows-arm64.state/snapshots/clfs-park`), and writes the harness: `cpu` of the stop, `input_gpa`, the client's addresses, `bugcheck_pc`, `target`, `entries` (cmplog), blocks mode, no isolation, detectors `exceptions`, `bugcheck`, `timeout`; `config`, `state_dir` and `calls_file` kept from an existing harness. Call descriptions: `cp research/fuzz/clients/clfs/calls.json $VM/clfs-calls.json`, `"calls_file": "clfs-calls.json"`; `$F show $VM/clfs-harness.json FILE` prints a program.

A rebuilt client moves its addresses: steps 2–3 again, after `vroom snapshot delete $VM/clfs-windows-arm64.toml clfs-park` (a CLI from 956acff on).

## 4. Blocks

```bash
$R run guest.download '{"guest_path":"C:\\Windows\\System32\\drivers\\clfs.sys","host_path":"'$VM'/clfs.sys"}'
$F blocks $VM/clfs.sys --base 0xfffff804000d0000 --module CLFS.SYS --symbols $VM/clfs-windows-arm64.state/symbols --output $VM/clfs-blocks.json
# 18003 blocks, 1305 functions (992 named from the PDB), ~2 s
```

The base is `kernel.modules`' (the harness tool prints it).

## 5. Fuzz

```bash
$F run $VM/clfs-harness.json $VM/seeds/append-read.bin --duration-secs 600 --no-boundary-seeds --tui --state-dir $VM/fuzz.state --output $VM/runs/a
$F triage $VM/clfs-harness.json $VM/runs/a/crashes/HASH-CLASS.bin --state-dir $VM/fuzz.state
```

- Seeds: `seeds/rich.bin` (47 calls: two containers, records, reads in each mode, tails, reservations, reopen, a multiplexed log with two streams, BLF bytes then reopen, delete), `seeds/append-read.bin`, `seeds/create.bin`.
- Cmplog (`$F cmplog $VM/clfs-harness.json $VM/seeds/rich.bin`): 91 entries into `LogIoDispatch`, 300 steps each, ~2.1k comparisons, ~1.3k candidates (128 run per log), 1.6 s.
- The whole guest runs (CLFS does disk I/O); each reset restores RAM, disks and devices.
- `VROOM_FUZZ_TRACE=1`: the client's per-call results at `stop_pc` (op in the top byte, Win32 error below).

10 min from `rich.bin`, M3 Max, another lab VM alongside (blocks of 18003):

| | programs | blocks |
|---|---|---|
| before: op 14 always error 87, no cmplog for clients | 15.0k (25/s) | 4643 |
| op 14 fixed, `--no-cmplog` | 19.6k (33/s) | 4748 |
| op 14 fixed, cmplog (130 logs, ~1.6 s each) | 20.6k (34/s) | 5738 |

Cmplog's extra blocks: the management IOCTLs (`ClfsMgmtDispatchIo`, managed-log policies, container growth), archival and restart requests. Earlier: `append-read.bin` 36k (60/s), 4529. No findings.

First night (3 workers, ~8 h, resumed sessions; seeds also `rich2.bin`: ops 15-25, `rich3.bin`: an overlapped log): 6763 blocks (~1000 more are the kernel-mode marshalling API, unreachable from user mode), 40-60 programs/s. One finding: `CClfsManagedLogClientUser::QueryRegistration` reads `0x28` (a NULL client) on management IOCTLs `0x80077018` then `0x8007b020` on a fresh log, swallowed by CLFS's own `__except`, caught at first chance; 98-byte reproducer in `runs/night/triage/`.

Typed mutation (`calls_file`), 30 min, 2 workers, `--no-cmplog`, a fresh output seeded with the night's 593 inputs (6797 blocks); harness with and without `calls_file`, then later builds. Calls ok and errors: the workers' `VROOM_FUZZ_TRACE` `stop_pc` lines past each one's first 300 (the corpus), errors of all calls:

| | execs | blocks at 5 / 10 / 20 / 30 min | new vs the night | corpus | calls ok (ops 0–25) | error 87 / 6 |
|---|---|---|---|---|---|---|
| bytes (no descriptions) | 43.2k | 6795 / 6796 / 6802 / 6803 | 6 | +6 | 52.0% | 21.3% / 9.8% |
| typed (846399c) | 41.7k | 6819 / 6838 / 6858 / 6861 | 68 | +31 | 53.5% | 20.0% / 8.2% |
| + runtime results: a failed call fills no slot | 48.3k | 6810 / 6831 / 6851 / 6868 | 75 | +36 | 52.7% | 20.9% / 8.3% |
| + repair 50%, retarget only | 50.2k | 6819 / 6828 / 6840 / 6854 | 62 | +23 | 54.2% | 20.1% / 8.1% |
| + repair 50%, one producer inserted | 54.7k | 6832 / 6833 / 6848 / 6859 | 66 | +28 | 53.0% | 20.3% / 8.2% |
| + repair 90%, ≤4 producers (committed) | 50.2k | 6806 / 6810 / 6833 / 6862 | 70 | +30 | 52.9% | 19.2% / 8.2% |

Typed's new blocks: `AdjustReservation`, `ReserveAdjustment`, `ReadNotification`, `ReadMgmtNotificationDone`, `ClfsMgmtUserModeRegister`, `GetIoStatistics`.

Runtime results change neither calls ok nor coverage within 30 min (typed variants: 6854–6868, the noise between runs). The corpus programs' 87s (their `stop_pc` lines matched to inputs by op sequence, 80k calls): 89% are calls on an area that is dead (its `CreateLogMarshallingArea` failed) or never made, 64% with no area live at all (retargeting alone can't fix those: repair inserts producers). Offline, on the same programs: handle uses naming no live slot are 31% of parents', 24.5% of children's (one producer), 19% (≤4). Live, 87s only go 20.0% → 19.2%. Calls ok falls during every run (~55% at 10 min, ~53% at 30).
