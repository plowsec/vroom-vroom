# Native ICH9 PM timer prerequisite

`examples/ich9_pm.rs` exercises the checked LPC configuration and PM timer
through native x64 KVM exits. It is a small machine-code harness, not OVMF,
Windows or a complete Q35 board. Only pure tests and target compilation have
been performed; the native harness has no runtime pass yet.

`MchPci::with_q35_lpc` composes the existing fixed-Q35 SMRAM model with a private
LPC at PCI `0:31.0`. The caller must first configure that actual HV backend and
provide the same complete RAM/ROM/private/MMIO inventory required by
`with_q35_smram`. Slot 31 cannot also hold a generic endpoint. The LPC state is
shared by CF8, ECAM and PM port access, with no writable root or endpoint handle
escaping the exclusive router. Every new route checks existing SMRAM poison.
Legacy constructors, BAR validation and ARM snapshot bytes remain unchanged.
The new composition is cold-only and rejects snapshot/reset paths.

Supported configuration is deliberately narrow:

| Register | Behavior |
| --- | --- |
| Standard header | Intel `8086:2918`, revision 2, ISA bridge class, no BARs |
| `PMBASE`, `0x40..0x43` | Address bits 15:7; bit 0 reads one; reset value 1 |
| `ACPI_CNTL`, `0x44` | Bit 7 enables PM decode; only SCI selector 0 is supported |
| `PCI_COMMAND.IOSE` | Writable, but does not gate the PM timer |

The early OVMF timer constructor sets PMBASE and ACPI_EN before PlatformInit
sets IOSE. Requiring IOSE would stall that firmware. ACPI_EN here means port
decode only: this change does not implement SCI, ACPI mode switching or SMI.
Other chipset banks, including RCBA and GEN_PMCON_1, return explicit errors.

`LpcPioLayout` accepts one or two aligned 128-byte PM windows and at most 64
excluded port intervals. It checks complete ranges against each other and
unconditionally reserves common PIC/PIT/keyboard/CMOS/A20/APM/COM/fw_cfg/CF8
ports. The caller must include **all additional actual board ports**, since
the device layer cannot discover omissions. A valid disabled PMBASE can be
staged; enabling a nonzero base outside the allowed windows fails before
readback or decoding changes. Base zero never decodes. Disabling or rebasing
removes the old route immediately. LPC reads returning `false` are unmapped
and leave their buffer unchanged; the board decides how absent ports complete.

The read-only PM timer is at `PMBASE + 8`. Byte, word and dword accesses must
fit entirely inside its four bytes. A single sampled 24-bit value supplies
each read, with the upper eight bits zero. The calculation is
`floor(elapsed_ns * 3579545 / 1000000000) mod 2^24`, using a wide intermediate.
Repeated reads at one timestamp return the same value. The caller supplies
monotonic nanoseconds since one cold epoch; backwards observations fail.
The current KVM `Vm::clock()` returns zero and is not used. Disable/rebase and
ignored writes to the read-only timer do not reset it. Unmapped or invalid
accesses do not alter the accepted clock. Pause, migration and restored clocks
require a later common board-clock implementation and are not claimed here.

The compiled harness uses one `Instant` epoch and real elapsed time. Its guest
programs MCH ECAM through CF8, accesses LPC through both transports, verifies
timer operation with IOSE still zero, disables the decoder, then enables it at
a second reserved base. It checks old ports are absent, observes continued
timer progress and verifies writes cannot replace the timer value. Every read
is completed through HV and echoed by the guest before acceptance. There is
one VM and one live vCPU, less than 9 MiB of guest backing (including the fixed
private Q35 ranges), and no disks, network, firmware media or worker devices.
The final marker causes a controller stop with no further `RUN`; it is not an
S5 shutdown. An internal watchdog covers the whole 15-second invocation.
A future separately authorized invocation must also be bounded externally:

```sh
timeout --signal=TERM --kill-after=2s 20s ./ich9_pm
```

Success requires exit zero and the final `native_ich9_pm` JSON record with all
timer/decode/read-echo flags true. Unsupported targets exit 2 without a guest.
SMI/APM, GEN_PMCON lock, PM1/SCI/S5, RTC, RCBA/RCRB and matching ACPI tables
remain separate work. The selected OVMF programs RCBA, so this unit alone does
not make the firmware bootable. No native Windows, protected variables, TPM,
BitLocker, SMP, power management or production VMM profile is enabled.

The register/decode contract follows pinned
[QEMU ICH9 LPC](https://github.com/qemu/qemu/blob/v11.1.2/hw/isa/lpc_ich9.c)
and its [ACPI timer](https://github.com/qemu/qemu/blob/v11.1.2/hw/acpi/core.c).
The early initialization and 24-bit timer assumptions come from EDK2
[BaseAcpiTimerLib](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/AcpiTimerLib/BaseAcpiTimerLib.c)
and [AcpiTimerLib](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/AcpiTimerLib/AcpiTimerLib.c).
