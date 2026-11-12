# Fixed RCBA decode for first EFI boot

`MchPci::with_q35_lpc_rcrb` opts into one checked root-complex register block
at `FED1C000..FED20000`. Existing constructors retain their previous behavior.
The selected OVMF writes LPC configuration dword `F0=FED1C001` before enabling
PCIEXBAR and reserves this 16 KiB region as IO memory. This unit accepts that
setup through the actual shared CF8/ECAM configuration path.

RCBA resets to zero. Only aligned dword accesses at F0 are supported; accepted
values are zero, `FED1C000` (base staged, disabled), and `FED1C001` (enabled).
Other bases, reserved bits, partial or wider writes fail before state changes.
The same private LPC state controls configuration readback and MMIO routing.
Enablement follows RCBA bit zero independently of PMBASE, ACPI_EN and PCI
command bits, as in the pinned QEMU update path.

The constructor requires the exact range once in the permanent reserved
inventory and rejects overlapping reservations, GuestMemory, fixed devices,
ECAM or BAR windows. It cannot discover hidden HV mappings omitted by a caller:
the future board must bind the complete inventory to actual mappings. The
authoritative `Q35FirmwareLayout` already reserves this exact range.

Disabled accesses are unmapped with unchanged read buffers. Enabled accesses
return `LpcError::RcrbRegister` for **every register**, before any fixed or BAR
callback. Boundary/width/alignment failures never fall through to another
device. Common SMRAM/SMI poison and terminal S5 checks precede every route.
There is no backing RAM, chip-config byte array, IRQ routing, OIC, GCS or lock
readback behind this decoder. An observed required register must acquire real,
separately reviewed behavior before it can be accepted.

No RCRB MMIO transaction appears in the examined pinned platform initialization;
that source observation is not proof that later firmware or OS code never uses
the block. This slice closes a known setup failure and leaves an explicit
diagnostic for such accesses. It enables no VMM profile and proves no firmware
boot, SMI hardware, protected variables, TPM, Windows or BitLocker behavior.

Validation uses actual MchPci CF8/ECAM/MMIO operations, layout inventories,
negative mappings and existing poison/terminal effects. Tests create no VM.
The collision fixture allocates one host page; other fixtures use empty
GuestMemory and do not claim to populate the complete 512 MiB board.

```sh
cargo test --locked --offline -p vroom-devices --lib
cargo test --locked --offline -p vroom-linux-harness --test rcba_decode
```

Sources: [selected OVMF setup and reservation](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/PlatformInitLib/Platform.c),
[Q35 register constants](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Include/IndustryStandard/Q35MchIch9.h),
[QEMU RCBA map/unmap and real chip-config effects](https://github.com/qemu/qemu/blob/v11.1.2/hw/isa/lpc_ich9.c).
