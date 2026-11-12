# Native MCH/SMRAM binding prerequisite

`examples/mch_smram.rs` composes the cold-only MCH router with the existing
fixed-Q35 KVM SMRAM owner. This is a bounded machine-code harness, not an OVMF,
Windows or complete 512 MiB board. No firmware/Windows VMM profile is enabled.
The tiny ordinary-RAM mapping proves register-to-backend binding at the fixed
SMRAM geometry; a future board must supply its complete RAM/ROM/reserved map.

The device crate has no HV dependency. `MchPci::with_q35_smram` requires an
inventory covering private default-SMBASE `0x30000..0x50000` and 8 MiB TSEG
`0x1f800000..0x20000000`, excluded from GuestMemory, fixed MMIO, ECAM and BAR
windows even while CPU-open. The caller must first configure that exact real
backend; the device constructor cannot independently prove hidden ownership.
Existing constructors keep SMRAM unsupported and legacy ARM snapshot bytes stay
unchanged. BAR routing retains its full-map validation and locking.

Only the selected firmware's register subset is accepted:

| Offset | Width | Supported operation |
| --- | --- | --- |
| `0x50` | 2 | Extended TSEG query `0xffff`, readback 8 MiB |
| `0x52` | 2 | GGC = 2: no integrated graphics or stolen graphics memory |
| `0xa4`, `0xa8` | 4 | GBSM/BGSM = `0x20000000` |
| `0xac` | 4 | TSEGMB = `0x1f800000` |
| `0xb0` | 2 | TOLUD = `0x2000` (512 MiB, MiB shifted by 4) |
| `0x9c` | 1 | Default-SMBASE query and permanent lock |
| `0x9d` | 1 | G_SMRAME, D_LCK and masked D_CLS; no legacy D_OPEN view |
| `0x9e` | 1 | 8 MiB TSEG size and T_EN; no H_SMRAME view |

Registers have explicit reset values and exact aligned access widths. Geometry
writes must match the already-owned backing, rather than advertising relocation.
TSEG is closed only when G_SMRAME and T_EN are both set. D_LCK requires that closed
state and makes its visibility irreversible; default-SMBASE locks independently.
Locked writable masks ignore attempted downgrades. Default-SMBASE query recognition
occurs after its current write mask: first `0xff` returns 1, second `0xff` locks
to 2, and all subsequent writes remain 2. PAM, legacy SMRAM, graphics, other TSEG
sizes, relocation, reset/S3 and unlisted registers remain unsupported. The 8 MiB
profile is explicitly selected here, not QEMU's generic extended-TSEG default.

Every accepted new-bank write, including query, geometry and masked no-op writes,
uses the nonstored callback supplied to `pio_write_with_smram` or
`mmio_write_with_smram`. The router prepares a candidate, latches poison, calls
the actual exiting vCPU, verifies the returned state, then commits readback and
clears poison. Invalid requests and missing callbacks have no side effects.
Callback error, mismatched return or unwinding panic permanently poisons all
router paths, including CF8 selectors, fixed MMIO and BARs. The caller must stop
the VM. Callback code must not reenter PCI/router/endpoint locks. It receives no
authorization token: the real HV enforces its one-shot write-exit grant and
irreversible locks. Infallible BusDevice routing must not be used for this path.

The compiled harness uses real CF8 and ECAM through one MCH state, programs the
fixed geometry, opens/closes/reopens TSEG, initializes both private ranges using
CPU stores, locks both, and checks masked unlock readbacks. Each read completes
through HV and the guest outputs the received value. Actual SMI/RSM must then
prove AS1 access and return to normal context; two fresh negative VMs must reject
normal private-memory access. The public VM handle is dropped before transitions.
There are at most three sequential VMs, one live CPU, less than 9 MiB backing per
VM, no disks/network, and a 15-second process watchdog. A future separately
authorized Linux invocation must also use an outer 20-second watchdog:

```sh
timeout --signal=TERM --kill-after=2s 20s ./mch_smram
```

Only pure tests and Linux-target compilation have been performed for this slice.
Neither this composed harness nor the underlying SMI hardware path has a runtime
pass from this change. Firmware boot, protected variables, TPM, BitLocker and
Windows are not acceptance claims.

Register masks and default-SMBASE behavior follow [QEMU v11.1.2 q35.c](https://github.com/qemu/qemu/blob/v11.1.2/hw/pci-host/q35.c)
and [q35.h](https://github.com/qemu/qemu/blob/v11.1.2/include/hw/pci-host/q35.h).
The selected initialization and lock sequence comes from pinned EDK2
[SmmAccessPei.c](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/SmmAccess/SmmAccessPei.c),
[SmramInternal.c](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/SmmAccess/SmramInternal.c)
and [MemDetect.c](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/PlatformInitLib/MemDetect.c).
