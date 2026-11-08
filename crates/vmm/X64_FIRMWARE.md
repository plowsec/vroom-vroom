# Native x64 firmware: owned preflight only

The production `Vmm::start` x64 branch can validate and own inputs for a
future first-EFI board. **Even valid inputs return `native x64 firmware
execution is not enabled`, before creating VM state or a VM.** Invalid
inputs return their preflight errors. The existing direct-Linux x64 board
and ARM firmware path are unchanged.

This preflight requires x64 KVM, the default feature set, one CPU, nominal
512 MiB RAM, `BootPaused`, sandboxing, explicit `os = "other"`, default
identity/transport/headless settings, disabled clipboard, and stdio serial.
It accepts exactly one read-only raw NVMe disk input (not a CD-ROM), from
1 MiB through 32 MiB in whole 512-byte sectors. Other devices are rejected.
Disk bytes are opaque here: this does not prove a valid ESP or EFI program.

UEFI input paths must explicitly name the selected CODE and an existing
blank VARS template, with `secure_boot = false`. The selected QEMU 11.1.2
SMM-requiring OVMF pair is pinned by size and SHA-256:

| Input | Bytes | SHA-256 |
| --- | ---: | --- |
| CODE | 3,653,632 | `32807682a9e5c0e2d192ecb6077941d4be8bcfa6b9cd6e60830c3a9f48c0555a` |
| Blank VARS | 540,672 | `5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e` |

The blank template is in Setup Mode. Firmware capability is not evidence
of enrolled keys, protected variable enforcement, TPM, Windows or BitLocker.
This inspection neither changes the meaning of persistent UEFI settings nor
enables a volatile boot profile; that selection needs separate implementation.

Each input is opened read-only, nonblocking and without following a final
symlink. Regular-file type, length and alignment are checked before reading.
The same descriptor is inspected before and after a bounded read; device,
inode, size and modification/change timestamps must match. SHA-256 covers
the exact owned bytes. Metadata checks are not a lock against every external
write race; the CODE/VARS pins authenticate the resulting snapshots, while
the disk digest identifies its contents without asserting payload provenance.

The prepared owner retains all original descriptors and immutable byte
snapshots. Path replacement or subsequent same-inode writes cannot change
those snapshots. The in-memory disk checks complete read spans and rejects
every mutating operation, including discard and zero-length writes. Debug
formatting includes identity, sizes and hashes, never input contents.

No input is created, resized, replaced, backed up, enrolled or written.
There is no firmware search, image-format sniffing, disk overlay, helper,
viewer, state lock, UUID file or hypervisor creation. Permitted input opens
must not be confused with later device/storage setup. Tests use a private,
thread-scoped `cfg(test)` digest seam for synthetic images; no production
environment variable, configuration or public API can change the pins.

## Remaining execution gates

The eventual board must consume these same owners without pathname reopen,
route actual same-RUN SMM context through its sole vCPU worker, and use the
fixed Q35 layout without exposing private SMRAM to device DMA. Its read-only
NVMe namespace can use the existing inline-only `Nvme::new` construction;
it must not opt into asynchronous workers through `into_shared()`.

First native acceptance is limited to an exact EFI marker from a reviewed
read-only ESP and typed normal ACPI S5, with no RUN after S5. It must prove
`BootPaused` zero entry, checked initialization and complete worker joins.
First-boot VARS may be explicitly volatile, with no durability claim. Real
post-join checked persistence is required before protected-variable or
BitLocker cold-restart acceptance. None of those runtime gates is satisfied
by this source-only preflight; no firmware or Windows profile is enabled.
