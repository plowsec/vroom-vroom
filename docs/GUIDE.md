# Guide

## Prerequisites (macOS, Apple Silicon)

```bash
brew install qemu            # only for the EDK2 firmware files and qemu-img
scripts/build.sh
```

`qemu` is not used to run anything: vroom only reads
`/opt/homebrew/share/qemu/edk2-aarch64-code.fd` (UEFI) from it.

## The app

`vroom-gui` with no arguments opens the app.

![New VM](img/new-vm.png)

- New VMs go in `~/Library/Application Support/vroom/VMs/<name>/` (Linux:
  `~/.local/share/vroom/VMs/`). "Add existing" takes any VM file; it stays
  where it is.
- Settings are saved to the VM file right away. Comments and layout in the
  file are kept. Hand edits show up in the app within a couple of seconds.
- A running or suspended VM's hardware is locked. Clipboard rules apply
  right away.
- Hover a setting to see its key in the file. Every option is in
  [CONFIG.md](CONFIG.md); the rare ones are under "Advanced" in each card.
- `⌘K` (Ctrl+K on Linux): start, suspend, snapshot, jump to a setting.
- Each VM's last run log: `⋯` → Show log.
- The app only reads pictures it saved itself (VM screens, snapshot
  thumbnails, under `.../vroom/thumbs/`), never files from a state dir:
  the sandboxed VMM can write there.
