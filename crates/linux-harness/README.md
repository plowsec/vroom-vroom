# Native Linux harness

This is a bounded acceptance binary for the native x64 and ARM64 KVM backends, direct Linux loaders and serial devices. It is separate from `vroom run`, the VMM lifecycle and RPC. It creates one vCPU and 512 MiB RAM, directly loads a compatible Linux kernel and generated initramfs, and provides no guest disks, NICs, host mounts, firmware or TPM. The x64 path uses bzImage and COM1; the ARM64 path uses an ARM Image, device tree and PL011.

```sh
python3 scripts/make-linux-heartbeat.py --output /tmp/heartbeat.cpio --token 0123456789abcdef0123456789abcdef
cargo build --locked -p vroom-linux-harness
timeout --signal=TERM --kill-after=2s 70s target/debug/vroom-linux-harness /boot/vmlinuz-$(uname -r) /tmp/heartbeat.cpio 0123456789abcdef0123456789abcdef
```

Use a fresh random 32-character lowercase hex token for recorded runs. Use a reviewed packaged kernel; the invocation above assumes the current host kernel is appropriate. The harness accepts only bounded regular input files. For a cloud run, first verify the kernel's package ownership and hash, reviewed source hashes, budget, original host deadline and independent termination guard. Build offline from the reviewed vendored bundle. These examples do not authorize cloud provisioning or extend any host lifetime.

For ARM64, generate the initramfs with `--architecture arm64` and use the reviewed `linux_harness_arm64` bundle profile. Its package resolver also verifies the installed kernel configuration, and the compatibility precheck requires built-in GICv3, architectural timer, PSCI, PL011 console, initramfs and ELF support. The combined wrapper runs the native ARM CPU smoke before Linux acceptance. The combined native ARM CPU smoke and Linux harness passed on a Graviton bare-metal host, as recorded below. The packaged-kernel wrapper extracts a direct-boot Image from the verified compressed payload; no EFI firmware runs.

Success requires five exact sequential PID1 heartbeats, a worker acknowledgement that execution is paused, 350 ms with no change in either heartbeat count or serial byte count, five more sequential heartbeats after resume, and destruction of the vCPU with a successful worker join within two seconds. Serial output is limited to 128 KiB; the whole process has a 65-second watchdog. JSON goes to stdout; a bounded serial tail goes to stderr on failure. Exit codes are 0 pass, 1 unmet/error, and 2 unsupported/watchdog unknown.

`hypervisor_initiated_guest_stop_verified` means the controller destroyed the guest vCPU. It does not mean guest ACPI poweroff or AWS host shutdown. The guest's PID1 intentionally continues until stopped by the controller. Guest poweroff, full VMM/RPC integration, snapshots, serial RX/IRQ delivery, Secure Boot, TPM, BitLocker and Windows remain unverified by this harness.

The small PC profile uses KVM's actual PIT/PIC and disables ACPI, PCI and IOAPIC use in the guest command line. KVM provides port 0x61 through [`KVM_PIT_SPEAKER_DUMMY`](https://docs.kernel.org/virt/kvm/api.html#kvm-create-pit2); unmapped ports read as an absent device, without fabricated RTC/counter values. COM1 implements byte registers, the Linux 8250 probe/loopback, FIFO receive thresholds, receive timeout and interrupt priorities. Transmit drains synchronously to the host sink. The harness supplies no external serial input, and its UART timeout service runs at vCPU exits; it does not independently wake a guest for RX timeout. A future input path needs a separate periodic service/wakeup before claiming RX delivery.

Validation: the native x64 Linux harness passed on an AWS `m7i-flex.large` nested-KVM host on 2026-11-07. The reviewed package resolver verified the canonical Amazon Linux 6.18.56 kernel payload against its recorded RPM SHA256 before execution. All 109 native Rust library tests and five heartbeat-helper tests passed. The actual one-vCPU, 512 MiB guest produced five sequential PID1 heartbeats, acknowledged pause, remained quiescent for 350 ms, resumed to ten heartbeats, and stopped through the hypervisor with a successful worker join. The SSM command exited zero with complete captured output. This proves the bounded harness acceptance above; the separately listed VMM/RPC, firmware, security, input and guest-poweroff features remain unverified.


ARM validation: on 2026-11-07, the reviewed static AArch64 musl package passed on an AWS `c6g.metal` host. All 114 native Rust tests and 19 helper tests passed, followed by the CPU smoke and one-vCPU, 512 MiB Linux acceptance. The canonical Amazon Linux 6.18.56 kernel and configuration matched their recorded RPM SHA256 values. Five PID1 heartbeats preceded acknowledged pause/quiescence; execution resumed to ten heartbeats and then stopped through the controller with successful worker cleanup. SSM returned exit zero with complete output. The disposable host, root disk and temporary controls were removed and independently audited. Full VMM/CLI/RPC, firmware/security, serial input and Linux guest-poweroff behavior remain outside this acceptance.


A separate native x64 `Vmm::start` lifecycle acceptance subsequently passed; see [the restricted VMM profile](../vmm/X64_LINUX.md). That result is distinct from this standalone harness, and neither establishes CLI/RPC or firmware/security support.
