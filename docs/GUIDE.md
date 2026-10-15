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
each logon (clipboard, window resizes, UI scale). The status line says when it
connects. Upgrading an older install: run the new `vroom-setup.cmd` as
administrator.

Until the agent runs, Windows may show two monitors (firmware framebuffer
and virtio-gpu). The window shows whichever one Windows last drew on.

## Retina screens

With `[display] hidpi = true` (the default; "Full resolution on Retina"
in the Display settings) a window on a Retina screen gives the guest its
real pixels, 2560x1600 for a window that looks 1280x800, and the guest
agent sets the guest's UI scale to 200% so things keep their size. The
picture is shown 1:1, without the blur of scaling it up. It can be
switched while the VM runs.

- Needs the current guest agent. Older agents, or a guest without one,
  get the old behavior: half the pixels, scaled up.
- Linux: GNOME (X11 and Wayland). Other desktops don't get the scale yet:
  set it in the guest yourself, or turn `hidpi` off.
- Windows: the scale changes right away, without logging off. The shell
  and most apps redraw sharp; a few older apps look blurry until they are
  restarted.
- Windows' display driver (viogpudo) has a 16 MiB framebuffer: modes up
  to about 4 million pixels (2560x1600). A bigger window gets the largest
  mode of the same shape that fits, scaled up a little.
- Turning `hidpi` off puts the guest's own scale back.

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

```toml
[audio]
output = true    # guest sound through the Mac's speakers
input = true     # the Mac's microphone, for video calls (off by default)
```

The guest gets a sound card with 44.1/48 kHz, 16-bit, mono/stereo:

- Windows: an Intel HD Audio card. Windows' own driver picks it up
  (Device Manager: "High Definition Audio Controller", then "Speakers"
  and "Microphone (High Definition Audio Device)"). Nothing to install.
- Linux: virtio-snd (`virtio_snd` module). Ubuntu's cloud images keep
  it in `linux-modules-extra-$(uname -r)`; desktop installs have it.
  `device = "hda"` gives Linux the HD Audio card too (`snd_hda_intel`).

`vroom check` names the microphone as a risk when it's on.

### Microphone

The microphone is off unless `input = true` (app: Settings, Sound,
Microphone). When on, sound from it reaches the guest only while all of
these hold:

- the VM's window is open (closing it suspends the VM anyway; `vroom run`
  without a window never gets the microphone),
- the VM runs (pausing it stops the microphone),
- the guest records (an app in it opened the input).

The window shows "Microphone in use" in its toolbar, and a small "Mic"
pill at the top while the toolbar is hidden. macOS shows its own orange
dot in the menu bar too. Focus doesn't matter: a call keeps going while
you work in another window, the way a native app would.

The first time a guest records, macOS asks for the microphone
permission. It asks for the app that started vroom, not vroom itself:
Terminal (or iTerm, VS Code...) for `vroom run` and for `vroom-gui`
started from a terminal. Saying yes lets everything started from that
app use the microphone. If you said no, or never saw the question, the
guest records silence and the window says so after two seconds; turn
the app on in System Settings, Privacy & Security, Microphone (or reset
the question with `tccutil reset Microphone com.apple.Terminal`).
Sound input in the guest follows the Mac's default input device (System
Settings, Sound, Input).

On Linux hosts the helper plays and records through ALSA (`libasound`,
loaded at run time, "default" device): use a normal (glibc) build there;
a static musl build can't load it and stays silent.

Testing without speakers or a microphone: `VROOM_AUDIO_WAV_DIR=dir vroom
run ...` writes what the guest plays to `dir/audio.wav` and, with
`input = true`, feeds `dir/mic.wav` (16-bit PCM, looped) to the guest
instead of the microphone. The window gate still applies.

## Snapshots, suspend

- `Ctrl-A s` / `Ctrl+Alt+Shift+S`: snapshot to `<state>/snapshots/snap-<time>`.
- `vroom run vm.toml --restore <state>/snapshots/snap-…`: go back to it
  (the disks too).
- `Ctrl-A z` / `Ctrl+Alt+Shift+Z` / closing the GUI window: suspend; the
  next `run` resumes. `--fresh` ignores the suspend image.
- Not possible while 3D contexts are active.
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
- Windows guests: no shared folders without WinFsp (untested), no 3D (no
  Venus driver).
- 3D snapshots, OpenGL on Venus (Zink, see above): not supported.
- NAT connections don't survive suspend/resume (the guest reconnects).
