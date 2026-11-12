# PC RTC for first EFI boot

`vroom_devices::pc_rtc::PcRtc` supplies checked byte accesses at ports `70/71`
for the selected OVMF calendar/CMOS sequence. It is a restricted cold device,
not a complete MC146818, wakeup clock or interrupt implementation. No VMM
profile is enabled, and these source tests do not prove firmware execution.

The caller supplies a UTC Unix-seconds epoch once and
`Q35FirmwareLayout::cmos_memory()`. Each checked access supplies monotonic
nanoseconds since that epoch. The future worker must use the same elapsed
clock as the PM timer and subtract acknowledged pause durations. The device
never reads or changes the host clock/NVRAM, starts a thread, or maps memory.
It implements no infallible `BusDevice` or snapshot adapter.

The supported mode is BCD, 24-hour time, with the selected firmware's
1998–2097 year pivot. Fresh register B is `02`; only `02` and `82` (SET) are
accepted. Active PIE/UIE/AIE, square-wave/daylight and binary/12-hour modes
return errors before committing state. Register A accepts the observed `26`
oscillator/rate configuration; UIP is read-only and derives from the final
`8 / 32768` second before each calendar update. SET freezes calendar/UIP.
Missed elapsed time is converted with bounded date arithmetic, not tick loops.

Calendar writes require SET. Clearing it with calendar changes requires all
six fields and a valid date; invalid/incomplete candidates leave staging and
the clock unchanged so they can be repaired. Repeating `82` preserves staging;
repeating `02` preserves fractional progress. An alarm-only SET cycle succeeds
without calendar writes and resumes the frozen calendar. Alarm fields `1/3/5`
are private storage only: enabling an alarm is rejected, and there is no
matching, wakeup or IRQ claim. Weekday is derived; weekday writes are outside
this profile.

Register C exposes only coalesced real calendar-update UF and clears it on
read; it never asserts AF/PF/IRQF. C/D writes are ignored, including OVMF's
initial D=`0`; D reads VRT=`80`. Port `70` reads `ff`, matching pinned QEMU.
Writes select the low seven bits and retain the NMI-mask bit internally;
no NMI injection path is implemented. EDK2's read-preserve-write selection
sequence works with this behavior. Ordinary CMOS, including memory cells
seeded from the authoritative layout, is private byte storage. Guest writes
cannot change host memory mappings and imply no warm-reset persistence.

The actual selected `PcRtcInit` sequence writes A=`26`, reads B/C, writes D=`0`,
waits for valid time/non-UIP, reads calendar, sets B=`02`, then performs a full
SET transaction. It also reads disabled alarm fields and may repair only those
fields under SET. `GetCenturyRtcAddress` returns zero without a FADT; calendar
conversion still uses the configured year pivot. Thus real calendar/CMOS is a
first-EFI prerequisite, while an active IRQ8 worker and full ACPI tables can
remain deferred. S5 and RCBA remain separate LPC/board requirements.

Pure device tests cover calendar boundaries, leap years, UIP, monotonic/range
failures, idempotence, repair, disabled storage and side-effect-free rejection.
The integration test replays the selected firmware's port transactions against
the real device with layout-provided CMOS and explicit simulated elapsed time:

```sh
cargo test --locked --offline -p vroom-devices --lib
cargo test --locked --offline -p vroom-linux-harness --test pc_rtc_boot
```

Both Linux target checks compile this code. No test starts a VM, helper,
firmware, IRQ thread or cloud resource. Production must route errors through
the fallible worker-owned board path before another guest entry. Actual EFI
GetTime/SetTime and shutdown acceptance remain pending that integration.

Pinned sources: [RTC transactions and year conversion](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/PcAtChipsetPkg/PcatRealTimeClockRuntimeDxe/PcRtc.c),
[register definitions](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/PcAtChipsetPkg/PcatRealTimeClockRuntimeDxe/PcRtc.h),
[selected PCD defaults](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/PcAtChipsetPkg/PcAtChipsetPkg.dec),
[QEMU index/UIP behavior](https://github.com/qemu/qemu/blob/v11.1.2/hw/rtc/mc146818rtc.c).
