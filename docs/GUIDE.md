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

The agent is a Windows service (`BulwarkAgent`, clock sync and clean
shutdown via `Ctrl+Alt+Shift+D`) plus an unprivileged helper started at
each logon (clipboard, window resizes). The status line says when it
connects. Upgrading an older install: run the new `bulwark-setup.cmd` as
administrator.

Until the agent runs, Windows may show two monitors (firmware framebuffer
and virtio-gpu). The window shows whichever one Windows last drew on.

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

OpenGL on top of it (Mesa's Zink) doesn't work yet: Zink needs Vulkan's
`nullDescriptor` (robustness2), which MoltenVK can't offer on Metal, so GL
apps fall back to llvmpipe. Windows guests are not covered: Windows has no
Venus driver.

## Shared folders

```toml
[[share]]
path = "/Users/me/shared"
tag = "host"
readonly = false     # default true
```

Linux guest: `sudo mount -t virtiofs host /mnt` (or in `/etc/fstab`:
`host /mnt virtiofs defaults 0 0`). Files show up owned by whoever looks
at them; the guest can't create symlinks, hard links or device nodes in
the share, and symlinks already there are shown but never followed on the
host. Windows guests need virtio-win's virtiofs service and WinFsp (not
tested yet).

## Sound

`[audio] output = true` gives Linux guests a sound card (virtio-snd; the
`virtio_snd` module, in stock Ubuntu kernels). 44.1/48 kHz, 16-bit,
mono/stereo. There is no input: the guest can't reach the microphone.
Windows has no virtio-snd driver, so Windows guests stay silent.

## Snapshots, suspend

- `Ctrl-A s` / `Ctrl+Alt+Shift+S`: snapshot to `<state>/snapshots/snap-<time>`.
- `bulwark run vm.toml --restore <state>/snapshots/snap-…`: go back to it
  (the disks too).
- `Ctrl-A z` / `Ctrl+Alt+Shift+Z` / closing the GUI window: suspend; the
  next `run` resumes. `--fresh` ignores the suspend image.
- Not possible while 3D contexts are active.
- `bulwark snapshot list vm.toml`, `bulwark snapshot delete vm.toml snap-…`:
  deleting a snapshot also removes the disk layers only it used, and merges
  layers that became pass-through, so disk chains stay short.
  `bulwark snapshot prune vm.toml` does the cleanup alone (e.g. after
  reverting, the abandoned layers). These need the VM stopped: a state dir
  is locked by the VMM using it.

## Disk space

Disk layers grow in the state dir. When less than 2 GiB is left on that
file system, the VM is paused with a message; free space and resume
(`Ctrl-A p`, or the GUI shortcut).

## Disk sync

`sync = "fsync"` (default) survives a host crash. `"full"` also survives a
power cut but costs milliseconds per guest flush on macOS (Windows setup
flushes a lot). `"none"` ignores flushes: scratch disks only.

## Known limitations

- x86_64 hosts: not implemented.
- Bridged / host-only: needs `bulwark-netd` running as root; not tested
  with real vmnet/tap yet.
- Windows 11 is installed with its hardware checks bypassed; add
  `tpm = true` (needs `brew install swtpm`) for a real TPM 2.0.
- 3D snapshots, GL (virgl) path, Windows 3D: not supported.
