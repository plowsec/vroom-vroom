# Direct x64 Linux reference board

This document describes only the direct-kernel reference profile, selected
by automatic transport (and requiring the exact command line below).
Explicit PCI transport selects the separate experimental full PC/Windows board,
documented in [docs/X86_64.md](../../docs/X86_64.md).

This is a narrow, cold-boot-only VMM integration. Its native `Vmm::start` Linux
acceptance passed on an AWS x64 nested-KVM host on 2026-11-07. This is separate
from the earlier `vroom-linux-harness` controller test. The supported profile is
restricted as described below; general CLI/RPC and firmware/security support
are not established.

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

The ARM and full PC paths keep their resettable lifecycle. This reference profile selects `ColdBootOnly`:
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


## Observed native validation

The seventh AWS pilot ran the reviewed static x64 musl example on one
`m7i-flex.large`. All 175 native tests (including 66 VMM and three example
tests) and 17 helper tests passed. The canonical Amazon Linux 6.18.56 kernel
matched its recorded RPM SHA256 before execution. Actual acceptance verified
BootPaused with zero worker run attempts, sandbox application before resume,
five sequential PID1 heartbeats, acknowledged pause with unchanged serial/run
counts, resume to ten heartbeats, and checked stop/join. Exactly one guest ran;
the external supervisor removed its owned state directory afterward.

The 23.481-second wrapper exited zero with complete captured output and an
independent result audit. Landlock enforcement, guest poweroff, snapshots,
serial RX/IRQ delivery, guest RPC and security features remained explicitly
false. The disposable AWS instance, root volume and temporary controls were
removed and independently audited absent. Private evidence is
`.aws-pilot/vroom-pilot-282f5876653f.report.json`; this successful run does not
relax any profile restriction above.
