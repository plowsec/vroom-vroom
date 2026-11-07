# the paused Ex callback demo

`research/demo.sh CONFIG SNAPSHOT`: restores a snapshot stopped on the entry of a registered `Ex` process-notify callback; shows the process being created, its creator, the stack. `experiments/demo-s1.sh`: same, S1 lab paths filled in.

| angr_tui | |
|---|---|
| F11 | callback event (the arguments) |
| `:pane vroom-callbacks` | registered callbacks; `>` on the one whose `function` **equals** `PC` |
| F10 | stack |

No `>` = no demo: the event pane decodes whatever the registers hold. The snapshot must be made on purpose, below.

## what you need

- Windows ARM64 guest with a driver that registers a process callback (any EDR; S1 lab: `SentinelMonitor.sys`)
- guest tools, for `guest.exec`
- nothing else: no KDNET, no guest driver, no boot flags

## the lab VM

- Snapshot is 1:1 with its config: own config, own state dir, disposable overlay.
- Never point it at a VM the app may also run: one VM runs once.
- Clone without copying the disk. A VM with a snapshot already has its base pinned read-only: branching off it is safe.

```sh
VM="$HOME/Library/Application Support/vroom/VMs/S1-Windows-11-64-bit-Arm"
LAB=~/dev/vroom-s1-lab
mkdir -p "$LAB/s1-clean.state"

# APFS clone: no blocks copied, separate inode: the app's snapshot of the base
# stays restorable (it records size and mtime)
cp -c "$VM/S1-Windows-11-64-bit-Arm.qcow2" "$LAB/s1-base.qcow2"
qemu-img create -q -f qcow2 -b s1-base.qcow2 -F qcow2 "$LAB/s1-overlay.qcow2"

# EFI vars: boots Windows, not the UEFI shell. TPM: no BitLocker recovery prompt.
cp "$VM/S1-Windows-11-64-bit-Arm.state/efivars.fd" "$LAB/s1-clean.state/"
cp -R "$VM/S1-Windows-11-64-bit-Arm.state/tpm" "$LAB/s1-clean.state/"
chmod -R u+w "$LAB/s1-clean.state"
```

- Clone must sit next to the overlay: `resolve_backing` (`crates/block/src/qcow2.rs`) accepts a backing file only in the overlay's dir or under the disk's configured dir, after canonicalizing. A symlink into the app's VM folder is refused like an absolute path.
- `s1-clean.toml`: the VM's config, disk swapped, own state dir. Keep `[identity]`: an EDR can key its licence to the serial.
- Snapshot writes all guest RAM: `memory_mib = 4096` is plenty (4 GiB per snapshot instead of 8).

## the snapshot

Headless, over the agent socket. Relative socket path (104-byte limit, easy to hit in a temp dir).

Neither binary is on `PATH`. `research/build.sh` signs `vroom-lab` into `target/research/<profile>/`; `vroom-rpc` needs no entitlement, stays where cargo put it.

```sh
V=~/dev/vroom-vroom
VLAB=$V/target/research/debug/vroom-lab
RPC=$V/target/debug/vroom-rpc

cd ~/dev/vroom-s1-lab
$VLAB s1-clean.toml --agent-socket lab.sock --daemon
$RPC --socket lab.sock run vm.resume          # cold boot; lab and restores start paused
# wait for Windows, then:
$RPC --socket lab.sock run vm.pause
$RPC --socket lab.sock run kernel.info        # answers = kernel up (needs paused VM)
$RPC --socket lab.sock run kernel.symbols     # every new lab session
$RPC --socket lab.sock run kernel.callbacks
```

- Symbols are per session: PDB cached in `STATE/symbols` after the first time, but `kernel.callbacks`, `kernel.stack`, `kernel.functions`, `kernel.resolve` fail with `load kernel symbols first` until `kernel.symbols` runs in this session.
- Pick the callback whose `module` is your driver; break on its `function`. HW execution breakpoints: 4, aligned.

```sh
$RPC --socket lab.sock run debug.breakpoints '{"addresses":["0xfffff8003c0e50f0"]}'
$RPC --socket lab.sock run vm.resume
$RPC --socket lab.sock events.poll '{"wait_ms":120000}'
$RPC --socket lab.sock run kernel.process_event '{"cpu":0}'   # confirm the decode: this is the demo
$RPC --socket lab.sock run vm.snapshot '{"path":"/Users/you/dev/vroom-s1-lab/s1-callback-snapshot"}'
kill PID                                                       # the pid it printed; before any restore
```

- No trigger needed: process callbacks fire on every create/exit; idle Windows makes processes constantly.
- Stop event: `cpu`, `pc` = the breakpoint.
- Snapshot: 4 GiB at `memory_mib = 4096`; target dir must not exist.
- The saved hit may not be the one you inspected (whichever hit the guest was on at save). Demo works either way; the pid may differ after restore.

## running it

```sh
research/demo.sh ~/dev/vroom-s1-lab/s1-clean.toml \
  ~/dev/vroom-s1-lab/s1-callback-snapshot
```

Restore clears breakpoints. angr_tui opens on the module holding `PC`. F11 event, `:pane vroom-callbacks` the `>` row, F10 stack.

Headless check, same sequence as the panes:

```sh
$VLAB s1-clean.toml --restore s1-callback-snapshot --agent-socket lab.sock --daemon
for c in kernel.info kernel.symbols kernel.modules kernel.callbacks; do
  $RPC --socket lab.sock run $c
done
$RPC --socket lab.sock run cpu.registers '{"cpu":0}'      # pc
$RPC --socket lab.sock run kernel.process_event '{"cpu":0}'
```

Only precondition: `PC` appears in `kernel.callbacks` as some callback's `function`.

## when it refuses

| | |
|---|---|
| no `>` in the callbacks pane | `PC` is not a callback entry: snapshot saved elsewhere. Redo it. |
| `load kernel symbols first` | `kernel.symbols` in this lab session |
| `the VM is running: vm.pause before ...` | `vm.step`/`vm.resume` return at once, VM running: `events.poll` for the stop first. `vm.resume` when done, or the paused guest drops your probes. |
| `backing file ... is outside the allowed directories` | overlay's backing file not beside it: clone the base into the lab dir |
| restore refuses a changed disk | something wrote a layer the snapshot recorded (size, mtime). The lab's layers are its own; the app writing the source VM breaks it. |
