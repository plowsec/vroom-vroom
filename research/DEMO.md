# the paused Ex callback demo

`research/demo.sh CONFIG SNAPSHOT` restores a snapshot whose CPU is
stopped on the entry of a registered `Ex` process-notify callback, then
shows what the callback was called with: the process being created, who
created it, and the stack that reached it. `experiments/demo-s1.sh` is
that, with the S1 lab's paths filled in.

The TUI's demo refuses anything else:

```text
Demo needs a snapshot stopped at a registered Ex callback entry
```

It means what it says. `start_demo` (`research/tui/vroom_tui/app.py`)
discovers the kernel, lists the callbacks, reads `PC`, and looks for a
callback whose `function` **equals** `PC`. No match, no demo. So the
snapshot has to be made on purpose; this is how.

## what you need

A Windows ARM64 guest with a driver that registers a process callback
(any EDR does; the S1 lab has `SentinelMonitor.sys`), and the guest
tools if you want `guest.exec`. Nothing else: no KDNET, no driver in
the guest, no boot flags.

## the lab VM

The snapshot is 1:1 with the config that made it, so give the lab its
own config, its own state directory, and a disposable overlay. Never
point it at a VM the app may also run: one VM runs once.

Clone the app's VM without copying its disk. The base of a VM that has
a snapshot is already pinned read-only, so branching off it is safe:

```sh
VM="$HOME/Library/Application Support/vroom/VMs/S1-Windows-11-64-bit-Arm"
LAB=~/dev/vroom-s1-lab
mkdir -p "$LAB/s1-clean.state"

# APFS clone: no blocks copied, and a separate inode, so the app's own
# snapshot of the base stays restorable (it records size and mtime).
cp -c "$VM/S1-Windows-11-64-bit-Arm.qcow2" "$LAB/s1-base.qcow2"
qemu-img create -q -f qcow2 -b s1-base.qcow2 -F qcow2 "$LAB/s1-overlay.qcow2"

# Boot variables, so it boots the installed Windows and not the UEFI
# shell; TPM state, so BitLocker does not ask for a recovery key.
cp "$VM/S1-Windows-11-64-bit-Arm.state/efivars.fd" "$LAB/s1-clean.state/"
cp -R "$VM/S1-Windows-11-64-bit-Arm.state/tpm" "$LAB/s1-clean.state/"
chmod -R u+w "$LAB/s1-clean.state"
```

The clone has to sit next to the overlay: `resolve_backing`
(`crates/block/src/qcow2.rs`) only accepts a backing file in the
overlay's own directory or under the disk's configured directory, and it
canonicalizes first, so a symlink into the app's VM folder is refused
the same way an absolute path to it is.

`s1-clean.toml` is the VM's config with the disk swapped and its own
state directory. Keep `[identity]`: an EDR can key its licence to the
serial. A snapshot writes the guest's RAM whole, so size `memory_mib`
for the disk you have, not for the guest's comfort -- 4096 is plenty,
and it is 4 GiB per snapshot instead of 8.

## the snapshot

Headless, over the agent socket. Relative socket path: the 104-byte
limit is easy to hit in a temp directory.

Neither binary is installed on `PATH`. `research/build.sh` signs
`vroom-lab` into `target/research/<profile>/`; `vroom-rpc` needs no
entitlement and stays where cargo put it:

```sh
V=~/dev/vroom-vroom
VLAB=$V/target/research/debug/vroom-lab
RPC=$V/target/debug/vroom-rpc

cd ~/dev/vroom-s1-lab
$VLAB s1-clean.toml --agent-socket lab.sock --daemon
```

Cold boot it. The lab starts paused, and a restore does too.

```sh
$RPC --socket lab.sock run vm.resume
```

Give Windows its minutes, then pause and check the kernel is up.
`kernel.info` answering is the signal; it needs a paused VM.

```sh
$RPC --socket lab.sock run vm.pause
$RPC --socket lab.sock run kernel.info
```

Symbols are per session, not per machine: the PDB is cached under
`STATE/symbols` after the first time, but every new lab has to load it
again, and `kernel.callbacks`, `kernel.stack`, `kernel.functions` and
`kernel.resolve` all fail with `load kernel symbols first` until it is
loaded.

```sh
$RPC --socket lab.sock run kernel.symbols
$RPC --socket lab.sock run kernel.callbacks
```

Pick the callback to stop on -- the one whose `module` is the driver you
came for -- and break on its `function`. These are hardware execution
breakpoints: four, aligned.

```sh
$RPC --socket lab.sock run debug.breakpoints '{"addresses":["0xfffff8003c0e50f0"]}'
$RPC --socket lab.sock run vm.resume
$RPC --socket lab.sock events.poll '{"wait_ms":120000}'
```

Nothing needs triggering: a process callback fires on every create and
exit, and an idle Windows desktop makes processes constantly. The stop
event carries the `cpu` and a `pc` equal to the breakpoint.

Confirm the decode before saving, because this is the whole demo:

```sh
$RPC --socket lab.sock run kernel.process_event '{"cpu":0}'
```

Then save it. 4 GiB at `memory_mib = 4096`, and the directory must not
exist.

```sh
$RPC --socket lab.sock run vm.snapshot '{"path":"/Users/you/dev/vroom-s1-lab/s1-callback-snapshot"}'
```

Stop the lab -- `kill` the pid it printed -- before anything restores
the snapshot. One VM runs once.

A snapshot saved at a breakpoint does not necessarily hold the hit you
just inspected; it holds whichever hit the guest was on when the save
ran. Both are a process callback with decodable arguments, so the demo
works either way, but the pid you read before saving may not be the pid
you see after restoring.

## running it

```sh
research/demo.sh ~/dev/vroom-s1-lab/s1-clean.toml \
  ~/dev/vroom-s1-lab/s1-callback-snapshot \
  --atui /path/to/run-atui-macos.sh
```

The restore clears breakpoints, so the demo opens on a clean list.

Headless, the same sequence the TUI runs, to check a snapshot without
driving a terminal UI:

```sh
$VLAB s1-clean.toml --restore s1-callback-snapshot --agent-socket lab.sock --daemon
for c in kernel.info kernel.symbols kernel.modules kernel.callbacks; do
  $RPC --socket lab.sock run $c
done
$RPC --socket lab.sock run cpu.registers '{"cpu":0}'      # pc
$RPC --socket lab.sock run kernel.process_event '{"cpu":0}'
```

`PC` has to appear in `kernel.callbacks` as some callback's `function`.
That is the precondition, and it is the only one.

## when it refuses

| | |
|---|---|
| `Demo needs a snapshot stopped at a registered Ex callback entry` | `PC` is not a callback entry: the snapshot was saved somewhere else. Make it again. |
| `load kernel symbols first` | `kernel.symbols`, in this lab session. |
| `the VM is running: vm.pause before ...` | `vm.step` and `vm.resume` return at once and leave the VM running: `events.poll` for the stop before reading anything. `vm.resume` again when done, or the paused guest drops your probes. |
| `backing file ... is outside the allowed directories` | The overlay's backing file is not beside it. Clone the base into the lab directory. |
| a restore that refuses a changed disk | Something wrote a layer the snapshot recorded by size and mtime. The lab's layers are its own; the app writing the VM those layers came from is what breaks it. |
