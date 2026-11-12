# Linux firmware and TPM reference

Source-only QEMU/EDK2/swtpm fixture for normal authenticated-variable updates,
Secure Boot image refusal, PCR7/event-log consistency and a real PCR-policy
sealed object's persistence across two Linux cold boots. It does not establish
native vroom firmware, Windows, BitLocker, ARM firmware, migration, independent
firmware attestation or variable-storage attack resistance.

The EFI and Rust guest sources preserve the reviewed reference campaign. This
durable packaging has local unit/build validation only and does not authorize
another guest campaign. No keys, certificates, kernels, media, binaries,
runtime reports or account details belong in source control.

## Inputs and local checks

Use Python 3.11+, an installed Rust x64 musl target and cached locked crates.
The reference used QEMU 11.1.2 (`pc-q35-11.1`, TCG/SMM), swtpm 0.10.2, Secure
Boot-capable x64 EDK2 with a blank compatible variable template, OpenSSL 4.1.0,
osslsigncode 2.15, mtools 4.0.50 and an x64 EFI-stub Linux kernel. The kernel
needs built-in TPM TIS, securityfs, devtmpfs, serial, EFI and initramfs support.
Verify these inputs separately. Nothing here installs or downloads missing
inputs.

Copy `inputs.example.json` into ignored `output/`, fill every absolute path,
and supply the independently verified kernel SHA256. `lld_link` is the EFI
linker; `llvm_library` identifies its installed LLVM shared library, whose
containing directory is supplied to the macOS loader. Candidate manifests
freeze the resolved input paths and hashes.

Unit tests do not start a TPM helper or guest:

```sh
python3 -I -m unittest discover -s tests/firmware-reference/tests -v
cargo test --locked --offline --manifest-path tests/firmware-reference/pid1/Cargo.toml
```

The optional EFI compile/native SHA check requires `VROOM_EFI_CLANG`,
`VROOM_EFI_LLD_LINK` and `VROOM_EFI_LLVM_LIBRARY`; otherwise that check is skipped.
The Rust helper integration test stays ignored. No helper supervisor is included.

Build fresh PID1 provenance using installed toolchain binaries, not rustup
proxy symlinks:

```sh
python3 tests/firmware-reference/build-pid1.py \
  --output tests/firmware-reference/output/pid1-build \
  --cargo /path/to/toolchain/bin/cargo \
  --rustc /path/to/toolchain/bin/rustc \
  --linker /path/to/toolchain/bin/rust-lld \
  --llvm-library /path/to/toolchain/lib/libLLVM.dylib
```

This offline locked build records sources, lockfile, compiler, linker, installed
target libraries and binary hashes in `pid1-build.json`. It is build provenance,
not a signed attestation or exhaustive toolchain supply-chain verification.

## Preparing and reviewing a campaign

Persistent outputs must be below repository `.aws-pilot/` or this fixture's
ignored `output/`. QEMU option delimiters and symlink escapes are rejected.
Use new output directories for each build and candidate.

```sh
python3 tests/firmware-reference/prepare.py \
  --inputs tests/firmware-reference/output/inputs.json \
  --pid1-build tests/firmware-reference/output/pid1-build/pid1-build.json \
  --output tests/firmware-reference/output/candidate
python3 tests/firmware-reference/run.py \
  --candidate tests/firmware-reference/output/candidate
```

Preparation compiles EFI apps, creates disposable PK/KEK/db keys and an
unenrolled signer, signs media, and deletes private signing keys in `finally`.
It never mounts media or starts a guest. The default runner verifies inputs
and prints commands. Review the exact manifest and commands before supplying
`--execute-reviewed MANIFEST_SHA256 --report NEW_JSON`. Reports must also be
under an ignored output root. Campaigns are single-use, with no automatic retry,
fallback or deadline extension.

The fixed campaign uses one emulated x64 CPU and 1 GiB RAM, no NIC or host
sharing, read-only firmware code/FAT, and fresh writable TPM/NVRAM state with
private Unix sockets. Each boot fits within 180 seconds including cleanup;
three boots and cleanup fit within 600 seconds. Both children must be reaped
before state deletion; an unproven cleanup retains the exact private path.

1. Enrollment requires SetupMode=1/SecureBoot=0, installs fresh guest-only
   db/KEK/PK, re-reads their values and attributes, then shuts down.
2. Both Linux boots require SecureBoot=1/SetupMode=0, reject an unenrolled signed
   db update and accept a newer KEK-signed update preserving db. A signed child
   executes once. The hash-bound unsigned image must return ACCESS_DENIED/null
   handle or SECURITY_VIOLATION/valid loaded image followed by successful unload;
   it is never started. The signed loader verifies the exact initrd hash.
3. PID1 replays SHA256 PCR7 and checks the live PCR and measured SecureBoot=1.
   Real TPM2_Create/Load/Unseal requires PCR policy; password and empty-policy
   authorization fail. Only wrapped blobs and metadata persist in fresh TPM NV.
   Cold boot must reproduce the PCR, object and value hashes. Ordinary NV reads
   do not replace sealed-object unsealing.

Acceptance also requires guest-shutdown QMP events, zero QEMU exits and complete
process cleanup. Reports retain statuses/hashes, never signing keys or the
unsealed value. See [the PID1 contract](pid1/README.md) for buffer bounds and
fixture-only authorization limitations.

Primary interfaces: [UEFI runtime services](https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html),
[Secure Boot](https://uefi.org/specs/UEFI/2.10/32_Secure_Boot_and_Driver_Signing.html),
[EDK2 image lifetime](https://github.com/tianocore/edk2/blob/master/MdeModulePkg/Core/Dxe/Image/Image.c),
[QEMU TPM](https://www.qemu.org/docs/master/specs/tpm.html),
[efitools Auth2 construction](https://github.com/vathpela/efitools/blob/master/sign-efi-sig-list.c).
