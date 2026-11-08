# Fixed Q35 firmware layout

`vroom_arch::x86_64::Q35FirmwareLayout` is an immutable layout and metadata
builder for one CPU, nominal 512 MiB and the selected 4 MiB OVMF pair
(3,653,632-byte CODE and 540,672-byte VARS). It checks image **geometry**;
it does not authenticate image bytes, construct a VM or enable firmware boot.
The existing direct-Linux loader and ARM paths are unchanged.

All range getters return `(base, size)`. Two memory views are intentional:

| Range, exclusive end | Owner / visibility |
| --- | --- |
| `0..30000`, `50000..a0000`, `100000..1f800000` | Ordinary `GuestMemory`, CPU and device DMA; total 503.5 MiB |
| `30000..50000` | Backend-owned default-SMBASE; AS1 always, AS0 until lock; never DMA |
| `1f800000..20000000` | Backend-owned 8 MiB TSEG; AS1 always, AS0 only open; never DMA |
| `a0000..100000` | Legacy hole; no PAM/shadow RAM implementation advertised |
| `ffc00000..ffc84000` | Emulated VARS; separate from ordinary memory |
| `ffc84000..100000000` | Read-only CODE in both CPU address spaces |
| `ffbfc000..ffc00000` | Four KVM-private pages, excluded from devices and common mappings |

Addresses in this table are hexadecimal. A CPU-open SMRAM mapping never
becomes device-accessible memory. The fixed backend, rather than this module,
owns private backing, changes visibility and enforces irreversible locks.

`etc/e820` deliberately describes **nominal physical topology**: one RAM
entry covering `0..20000000`. This file is firmware-only, not an OS usable-RAM
map and not input for the direct-Linux loader. QEMU publishes nominal low RAM;
the pinned OVMF `PlatformGetLowMemoryCB` takes its highest RAM end, then
`PlatformQemuInitializeRam` subtracts the queried 8 MiB TSEG and publishes its
reservation. `QemuInitializeRamBelow1gb` independently reserves default-SMBASE.
Publishing `1f800000` as the RAM end would subtract TSEG twice and put it at
`1f000000`. CMOS memory registers derive from the same nominal end:
`34/35=00/1f`, `5b/5c/5d=0`; fw_cfg reports RAM_SIZE=512 MiB and NB_CPUS=1.

The same layout reserves ECAM at `e0000000..f0000000`, segment 0, buses 0–255,
and provides BAR windows `80000000..e0000000` and `800000000..1000000000`.
The latter stays inside the checked MCH router's 36-bit limit. The exact
136-byte `etc/hardware-info` file contains one version-0 host-bridge record:
attributes zero, CombineMemPMem set, DMA above 4 GiB false, extended config
space available, and these two memory apertures. Unused IO, prefetch-only and
ECAM descriptor pairs are zero; actual ECAM remains decoded by MCH at the
selected firmware's PCD address and must later agree with MCFG.

Both PEI and DXE ignore the descriptor's ECAM output. A nonempty ignored
aperture becomes `EFI_WARN_STALE_DATA`, which those callers tolerate; absence
is the minimal warning-free record, not a mandatory parser rule. DXE prefers
host-provided roots over fallback PCI scanning. These records bind its PCI
allocation windows; they do not mask CPUID or prove early firmware page-table
allocation. Fixed IOAPIC, HPET, RCRB and LAPIC reservations prevent BAR overlap;
a reservation alone does not advertise an implemented device. TPM is not yet
advertised by this layout.

The immediate consumer is `tests/firmware_layout.rs`. Its single memory test
reserves 503.5 MiB anonymous demand-zero virtual memory, touches less than
64 KiB, and exercises actual `FwCfgIo` selector/data/DMA and `MchPci` CF8/ECAM.
Device-state callbacks are explicit test acknowledgments, not KVM mapping
proof. Tests replay OVMF's HOB derivation and both PEI/DXE hardware-info
consumers, reject mismatched queries/CMOS/resources and malformed records,
and show private/ROM ranges remain absent from DMA throughout modeled state
changes. No test constructs or executes a VM.

```sh
cargo test --locked --offline -p vroom-arch --lib -p vroom-linux-harness --test firmware_layout
cargo check --locked --offline -p vroom-arch -p vroom-linux-harness --all-targets --target x86_64-unknown-linux-gnu
cargo check --locked --offline -p vroom-arch -p vroom-linux-harness --all-targets --target aarch64-unknown-linux-gnu
```

The production consumer remains to be implemented: bounded owned firmware
preflight retains this layout, configures the fixed Q35 backend before maps,
and supplies its ranges/metadata to RAM, ROM, MCH, fw_cfg, CMOS, ACPI and NVMe.
A worker-owned fallible board route must bind each chipset/flash operation to
the actual exiting vCPU; generic SMM routing stays refused. LPC services,
x64 ACPI, protected-variable persistence and actual reset-vector firmware boot
remain separate work. First acceptance must use `Vmm::start`, BootPaused,
an actual EFI marker, acknowledged pause/resume and checked host stop. These
source tests claim none of those runtime results or Windows security features.

Pinned sources: [QEMU PC memory topology](https://github.com/qemu/qemu/blob/v11.1.2/hw/i386/pc.c),
[OVMF memory/HOB consumers](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/PlatformInitLib/MemDetect.c),
[host-bridge record](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/HardwareInfoLib/HardwareInfoPciHostBridgeLib.h),
[aperture parser](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/HardwareInfoLib/HardwareInfoPciHostBridgeLib.c),
[DXE root selection](https://github.com/tianocore/edk2/blob/4dfdca63a93497203f197ec98ba20e2327e4afe4/OvmfPkg/Library/PciHostBridgeUtilityLib/PciHostBridgeUtilityLib.c).
