# Direct x64 Linux reference board

This is a narrow, cold-boot-only VMM integration. Local tests and cross-compilation
are separate from running a real Linux guest; this patch has no native runtime
acceptance yet. The earlier `vroom-linux-harness` runtime exercised its own
controller, not this `Vmm::start` integration.

The accepted profile is x86_64 KVM, one vCPU, 512 MiB RAM starting at zero,
COM1 byte-wide PIO at 0x3f8 with IRQ4, and KVM's in-kernel PIC/PIT/APIC. Input is
a relocatable 64-bit bzImage (at most 128 MiB) plus a nonempty initramfs (at most
8 MiB). Both must be regular files, not final-component symlinks. The validated
bytes are owned before any VMM state, disk layer, viewer, device or helper is
created. The kernel command line must be exactly:

```
console=ttyS0,115200 earlycon=uart8250,io,0x3f8,115200n8 rdinit=/init panic=-1 nokaslr acpi=off pci=off noapic
```

The config requires `cpus=1`, `memory_mib=512`, `balloon=false`, both clipboard
directions disabled, default headless display/identity/transport settings, stdio
serial, and a Linux OS designation (or the direct-kernel Linux default). Disks,
networks, shares, USB, TPM, nested virtualization, audio, camera, a viewer and a
host clipboard provider are rejected. Firmware and restore starts are rejected.
The default VMM feature set is required. Unsupported configurations and malformed
or oversized images fail before VMM state/device/helper side effects.

The ARM path keeps its default resettable lifecycle. X64 selects `ColdBootOnly`:
no CPU, interrupt-controller or device snapshot is taken. Reset, snapshot,
suspend and restore fail explicitly. Guest-agent exec/file/update calls and
control-socket requests fail with the unavailable reason. Host serial input is
unavailable: there is no receive worker, periodic receive timeout service or
receive wakeup. No guest-agent transport, guest OS poweroff, ACPI, UEFI, Secure
Boot, TPM, BitLocker, Windows, VBS or snapshot support is established here.

Serial writes and IRQ callbacks are synchronous. Their first failure is latched;
PIO dispatch checks it before input completion and the worker checks it before
another attempted run. The worker parks with a crash event after such a failure.
Unmapped PC MMIO fails rather than synthesizing a device response.

## Control and acceptance

`Vmm::vcpu_run_calls()` counts attempts by real workers to call the backend's
`Vcpu::run`, saturating at `u64::MAX`. It does not count instructions and a
positive count need not mean any guest instruction executed. Zero together with
the acknowledged BootPaused state proves no worker attempted guest entry. A
counter read after `pause()` returns is stable until resume.

`Vmm::shutdown_checked()` stops and joins every worker, performs the remaining
teardown, and then reports the first join panic if any. It does not return early
and strand later workers. Existing `shutdown()` retains its unit-returning
compatibility behavior. A successful host stop is distinct from guest OS poweroff.
With `panic=abort` builds, a worker panic terminates the process instead of
returning a join error; the supervisor must reject that nonzero exit. These
methods rely on an independent process timeout for a stuck backend ioctl.

Build the new example with existing pinned dependencies (no Cargo additions):

```
cargo build --locked --offline -p vroom-vmm --example linux_x86_64
```

Run only on a reviewed Linux x64 KVM host with a separately verified packaged
kernel and the existing `make-linux-heartbeat.py` x64 initramfs. The supervising
process supplies a fresh lowercase 32-hex token and a new absolute state path in
its own temporary workspace:

```
timeout --signal=TERM --kill-after=5s 72s target/debug/examples/linux_x86_64 KERNEL INITRAMFS TOKEN_HEX32 NEW_STATE_DIRECTORY
```

The example uses public VMM APIs: BootPaused and zero run calls, 350 ms with no
run attempts, then sandbox application before first resume, five exact sequential
PID1 heartbeats, acknowledged pause, 350 ms of unchanged serial bytes and run
calls, resume with five more heartbeats, and checked host stop/join. It also checks
for late crash events after joining. Output is one bounded JSON report; only
`status=pass` plus process exit zero is acceptance. A 65-second internal watchdog
exits unsuccessfully if setup, execution or teardown stalls. Unsupported host
platforms return exit 2 with no guest execution.

Sandbox flags retain their existing meaning and defaults. The example requests
the sandbox and calls the same `vroom_sandbox::apply` API as the CLI; it fails on
an apply error. `sandbox_apply_succeeded` records only a successful API return.
The existing API treats unavailable Landlock as a warning, so this is not proof
of Landlock enforcement or complete process isolation. This patch changes no
sandbox implementation or policy defaults. The supervising process must remove
its own temporary state after child termination: the existing sandbox syscall
allowlist does not permit the child to unlink/rmdir it. No guest disks or network
interfaces are created.
