# Disposable Linux TPM reference PID1

This standalone static x64 Linux payload is a QEMU reference fixture, not native
vroom/Windows/BitLocker acceptance or independent firmware attestation. It uses
only cached pinned `libc`, `sha2` and their locked dependencies.

```
cargo test --locked --offline
cargo build --locked --offline --release --target x86_64-unknown-linux-musl
```

Install `target/x86_64-unknown-linux-musl/release/vroom-tpm-reference-init` as
`/init`. Include `/etc/vroom-reference-token`: exactly 32 lowercase hexadecimal
bytes, no newline; `/dev/console` (character 5:1); and `/dev/null` (1:3). Use the
same kernel and exact same initramfs on both boots. The signed EFI launcher
separately verifies the initramfs hash before starting Linux.

The PID1-only program mounts guest proc/sysfs/devtmpfs/securityfs, reopens the
console, and uses `/dev/tpm0` and the kernel's binary firmware log. It accesses
no network, guest data disk, host share or raw flash. The controller owns its
reviewed maximum 180-second VM / 10-minute campaign bounds and full teardown.

The acceptance sequence is:

1. Require TPM2/SHA256 PCR7. Parse bounded TCG2 records using SpecID digest
   widths, skip EV_NO_ACTION, verify supported PCR7 event-data hashes, require
   a measured SecureBoot=1 variable, and compare replay against live PCR7.
   This establishes measurement consistency, not independent firmware trust
   or protection of authenticated variables.
2. Only the exact missing-index response for `0x01500016` selects phase `seal`.
   Create an ECC P256 owner primary, obtain PCR7 policy using a **trial** session,
   generate a random 32-byte value with TPM2_GetRandom, and **TPM2_Create** a
   sealed object. Attributes `0x492` clear USERWITHAUTH: policy authorization is
   required even though the object's password is empty.
3. **TPM2_Load**, require password-only Unseal AUTH_UNAVAILABLE/AUTH_TYPE and
   empty-real-policy Unseal POLICY_FAIL, then use a separate **real** PolicyPCR
   session and **TPM2_Unseal**. Verify the returned value's hash. No PCR changes.
4. Persist only wrapped private/public blobs, token, public primary name,
   PCR/policy metadata and value hash in ordinary owner-read/write NV. Verify
   readback. Ordinary NV reads are never substituted for sealed-object Unseal.
5. Phase `unseal` validates existing NV attributes/size/token, unchanged PCR7
   and recreated primary name, then loads the persisted object and repeats all
   authorization checks. No fallback creates a replacement object.

One success line has prefix `VROOM_TPM_REFERENCE ` and JSON keys:

```
schema, token, phase, status,
pcr7_sha256, event_log_sha256, pcr7_event_count,
replay_matches, secure_boot_measured,
sealed_blob_sha256, secret_hash,
password_auth_rejected, empty_policy_rejected, policy_unseal_verified
```

Require seal then unseal, exact token, true boolean gates, positive event count,
and matching PCR7/sealed-blob/value hashes. The full event-log hash may differ.
The host must also observe clean guest shutdown and complete QEMU/swtpm exit
between boots. Failure reports contain only a static stage and optional numeric
TPM return code. Poweroff alone is never a pass. No values, keys, raw TPM replies
or blobs are logged.

Limits: TPM packets 4096 bytes; log 1 MiB; event 64 KiB; 4096 records; 16 digest
algorithms; NV bundle 1536 bytes and transfers 512 bytes. Malformed/truncated
records, wrong attributes/selection, duplicate entries and trailing data fail.
Temporary sensitive buffers are overwritten on normal paths without a secure
erasure guarantee. Unsalted sessions and empty owner/parent authorization are
limited to this fresh disposable test TPM, not recommended for real secrets.

The ignored `helper_codec` test requires a separately reviewed external
supervisor: fresh swtpm, execution plus cleanup within 30 seconds, no VM,
both children reaped and state deleted. It validates command encoding and
opaque-NV roundtrip, not firmware or cold-boot behavior. No helper supervisor
or helper launch is included in this source-only extraction.

Primary references: [tpm2-tss types](https://github.com/tpm2-software/tpm2-tss/blob/master/include/tss2/tss2_tpm2_types.h),
[SAPI command layouts](https://github.com/tpm2-software/tpm2-tss/tree/master/src/tss2-sys/api),
[libtpms authorization processing](https://github.com/stefanberger/libtpms/blob/master/src/tpm2/TPMCmd/tpm/src/main/SessionProcess.c).
SHA256 uses the pinned library; sealing and policy enforcement run in the TPM.