- `VROOM_HOME=dir` uses another data dir. `VROOM_SHOT=out.png` saves a
  picture of the window and quits (that's how the README pictures are made).

## Moving a VM from VMware

ARM64 VMs from VMware Fusion (or Workstation on ARM) come over with their
disks. VMware's copy is only read, never changed.

```bash
vroom import "~/Virtual Machines.localized/Ubuntu.vmwarevm" --dry-run   # what would happen
vroom import "~/Virtual Machines.localized/Ubuntu.vmwarevm" --dir ~/VMs
```

In the app: "Import from VMware" in the `⋯` menu or `⌘K`, or drop the
`.vmwarevm` on the window.

What happens:

- Disks are copied to qcow2 (only the used parts). Split disks, snapshot
  chains and linked clones are read through; the current state is what
  you get. VMware snapshots stay in VMware.
- CPUs, memory, shared folders, sound and the TPM setting carry over.
  Every network adapter becomes NAT; switch it in the settings if you need
  bridged or host-only.
- ISOs stay where they are and are attached as CDs.

Before you import:

- Shut the VM down in VMware and quit VMware. A suspended VM can be
  imported (`--yes`), but what was in its memory is lost.
- x86 VMs can't run here (ARM64 guests only).
- VMs with encrypted disks: remove the encryption in VMware first. A
  Windows 11 VM with only its TPM encrypted (Fusion's default) is fine,
  but the TPM's contents can't come along: if BitLocker is on, have the
  recovery key.

After the import:

- Linux guests boot as they are. Remove `open-vm-tools` if you like.
- Windows guests boot from NVMe, but need the virtio-win drivers and the
  vroom tools for network, clipboard and resizing: attach `tools.raw` and
  the virtio-win ISO, run `vroom-setup.cmd` as administrator. Fusion puts
  Windows on NVMe; a Windows installed on a SATA disk may not boot from
  NVMe (INACCESSIBLE_BOOT_DEVICE). That case isn't handled yet.
- Until the tools are in, Windows' clock is off by your time zone (it
  reads the hardware clock as local time; `vroom-setup.cmd` fixes that).
- Windows may ask to reactivate (it sees new hardware).
  `--keep-hardware-ids` keeps VMware's BIOS UUID and SMBIOS names, which
  helps, but then the guest can tell it came from VMware.

## Ubuntu 24.04 (cloud image, direct kernel boot)

```bash
mkdir -p images/ubuntu && cd images/ubuntu
curl -LO https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img
mv noble-server-cloudimg-arm64.img noble.img
# kernel + initrd from the same release
curl -L -o vmlinuz https://cloud-images.ubuntu.com/noble/current/unpacked/noble-server-cloudimg-arm64-vmlinuz-generic
curl -L -o initrd https://cloud-images.ubuntu.com/noble/current/unpacked/noble-server-cloudimg-arm64-initrd-generic
../../target/debug/vroom disk create disk.qcow2 --backing noble.img --size 20G
```

Make a cloud-init seed (user `ubuntu`, password `vroom`) as `seed.iso`
with `user-data` / `meta-data` (any NoCloud ISO tool works, e.g.
`hdiutil makehybrid -iso -joliet -default-volume-name cidata -o seed.iso seeddir`),
then `target/debug/vroom run examples/ubuntu.toml`.

Clipboard, time sync and clean shutdown need the agent in the guest:
`cargo build --release --target aarch64-unknown-linux-musl -p vroom-agent`,
copy it and `examples/linux/*` in, run `sudo ./install-agent.sh`. That
installs a systemd service (root, hardened unit: the host channel, clock,
shutdown) and a desktop autostart entry for the unprivileged session
helper (clipboard); the service only serves the user on screen.

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

`target/debug/vroom-gui desktop.toml`. The guest follows window resizes.

## Windows 11 ARM64

1. Get a Windows 11 ARM64 ISO (e.g. via UUP dump) and the virtio-win ISO.
2. Build the install "stick":
   `scripts/make-windows-stick.sh win11-arm64.iso virtio-win.iso winusb.raw`.
   It's a FAT32 image with an **MBR** partition table and a non-zero disk
   signature, holding the ISO contents, the ARM64 virtio-win driver
   folders, `examples/windows/autounattend.xml`,
   `examples/windows/vroom-setup.cmd` and `vroom-agent.exe` (an
   `install.wim` over 4 GiB is split; needs `brew install wimlib`).
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
path = "win11.qcow2"        # vroom disk create win11.qcow2 --size 64G
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

4. `vroom-gui win11.toml`. Setup runs unattended (the unattend file wipes
   **disk 0**), reboots, and logs in as `vroom` / `vroom` (change it).
   First logon installs the virtio drivers and the agent.
5. **Remove the stick from the config afterwards.** If Windows ever fails to
   boot, the firmware falls back to the next boot entry: with the stick
   still attached, that's an unattended reinstall that wipes disk 0.
6. Add `[display] gpu = "virtio2d"` once the drivers are in. Windows then
   sees two monitors (firmware framebuffer + virtio-gpu); the agent keeps
   only the virtio-gpu one active, so windows open where you can see them.

The agent is a Windows service (`VroomAgent`, clock sync and clean
shutdown via `Ctrl+Alt+Shift+D`) plus an unprivileged helper started at
each logon (clipboard, window resizes). The status line says when it
connects. Upgrading an older install: run the new `vroom-setup.cmd` as
administrator.

Until the agent runs, Windows may show two monitors (firmware framebuffer
and virtio-gpu). The window shows whichever one Windows last drew on.

## 3D acceleration (Linux guests)

```bash
brew install meson ninja molten-vk
scripts/build-angle.sh            # OpenGL: ANGLE on Metal (about 10 minutes)
scripts/build-virglrenderer.sh    # virglrenderer, for Vulkan and OpenGL
```

Set `[display] gpu = "virtio3d"`. The guest gets two drivers, and Mesa
picks the right one by itself:

- **OpenGL** (virgl): Mesa's virgl driver sends GL work to the host,
  where virglrenderer replays it on OpenGL ES, which ANGLE turns into
  Metal. Stock Ubuntu works as it is (any kernel, Mesa from the archive).
  `glxinfo -B` shows `virgl (ANGLE (Apple, ANGLE Metal Renderer: Apple M…))`
  and GNOME draws with the Mac's GPU.
- **Vulkan** (Venus): needs `mesa-vulkan-drivers` (Mesa ≥ 24), the user
  in the `render` group, and on macOS hosts a kernel with **16 KiB pages**
  (HVF maps guest memory at 16 KiB granularity). Ubuntu ships no such
  kernel; build one from Ubuntu's config with `CONFIG_ARM64_16K_PAGES=y`
  (see `examples/ubuntu/kernel-16k.sh`). `vulkaninfo --summary` should
  show `Virtio-GPU Venus (Apple M…)`. There is no swapchain
  (`VK_KHR_swapchain` is missing, with or without OpenGL): Vulkan apps
  can compute and render off screen, not show a window.

What OpenGL apps get: OpenGL 2.1 and OpenGL ES 3.0. ANGLE's Metal
backend stops at ES 3.0, and virgl can't offer more than its host has.
That covers desktops and apps written for GL 2 or GLES; apps that need
GL 3.3 core (Blender, recent games) won't start. `LIBGL_ALWAYS_SOFTWARE=1`
runs one app on llvmpipe instead, which has GL 4.5.

`[display] opengl = false` keeps Vulkan only (the guest's GL then runs on
llvmpipe). That keeps less host code in reach of the guest, and keeps
suspend working (see below).

Speed, Ubuntu 24.04 GNOME (Wayland), 1280×800, 6 vCPUs, M3 Max,
glmark2 2023.01 at its default 800×600 window:

| | `virtio2d` (llvmpipe) | `virtio3d` (virgl on Metal) |
|---|---|---|
| glmark2 score | 463, 478 | 806 to 925 (four runs) |
| glmark2 terrain | 16 to 22 fps | 520 to 530 fps |
| glmark2 refract | 42 to 47 fps | 770 to 850 fps |
| glxgears (vsync off) | about 2,700 fps | 400 to 1,600 fps |
| guest CPU busy during terrain | 59% | 3% |

Numbers move with the host's load (these were taken next to other VMs).
glxgears is the one test llvmpipe wins: its frames are tiny, and with
virgl each one makes a round trip to the host GPU, and the window shows
a copy of the guest's screen read back from it. Anything with real work
is many times faster, and the guest's CPUs stay free.

Why virgl and not OpenGL on top of Vulkan (Zink): Zink needs Vulkan's
`nullDescriptor` (robustness2), which MoltenVK can't offer on Metal.
Faking it on the host would mean rewriting descriptor sets in
virglrenderer's Venus decoder. virgl on ANGLE is the route UTM takes on
macOS, and it works with any guest kernel.

Snapshots and suspend: GPU state lives on the host GPU and can't be
saved, so snapshots and suspend are refused while 3D contexts exist.
With OpenGL on, the GNOME desktop itself is one, so in practice: shut the
guest down instead of suspending it. Closing the window then leaves the
VM paused with "suspend failed"; resume it and shut down from the guest,
or power it off. With `opengl = false` this only happens while a Vulkan
app runs. Windows guests are not covered: Windows has no virgl or Venus
driver.

The build: `build-angle.sh` fetches ANGLE at the commit Chrome's stable
release uses, with Google's depot_tools, about 3 GB while it builds
(removed afterwards), and leaves `libEGL.dylib` and `libGLESv2.dylib`
(6 MB) in `target/angle`. `build-virglrenderer.sh` builds libepoxy 1.5.10
(EGL on, patched to load ANGLE from its own directory) into
virglrenderer (3.4 MB). Both copy their results next to `target/*/vroom`,
where the VMM looks; ship the three dylibs with the binary.

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
On Linux hosts the helper plays through ALSA (`libasound`, loaded at run
time): use a normal (glibc) build there; a static musl build can't load it
and stays silent.

## Snapshots, suspend

- `Ctrl-A s` / `Ctrl+Alt+Shift+S`: snapshot to `<state>/snapshots/snap-<time>`.
- `vroom run vm.toml --restore <state>/snapshots/snap-…`: go back to it
  (the disks too).
- `Ctrl-A z` / `Ctrl+Alt+Shift+Z` / closing the GUI window: suspend; the
  next `run` resumes. `--fresh` ignores the suspend image.
- Not possible while 3D contexts are active. With OpenGL on, the GNOME
  desktop is one (see 3D acceleration).
- `vroom snapshot list vm.toml`, `vroom snapshot delete vm.toml snap-…`:
  deleting a snapshot also removes the disk layers only it used, and merges
  layers that became pass-through, so disk chains stay short.
  `vroom snapshot prune vm.toml` does the cleanup alone (e.g. after
  reverting, the abandoned layers). These need the VM stopped: a state dir
  is locked by the VMM using it.

## Disk space

Disk layers grow in the state dir. When less than 2 GiB is left on that
file system, the VM is paused with a message; free space and resume
(`Ctrl-A p`, or the GUI shortcut).

## Disk size

Guests that TRIM (Ubuntu's cloud images mount with `discard`; Windows
trims on its own) give space back: freed clusters become holes in the
image and are reused. A qcow2 file can still keep a large apparent size
(its real usage, `du`, is what counts); `vroom disk compact vm.qcow2`
(VM off) rewrites it to its real size, keeping its backing file.

## Disk sync

`sync = "fsync"` (default) survives a host crash. `"full"` also survives a
power cut but costs milliseconds per guest flush on macOS (Windows setup
flushes a lot). `"none"` ignores flushes: scratch disks only.

## Known limitations

- x86_64 hosts: everything builds, but there is no x86 board: VMs can't
  start there yet.
- Bridged / host-only: needs `vroom-netd` running as root; not tested
  with real vmnet/tap yet.
- Windows 11 is installed with its hardware checks bypassed (no Secure
  Boot); `tpm = true` (needs `brew install swtpm`) gives it a real TPM 2.0,
  which Windows sees and uses.
- Windows guests: no shared folders without WinFsp (untested), no sound
  (no virtio-snd driver), no 3D (no Venus or virgl driver).
- 3D snapshots: not supported. Guest OpenGL stops at 2.1 / ES 3.0.
- NAT connections don't survive suspend/resume (the guest reconnects).
