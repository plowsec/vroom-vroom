# Restricted native SMM / PC flash acceptance

This example is a backend prerequisite for a future x64 firmware board. It is
**not OVMF, Windows, Secure Boot, a variable service, TPM or BitLocker acceptance**.
No normal VMM machine profile enables SMM. ARM backend sources are unchanged.

Build and unit-test without starting a guest:

```sh
cargo test --locked --offline -p vroom-hv -p vroom-vmm --lib
cargo test --locked --offline -p vroom-linux-harness --example smm_flash
cargo check --locked --offline -p vroom-hv -p vroom-vmm -p vroom-linux-harness --all-targets --target x86_64-unknown-linux-gnu
cargo check --locked --offline -p vroom-hv -p vroom-vmm -p vroom-linux-harness --all-targets --target aarch64-unknown-linux-gnu
```

Only after separate hardware-run approval, on Linux x86_64 with `/dev/kvm`:

```sh
cargo build --locked --offline --release -p vroom-linux-harness --example smm_flash
timeout --signal=TERM --kill-after=2s 20s target/release/examples/smm_flash
```

There is no software-emulation fallback. Before CPU creation, the backend requires
VM-level `KVM_CAP_X86_SMM` and numeric system-level
`KVM_CAP_MULTI_ADDRESS_SPACE >= 2`, then actually registers the AS1 slot. The pinned
KVM wrapper exposes no numeric VM-level address-space query; the reported count is
explicitly system-level. Failure exits without running a guest.

One invocation creates at most four **sequential** fresh VMs: positive SMI/RSM,
normal-mode SMRAM read denial, normal-mode SMRAM write denial, and normal-mode flash
write denial. At most one VM/vCPU is live. Each owns 64 KiB common RAM, 64 KiB
read-only reset ROM, 64 KiB SMRAM and 4 KiB emulated flash (196 KiB, excluding KVM
bookkeeping). A global 15-second process watchdog covers all cases; the external
20-second timeout is a second bound. No disk, OS, network, listeners, downloaded
code, external helper, persistence or secret data is involved.

The restricted API accepts exactly one copied 64 KiB initial image at
`0x30000..0x40000`, before **any mapping or CPU-creation attempt**. The backend
allocates its own non-shared backing and maps it only in AS1. Normal GuestMemory
has neither the GPA range nor its host backing. Common RAM and ROM are mirrored
in AS0 and AS1; overlap with SMRAM, host aliases, slot overflow, dynamic host-buffer
maps and later topology changes are rejected. All owners are retained before
ioctls; any partial registration failure poisons the topology. Each SMM vCPU
retains the owners until its fd is closed, even if the public VM handle is gone.

Successful acceptance requires the exact checked sequence, not silence or a
halt: ordinary PIO read completion, SMI injection, AS1 common-ROM read, private
SMRAM read, common-RAM read/write, byte-program of PcFlash, exact SMM MMIO and PIO
read completion, RSM, ordinary exit context, and common-RAM/flash readback. The
positive case drops the public VM and GuestMemory handles before the first RUN
to exercise backing lifetime. Wrong-width/kind/duplicate read completions fail;
RUN, SMI and boot-state changes cannot pass a pending read. Separate denial cases
never continue an unauthorized access or fabricate a read value.

`KVM_RUN_X86_SMM` is sampled from the **same successful KVM_RUN** before publishing
the I/O or its pending completion. SMM exits use the additive public
`VcpuExit::X86SmmIo(X86IoExit)` variant. Existing VMM workers reject it before device
dispatch or read completion. The harness converts only its exact expected SMM
exits to `PcFlashAccess::SystemManagement`; that caller-supplied enum alone remains
neither authorization nor isolation. Normal flash writes use `Normal` and fail.

Success is a JSON line with `test="native_smm_flash"`, `status="pass"`,
`vm_count=4` and `os_or_firmware_boot=false`, together with process exit zero.
Capability lines alone are not a pass. A timeout, unexpected exit, unsupported
capability, read mismatch or denial failure is nonzero. Initial validation of this
change is unit tests and cross-compilation only; live KVM execution is pending.

Public additions are default-unsupported `Vm::configure_x86_smm` and
`Vcpu::inject_smi`, the capability result and SMM I/O enum. Exhaustive downstream
VcpuExit matches must handle the new variant. Ordinary Linux initialization stays
repeatable; restricted SMM accepts only the existing once-only reset-vector entry.
There is no SMRAM relocation, Q35 D_OPEN/D_LCK/TSEG, guest chipset SMI controller,
SMM CPU snapshot/restore, protected firmware variables, or Windows board here.

Primary ABI sources:

- [KVM memory-slot address spaces](https://docs.kernel.org/virt/kvm/api.html#kvm-set-user-memory-region)
- [KVM_RUN flags and completion](https://docs.kernel.org/virt/kvm/api.html#the-kvm-run-structure)
- [KVM SMI injection](https://docs.kernel.org/virt/kvm/api.html#kvm-smi)
- [Linux v6.18 SMM entry and RSM](https://github.com/torvalds/linux/blob/v6.18/arch/x86/kvm/smm.c): default SMBASE entry at +0x8000 and save area at +0xfe00; initial DS base zero and 4 GiB limit enable the bounded AS1 ROM probe before DS is changed.
