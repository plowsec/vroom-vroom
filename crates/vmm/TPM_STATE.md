# TPM snapshot state

The swtpm backend runs TPM 2 with directory-backed state. Capturing a snapshot,
suspend, or pristine device state requires a successful `CMD_STORE_VOLATILE`,
followed by both `tpm2-00.permall` and `tpm2-00.volatilestate`. Each must be a
nonempty regular file of at most 1 MiB, matching the existing restore limit.
Read/export errors abort the capture instead of encoding a partial archive.
Errors name the failed operation/file and never include TPM state bytes.

Volatile state may legitimately be absent immediately after initialization:
swtpm consumes and deletes it when loading it. It must exist after successful
volatile export. Permanent state is established during TPM 2 initialization;
fresh manufacture commits its NV data even before the guest's first command.
An absent permanent file is therefore not an optional empty TPM. The separate
`savestate` blob is used by TPM 1.2 after `TPM_SaveState`; this TPM 2 backend does
not require or capture it. See upstream [swtpm control documentation](https://github.com/stefanberger/swtpm/blob/v0.10.2/man/man8/swtpm_ioctl.pod),
[STORE_VOLATILE handler](https://github.com/stefanberger/swtpm/blob/v0.10.2/src/swtpm/ctrlchannel.c),
[volatile-state storage](https://github.com/stefanberger/swtpm/blob/v0.10.2/src/swtpm/swtpm_nvstore.c),
[TPM 2 initialization](https://github.com/stefanberger/libtpms/blob/master/src/tpm_tpm2_interface.c)
and [manufacture](https://github.com/stefanberger/libtpms/blob/master/src/tpm2/TPMCmd/tpm/src/support/Manufacture.c).

`TpmBackend::save` now returns `vroom_snapshot::Result<Vec<u8>>`. Its default
remains deliberately stateless (`Ok(Vec::new())`), paired with the default
`load`. Persistent implementations must return errors when they cannot capture
complete state. Device restore always calls `load`, including for empty blobs.
Swtpm rejects malformed, missing, duplicate, unexpected, empty or oversized
state entries before stopping the current TPM or changing files. Complete
existing archives retain their JSON format; incomplete old captures are refused.

Capture never stops or reinitializes the backend. The existing VMM recovery
behavior is unchanged: failed snapshots resume a previously running VM; failed
suspends retain a paused VM that can resume. Neither failure commits a snapshot
manifest or stops workers. An incomplete output directory may remain, and a
disk overlay already created by snapshot preparation is not rolled back; the
current disk chain remains the live chain. Retry uses a fresh output directory.

This patch does not make valid-input restore file replacement transactional,
change reset error handling, or guarantee recovery from a dead TPM helper. It
does not establish cross-version portability, BitLocker unlock, protected UEFI
variables, or CPU-only snapshot portability. Default tests use injected faults
and a fake hypervisor. An explicitly ignored integration test can separately
exercise the existing adapter against a new disposable swtpm, without a guest.
