# Lifecycle policies

`Vmm::start` keeps the existing `Resettable` policy on ARM. The strict x64
reference board selects `ColdBootOnly` and validates its configuration and
owned image bytes before state/device/helper creation. See `X64_LINUX.md` for
that deliberately narrow profile and its separate native acceptance example.
The experimental full PC/Windows board uses `Resettable`, including its
existing x64 CPU/irqchip snapshots and research debugger. See
[docs/X86_64.md](../../docs/X86_64.md) for profile selection and limits.

Cold-boot workers do not capture or restore pristine CPU state. The VMM also
skips pristine device and interrupt-controller snapshots. Initial register
setup is a once-only, acknowledged command, separate from ARM's runtime
PSCI `PowerOn`. All workers are parked, and every initialization reply is
checked before any resume. A failed initialization is returned to the
caller; the partially initialized VMM is shut down. `BootPaused` returns
without entering a guest run loop.

Cold-boot reset, snapshot, suspend, and restore requests are rejected before
pausing CPUs/devices, touching RAM, opening snapshot paths, or changing disk
layers. Worker state commands have matching guards. Existing resettable
ARM behavior and snapshot formats are unchanged.

## Intentional API adjustment

`Vmm::suspend` now takes `&mut self` instead of consuming `self`. This keeps
a running VM available when suspend is unsupported. A successful suspend
still shuts it down. `suspend_in_place` retains its signature: an unsupported
request leaves the original run state intact; an ordinary save failure can
leave the VM paused. No repository caller used the consuming method when
this change was prepared.

The fake-hypervisor tests exercise initialization errors, duplicate starts,
paused boot, default reset and runtime ARM power-on, and rejection without
CPU, device, memory, or snapshot-file side effects. These tests do not claim
a new guest OS boot or hardware validation. Native acceptance is reported
separately from these fake-backend tests.

`shutdown_checked()` adds panic reporting to host stop: every worker is stopped
and joined and display teardown is completed before a join error is returned.
`shutdown()` retains its unit-returning compatibility behavior. Neither method
claims a guest OS poweroff, and a supervising timeout must bound a stuck backend.
`vcpu_run_calls()` exposes attempted backend run calls (not instructions); zero
combined with acknowledged BootPaused state proves no attempted entry.
