# bulwark

A small, security-first virtual machine monitor in Rust. Runs Linux and
Windows 11 ARM64 guests on Apple Silicon (Hypervisor.framework) and on
arm64 Linux (KVM). Every device model treats the guest as hostile, and the
VMM process locks itself down (Seatbelt / seccomp + Landlock) before the
guest runs a single instruction.

## Status

What has been run end to end on an M3 Max, macOS 15:

| Feature | State |
|---|---|
| Ubuntu 24.04 arm64 (direct kernel or UEFI), Alpine | works |
| Windows 11 24H2 ARM64: unattended install, desktop, drivers | works (see below) |
| SMP, snapshots, suspend/resume (disk-consistent), reset | works |
| Snapshot housekeeping (list, delete, merge unused disk layers) | works |
| Disk images locked against concurrent use; TRIM/discard returns space | works |
| UEFI (EDK2), ACPI, SMBIOS 3.0, NVRAM | works |
| virtio: blk, net, console, rng, gpu 2D, input; NVMe; xHCI USB HID | works |
| virtio-gpu 3D (Venus/Vulkan → MoltenVK → Metal), sandboxed | works, ~92% of native compute |
| Dynamic resolution (Linux natively, Windows via agent) | works |
| Clipboard both ways (Linux agent; Windows service + unprivileged helper) | works |
| NAT networking (DHCP, DNS, TCP/UDP, ICMP, port forwards), several NICs | works |
| Shared folders (virtio-fs; read-only by default) | works (Linux guests) |
| Sound output (virtio-snd; a helper process plays it; no microphone path) | works (Linux guests; macOS host: sandboxed CoreAudio helper) |
| Bridged / host-only (privileged `bulwark-netd`) | plumbing tested; real vmnet/tap needs root, not yet run |
| Nested virtualization (KVM inside an Ubuntu guest, M3+) | works |
| KVM backend (arm64), incl. snapshots | works (tested nested on HVF) |
| x86_64 hosts | builds (VMM on Linux, agents for x64 Windows/Linux); no x86 board yet, so VMs don't start |
| vTPM 2.0 (swtpm in its own sandboxed process; TIS, ACPI TPM2, measured boot) | works (EDK2, Linux, Windows 11 sees TPM 2.0; state survives suspend) |

Honest caveats live in [docs/SECURITY.md](docs/SECURITY.md) and at the end of
[docs/GUIDE.md](docs/GUIDE.md).

## Build

```bash
scripts/build.sh            # bulwark + bulwark-gui, signed with the hypervisor entitlement
scripts/build.sh --release
```

Every rebuild of `target/*/bulwark` must be re-signed (`scripts/sign.sh`),
otherwise Hypervisor.framework refuses to create the VM.

`scripts/dist.sh` makes a release tarball (signed binaries, guest agents,
the 3D library if built, docs) in `target/dist/`.

Optional pieces:

- 3D (macOS): `scripts/build-virglrenderer.sh` builds the patched
  virglrenderer next to the binaries (needs `brew install meson ninja libepoxy molten-vk`).
- Windows guest agent: `cargo xwin build -p bulwark-agent --release --target aarch64-pc-windows-msvc`.
- Linux guest agent / VMM for arm64 Linux: `cargo build --release --target aarch64-unknown-linux-musl -p bulwark-agent -p bulwark-cli`.

## Run

```bash
target/debug/bulwark new dev --os linux --disk 32G --iso ubuntu.iso   # dev.toml + dev.qcow2
target/debug/bulwark run examples/ubuntu.toml      # serial console in this terminal
target/debug/bulwark-gui examples/ubuntu.toml      # window (display, mouse, clipboard)
target/debug/bulwark check examples/ubuntu.toml    # validate a config
target/debug/bulwark disk create vm.qcow2 --size 64G
target/debug/bulwark snapshot list examples/ubuntu.toml   # also: delete <snap>, prune
```

Serial console escapes (`Ctrl-A` then): `x` power off, `d` ask the guest to
shut down, `z` suspend, `s` snapshot, `p` pause/resume, `r` reset, `c`/`v`
push/pull clipboard, `g` screenshot (PNG in the state dir).

GUI host shortcuts (`Ctrl+Alt+Shift` +): `S` snapshot, `Z` suspend, `P`
pause, `R` reset, `D` guest shutdown, `Q` power off. Closing the window
suspends the VM; the next start resumes it (`--fresh` to cold boot).

State (NVRAM, snapshots, suspend image, disk overlays, VM UUID) lives in
`<config dir>/<name>.state` unless `--state-dir` says otherwise.

## Configuration

One TOML file per VM. Unknown keys are errors, so a typo can't silently
turn off a security setting. Relative paths are relative to the file.

```toml
name = "dev"
cpus = 4
memory_mib = 4096
nested = false                 # KVM inside the guest (M3+, macOS 15+)
tpm = false                    # TPM 2.0 via swtpm (brew install swtpm); Windows 11 wants it
virtio_transport = "auto"      # auto (mmio for kernel boot, pci for uefi) | mmio | pci

[boot]
type = "uefi"                  # or "kernel"
vars = "dev.state/efivars.fd"  # NVRAM, created on first boot
# firmware = "/path/QEMU_EFI.fd"   (default: search Homebrew/distro paths)
# kernel boot: kernel = "...", initrd = "...", cmdline = "..."

[[disk]]
path = "dev.qcow2"
format = "qcow2"               # auto | raw | qcow2
bus = "virtio"                 # virtio | nvme (Windows setup needs nvme)
sync = "fsync"                 # fsync (default) | full (power-loss safe, slow on macOS) | none
# readonly = true, cdrom = true

[[net]]
mode = "nat"                   # nat | bridged | host-only
port_forwards = [{ host_port = 2222, guest_port = 22 }]
# allow_host_loopback = false, block_private = false
# bridged: interface = "en0"; host-only: network = "lab"  (both need bulwark-netd)

[display]
gpu = "virtio2d"               # none | virtio2d | virtio3d
width = 1280
height = 800

[[share]]                      # host folder, Linux guest: mount -t virtiofs host /mnt
path = "/Users/me/shared"      # absolute, or relative to this file
tag = "host"
readonly = true                # default; false lets the guest change files

[audio]
output = false                 # guest sound out (Linux guests); there is no input

[clipboard]
host_to_guest = true
guest_to_host = true
max_bytes = 1048576

[serial]
type = "stdio"                 # stdio | file (path = ...) | none

[security]
sandbox = true
```

## Layout

`crates/`: `hv` (HVF/KVM), `memory`, `devices` (UART, RTC, fw_cfg, flash,
PCI, NVMe, xHCI, TPM), `virtio` (incl. gpu, fs, sound), `arch` (memory map,
FDT, ACPI, SMBIOS, kernel loader), `block` (raw, qcow2), `net` (NAT, netd
protocol), `fs` (shared folder host side), `audio` (sound helper),
`snapshot`, `vmm` (board + run loop), `sandbox`, `gpu3d` (virglrenderer),
`display`, `agent`, `proto`, `config`, `cli`, `gui`, `netd`, `ipc`.
`fuzz/`: cargo-fuzz targets for everything a guest can feed us.

More: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md),
[docs/SECURITY.md](docs/SECURITY.md), [docs/GUIDE.md](docs/GUIDE.md),
[docs/NETWORKING.md](docs/NETWORKING.md).
