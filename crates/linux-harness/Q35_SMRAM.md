# Fixed Q35 SMRAM prerequisite

This is a native backend acceptance harness, not a Q35 machine or firmware
profile. **Runtime hardware proof is pending.** Unit tests and compile checks
do not establish SMI delivery or OVMF/Windows support. The existing `smm_flash`
example and its separately frozen artifact remain independent.

The fixed layout has nominal low RAM ending at 512 MiB, an 8 MiB TSEG at
`0x1f800000..0x20000000`, and a 128 KiB default-SMBASE reservation at
`0x30000..0x50000`. Both use owned anonymous backing absent from device-facing
`GuestMemory`, even during temporary normal-CPU visibility. AS1 stays mapped;
only AS0 aliases are added/removed. A failed mapping transaction poisons further
execution and completion while retaining all backing owners.

`Vm::configure_x86_q35_smram` is fresh-only. `Vcpu::apply_x86_smram` consumes
the current stopped write-exit grant before checking or applying a request.
TSEG can close/reopen until locked; default-SMBASE has a separate irreversible
lock. Neither can be unlocked by resetting a vCPU, restoring state or creating
another adapter. The complete VM must be recreated. The existing static 64 KiB
AS1-only profile is unchanged. This does not emulate legacy low/high SMRAM,
SMBASE relocation, or MCH register writes.

Future MCH integration must prepare a complete candidate, call the backend
synchronously on the sole exiting vCPU, then publish register readbacks only
after success. Any apply error must stop the router/worker, including future
readbacks. OVMF opens TSEG through `ESMRAMC.T_EN`, not `D_OPEN`; effective CPU
closure also requires `G_SMRAME`. Its `D_LCK` sequence follows close, and the
default-SMBASE lock is separate. The fixed 8 MiB choice follows the selected
OVMF fallback; it is not QEMU's generic extended-TSEG default.

After separate runtime review/authorization, the bounded command is:

```sh
timeout --signal=TERM --kill-after=2s 20s ./q35_smram
```

It accepts no arguments and has a 15-second internal watchdog. One invocation
creates three fresh sequential VMs, at most one VM/vCPU alive, each with less
than 9 MiB backing. There is no disk, network, OS, external firmware or helper.
Unsupported KVM SMM/address-space capabilities fail before guest execution.

The positive guest enters ordinary flat 32-bit mode and initializes both
private regions through their CPU-only AS0 mappings. It checks bytes across a
TSEG close/reopen, locks both ranges, then a real SMI handler reads both in AS1,
writes TSEG/common memory and returns through RSM. Two separate negative guests
must receive the backend's private-access error for normal reads of locked
TSEG/default-SMBASE. The public VM handle is dropped before all guest execution
to exercise CPU-retained descriptor/backing lifetime. Every marker and its
same-RUN normal/SMM context is checked; a halt or timeout is never success.
The final JSON `test=native_q35_smram,status=pass` is emitted only after all
three cases complete. Host teardown is distinct from guest OS poweroff.

Pure tests cover slot geometry, aliases/DMA exclusion, locks, poisoned partial
failures, owner retention and one-shot grants. The example's tests inspect its
instruction/layout bytes only; they never call the hypervisor.

Primary contracts: [pinned QEMU MCH mappings](https://github.com/qemu/qemu/blob/v11.1.2/hw/pci-host/q35.c),
[pinned OVMF SMRAM protocol](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/SmmAccess/SmramInternal.c),
[KVM memory-slot API](https://docs.kernel.org/virt/kvm/api.html#kvm-set-user-memory-region).
