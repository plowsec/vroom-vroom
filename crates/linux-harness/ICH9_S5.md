# Native normal ACPI S5 prerequisite

`examples/ich9_s5.rs` consumes the selected OVMF reset library's normal S5
sequence through native x64 KVM port exits. Only pure tests and Linux target
compilation have been performed; there is no native runtime pass, OVMF boot,
Windows support or production VMM/RPC integration here.

The checked LPC exposes only the two-byte `PM1_CNT` register at enabled
`PMBASE + 4`, with exactly aligned word accesses. PMBASE/ACPI_EN own its actual
decoding, including disable/rebase; IOSE does not gate it. Whole spans and the
existing immutable port reservations are checked before any effect.

This first-EFI subset starts with SCI disabled and supports only SLP_TYP zero:

| Access | Result |
| --- | --- |
| Word read | Zero: no SCI, BM_RLD/GBL_RLS or other unsupported state is fabricated |
| Word write zero | Handled, no terminal effect |
| Word write `0x2000` | Normal S5 through the typed power API only |
| Other nonzero bits | Explicit error, including SCI_EN and nonzero sleep types |

SLP_EN bit 13 is a command, never retained storage. There is no invented
clear-write arming requirement; the two-step sequence below is the firmware's
observed usage. S1/S3/S4, active PM events, GPE, SCI IRQ9 and SCI mode commands
2/3 remain unsupported. Existing timer, APM SMI and SMRAM behavior is preserved.

`lpc_pio_write_with_power` returns a `#[must_use] LpcPioWriteOutcome`:
`Unmapped`, `Handled` or `AcpiS5`. A sleep-triggering write requires Normal
context derived from the actual current CPU exit. SMM-context S5 is outside
this initial profile. Both older bool-returning APIs, including the SMI API,
reject an S5 effect before changing terminal state. Conversely the power API
cannot inject an APM SMI: B2 still requires the separate callback API.

Before `AcpiS5` escapes, the LPC latches a terminal state. All checked router
routes then fail before callbacks or buffer changes: CF8, ECAM, BAR/fixed MMIO
and every LPC port. The distinct `PoweredOff` error is normal terminal state,
not an ambiguous SMI/backend poison. Only constructing a new cold VM can reset
it; snapshots/reset/migration are not enabled by this change.

The actual owner must consume `AcpiS5` by stopping the exiting CPU **without
another RUN or read completion**, then disposing its owned CPU/VM backing.
The device cannot secretly stop a separately owned CPU. The native example
uses this real ownership path; pure tests exercise the same consumer with an
ordinary scripted CPU and prove that a queued successor exit is never fetched.
An unhandled outcome, bool success, HLT, triple fault, timeout, external stop
or unrelated `SystemOff` exit cannot satisfy S5 acceptance.

The selected OVMF `DxeResetShutdown` performs these word read-modify-write
operations at its fixed Q35 PMBASE `0x600`:

```text
read PM1_CNT; clear bits10..13 (AND AX,0xC3FF); write PM1_CNT
read PM1_CNT; set bit13 (OR AX,0x2000); write PM1_CNT
```

The native program reproduces both actual `IN AX` / modification / `OUT AX`
operations. Each read is completed through HV and echoed by the CPU before
its value is modified; the echo leaves AX intact. The first write must return
`Handled` and execution continues. The second actual normal write must return
`AcpiS5`; the owner immediately leaves the loop and drops its CPU, router,
RAM and VM. A forbidden successor marker exists in the guest code but has no
accepted execution step. No SMI or hidden host shutdown callback substitutes
for this sequence.

There is one fresh VM and one live vCPU, with less than 9 MiB of backing:
64 KiB common RAM, 64 KiB reset ROM and the existing fixed-Q35 128 KiB plus
8 MiB private ranges. No media, OS, firmware, disk, network or device workers
are present. A 15-second internal watchdog covers the whole invocation. Any
separately authorized future execution must also use an outer bound:

```sh
timeout --signal=TERM --kill-after=2s 20s ./ich9_s5
```

Success requires exit zero and the final `native_ich9_s5` JSON pass with
`vm_count=1`, `max_live_vcpus=1`, both read/echo and typed-consumption flags true,
`shutdown_reason="acpi_s5"`, and `no_run_after_shutdown=true`. Unsupported hosts
exit 2. No runtime is authorized by these instructions.

The selected first-EFI source paths do not require active SCI for this reset
sequence. This does not make the board firmware-ready: RCBA is unconditionally
programmed before PCIEXBAR in PlatformInit, RTC/calendar is another prerequisite,
and no matching ACPI tables or full board have been wired here. RCBA/PAM,
RTC, SCI/events, other sleep states, VMM lifecycle and Windows/TPM/BitLocker
acceptance are outside this slice.

Primary contracts are pinned
[OVMF DxeResetShutdown](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/ResetSystemLib/DxeResetShutdown.c),
[Q35 PMBASE](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Include/OvmfPlatforms.h)
and [QEMU PM1 control](https://github.com/qemu/qemu/blob/v11.1.2/hw/acpi/core.c).
QEMU's SLP_TYP zero plus SLP_EN requests guest shutdown and masks SLP_EN out of
readback. This implementation deliberately accepts only the word subset used
by that firmware path rather than pretending to implement every chipset bit.
