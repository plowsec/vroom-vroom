# Guide

## Prerequisites (macOS, Apple Silicon)

```bash
brew install qemu            # only for the EDK2 firmware files and qemu-img
scripts/build.sh
```

`qemu` is not used to run anything: bulwark only reads
`/opt/homebrew/share/qemu/edk2-aarch64-code.fd` (UEFI) from it.

## Ubuntu 24.04 (cloud image, direct kernel boot)

```bash
mkdir -p images/ubuntu && cd images/ubuntu
curl -LO https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img
mv noble-server-cloudimg-arm64.img noble.img
# kernel + initrd from the same release
curl -L -o vmlinuz https://cloud-images.ubuntu.com/noble/current/unpacked/noble-server-cloudimg-arm64-vmlinuz-generic
curl -L -o initrd https://cloud-images.ubuntu.com/noble/current/unpacked/noble-server-cloudimg-arm64-initrd-generic
../../target/debug/bulwark disk create disk.qcow2 --backing noble.img --size 20G
```

Make a cloud-init seed (user `ubuntu`, password `bulwark`) as `seed.iso`
with `user-data` / `meta-data` (any NoCloud ISO tool works, e.g.
`hdiutil makehybrid -iso -joliet -default-volume-name cidata -o seed.iso seeddir`),
then `target/debug/bulwark run examples/ubuntu.toml`.

Clipboard and time sync need the agent in the guest:
`cargo build --release --target aarch64-unknown-linux-musl -p bulwark-agent`,
copy it in, run it at login (it uses `/dev/virtio-ports/org.bulwark.agent.0`).

## Ubuntu desktop (live ISO, UEFI)

```toml
name = "desktop"
cpus = 4
memory_mib = 6144
[boot]
type = "uefi"
vars = "desktop.state/efivars.fd"
[[disk]]
path = "ubuntu-24.04-desktop-arm64.iso"
readonly = true
cdrom = true
[display]
gpu = "virtio2d"
[[net]]
mode = "nat"
```

`target/debug/bulwark-gui desktop.toml`. The guest follows window resizes.

## Windows 11 ARM64

1. Get a Windows 11 ARM64 ISO (e.g. via UUP dump) and the virtio-win ISO.
2. Build an install "stick": a FAT32 image with an **MBR** partition table
   and a non-zero disk signature, holding the ISO contents, the ARM64
   virtio-win driver folders, `examples/windows/autounattend.xml`,
   `examples/windows/bulwark-setup.cmd` and `bulwark-agent.exe`.
   (An ISO doesn't work: Windows Setup has no driver for 2 KiB-sector media
   on NVMe. A GPT image gets its EFI partition mistaken for the system
   partition.)
3. Config:

```toml
name = "win11"
cpus = 4
memory_mib = 6144
[boot]
type = "uefi"
vars = "win11.state/efivars.fd"
[[disk]]
path = "win11.qcow2"        # bulwark disk create win11.qcow2 --size 64G
format = "qcow2"
bus = "nvme"
[[disk]]
path = "winusb.raw"
readonly = true
format = "raw"
bus = "nvme"
[[net]]
mode = "nat"
```

4. `bulwark-gui win11.toml`. Setup runs unattended (the unattend file wipes
   **disk 0**), reboots, and logs in as `bulwark` / `bulwark` (change it).
   First logon installs the virtio drivers and the agent.
5. **Remove the stick from the config afterwards.** If Windows ever fails to
   boot, the firmware falls back to the next boot entry: with the stick
   still attached, that's an unattended reinstall that wipes disk 0.
6. Add `[display] gpu = "virtio2d"` once the drivers are in. Windows then
   sees two monitors (firmware framebuffer + virtio-gpu); the agent keeps
   only the virtio-gpu one active, so windows open where you can see them.

The agent runs elevated from a logon task (the VirtIO serial port only
admits administrators), syncs the clipboard, follows window resizes, syncs
the clock and handles clean shutdown requests (`Ctrl+Alt+Shift+D`).

## 3D acceleration (Linux guests)

```bash
brew install meson ninja libepoxy molten-vk
scripts/build-virglrenderer.sh
```

Set `[display] gpu = "virtio3d"`. In the guest: `mesa-vulkan-drivers`
(Mesa ≥ 24), the user in the `render` group, and on macOS hosts a kernel
with **16 KiB pages** (HVF maps guest memory at 16 KiB granularity).
Ubuntu ships no such kernel; build one from Ubuntu's config with
`CONFIG_ARM64_16K_PAGES=y` (see `examples/ubuntu/kernel-16k.sh`).
`vulkaninfo --summary` should show `Virtio-GPU Venus (Apple M…)`.

OpenGL on top (Zink) and Windows guests are not covered: Windows has no
Venus driver.

## Snapshots, suspend

- `Ctrl-A s` / `Ctrl+Alt+Shift+S`: snapshot to `<state>/snapshots/snap-<time>`.
- `bulwark run vm.toml --restore <state>/snapshots/snap-…`: go back to it
  (the disks too).
- `Ctrl-A z` / `Ctrl+Alt+Shift+Z` / closing the GUI window: suspend; the
  next `run` resumes. `--fresh` ignores the suspend image.
- Not possible while 3D contexts are active.

## Disk sync

`sync = "fsync"` (default) survives a host crash. `"full"` also survives a
power cut but costs milliseconds per guest flush on macOS (Windows setup
flushes a lot). `"none"` ignores flushes: scratch disks only.

## Known limitations

- x86_64 hosts: not implemented.
- Bridged / host-only: needs `bulwark-netd` running as root; not tested
  with real vmnet/tap yet.
- No vTPM; Windows 11 is installed with its hardware checks bypassed.
- 3D snapshots, GL (virgl) path, Windows 3D: not supported.
