# Native ICH9 APM SMI prerequisite

`examples/ich9_smi.rs` binds an APM command to a real SMI request on the
stopped native x64 KVM CPU, then checks private-handler execution and RSM.
Only pure tests and target compilation have been performed. There is no
native runtime pass, OVMF boot, Windows support or enabled VMM profile here.

The cold-only `MchPci::with_q35_lpc` composition retains one private LPC at
PCI `0:31.0`. CF8, ECAM and port access share its state. Existing constructors,
BAR validation, SMRAM application and legacy ARM snapshots are preserved.
The owner must bind the existing complete RAM/private/MMIO and PIO inventories
to the actual board; no raw LPC configuration or snapshot handle escapes.

| Register | Supported behavior |
| --- | --- |
| PCI `GEN_PMCON_1`, `0xA0`, word | Only `SMI_LOCK`, bit 4; reset zero, one-way until a new cold VM |
| Enabled `PMBASE + 0x30`, dword | `SMI_EN`: only `GBL_SMI_EN` bit 0 and `APMC_EN` bit 5, reset zero |
| Fixed port `0xB3`, byte | APM data scratchpad, reset zero; readable/writable in normal and SMM context |
| Fixed port `0xB2`, byte | Last accepted APM command, reset zero; reads permitted in both contexts |

SMI_LOCK freezes GBL_SMI_EN at its current value, even zero if locked too
early; APMC_EN remains writable. The selected OVMF starts with APMC clear,
sets both enable bits, sets SMI_LOCK, attempts to clear GBL, and demands the
unchanged enabled readback. That sequence is tested through CF8, ECAM and
actual PM port decoding. Unknown register bits and unsupported widths fail
before changing state. Disabling or relocating PM decode does not move the
fixed APM ports or erase SMI state. Unit A's timer epoch is unchanged.

A B2 write must use `lpc_pio_write_with_smi` and the context derived from that
**exact** `VcpuExit`: normal `PioOut` or `X86SmmIo(PioOut)`. `LpcIoContext` is
descriptive, not an authorization token. The actual backend must independently
check its CPU. The older noncontextual write API rejects B2 writes, including
disabled commands; it cannot silently invent normal mode.

The initial virtual-platform subset is explicit:

- Commands 2 and 3 always fail until real SCI mode effects exist.
- Other commands from an already-SMM exit fail; nested/queued SMI is unsupported.
- In normal context, APMC clear stores the command without an injection request.
- APMC set with GBL clear is an unsupported trigger state, not an emulated
  physical-chipset outcome.
- With both enable bits set, exactly one synchronous backend callback runs.

The nonstored callback receives `LpcSmiRequest { command, data }`. Its owner
must hold the actual stopped `&mut Vcpu`, call `inject_smi()` once and return
that result. It must not RUN, retry, complete another read, or reenter a router
or endpoint lock. The HV checks configured isolated SMM, CPU initialization,
absence of a pending read and the actual last-RUN SMM context. A failed KVM
injection already poisons that backend.

The LPC latches poison before the callback. Only success commits the command
and clears that latch, before any subsequent RUN. Error or unwinding panic
leaves the old command uncommitted and poisons **all** checked router routes,
including CF8 selection/data, ECAM, BAR/fixed MMIO and LPC access. No external
injection rollback is claimed. Stop that VM; never retry an ambiguous effect.
A validation refusal before a callback does not itself imply an ambiguous
backend state. Callback success means a request was accepted, not delivered.

The harness uses at most two sequential fresh VMs, one live vCPU and less than
9 MiB of backing at a time: 64 KiB common RAM, 64 KiB reset ROM, 128 KiB private
default-SMBASE and 8 MiB private TSEG. It has no arguments, media, OS, firmware,
disks, network or device workers. The CPU copies its handler into actual open
SMRAM, then closes/locks both private ranges through the existing checked
MCH-to-vCPU callback. Private backing remains absent from ordinary DMA memory.

The positive VM checks disabled commands and the OVMF enable/lock sequence,
including writable APMC after locking. Two normal APM commands each request
one real `KVM_SMI`. The private handler reads B2/B3, echoes their exact completed
values through SMM-tagged exits, increments a common mailbox and executes RSM.
An ordinary guest exit must then echo each incremented value. A second fresh
VM enters the handler through one valid APM request, attempts another B2 write
while actually in SMM and must receive the explicit refusal with no additional
callback or RUN. There is no injection site outside the APM callback.

Success requires exit zero and the final `native_ich9_smi` JSON pass, with
`vm_count=2`, `max_live_vcpus=1`, request counts 2 positive plus 1 negative entry,
and every advertised acceptance boolean true. Capabilities and callback counts
alone are insufficient. All reads are completed through HV and echoed by the
guest; actual SMM exit tags and normal continuation supply delivery evidence.
The final stops are controller initiated, not ACPI S5 or a triple fault.
A 15-second internal watchdog covers the whole invocation; any separately
authorized future execution must also use an outer bound:

```sh
timeout --signal=TERM --kill-after=2s 20s ./ich9_smi
```

No runtime is authorized by these instructions. Unsupported hosts exit 2.
SCI, PM1/S5, physical EOS/source arbitration, SMI_STS, RTC, RCBA, ACPI tables,
reset/migration and board wiring remain separate work. No authenticated-variable,
TPM, Windows, BitLocker, `Vmm::start` or RPC support is inferred.

The subset follows pinned
[QEMU ICH9 LPC](https://github.com/qemu/qemu/blob/v11.1.2/hw/isa/lpc_ich9.c)
and [PM reset](https://github.com/qemu/qemu/blob/v11.1.2/hw/acpi/ich9.c), plus
[OVMF SmmControl2Dxe](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/SmmControl2Dxe/SmmControl2Dxe.c).
QEMU's general APM branch tests APMC alone; this narrower profile deliberately
rejects its mixed-enable trigger state. OVMF's virtual SMI is automatically
deasserted and needs no physical EOS operation. The
[Intel ICH9 datasheet](https://www.intel.co.uk/content/dam/doc/datasheet/io-controller-hub-9-datasheet.pdf)
Table 5-28 describes physical global/source enables and EOS arbitration; this
subset does not claim full physical ICH9 equivalence or fabricate those banks.
