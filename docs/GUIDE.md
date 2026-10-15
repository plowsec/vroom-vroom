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
  vroom tools for network, clipboard and resizing: build the tools disk
  with the drivers on it (`scripts/make-windows-tools.sh tools.raw
  virtio-win.iso`; Windows can't read the ISO itself from an NVMe disk),
  attach it and run `vroom-setup.cmd` as administrator. Fusion puts
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

The cloud image has no driver for the hardware clock (`rtc-pl031`, in
`linux-modules-extra-$(uname -r)`), so it boots with a wrong date until
NTP or the vroom agent sets it. Install that package if the guest has no
network.

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
brew install meson ninja molten-vk
scripts/build-virglrenderer.sh    # virglrenderer, for Vulkan and OpenGL
scripts/build-angle.sh            # only for opengl_host = "angle" (about 10 minutes)
```

Set `[display] gpu = "virtio3d"`. The guest gets two drivers, and Mesa
picks the right one by itself:

- **OpenGL** (virgl): Mesa's virgl driver sends GL work to the host,
  where virglrenderer replays it on the Mac's GPU. Stock Ubuntu works as
  it is (any kernel, Mesa from the archive), in Wayland and X11 sessions.
- **Vulkan** (Venus): needs `mesa-vulkan-drivers` (Mesa ≥ 24), the user
  in the `render` group, and on macOS hosts a kernel with **16 KiB pages**
  (HVF maps guest memory at 16 KiB granularity). Ubuntu ships no such
  kernel; build one from Ubuntu's config with `CONFIG_ARM64_16K_PAGES=y`
  (see `examples/ubuntu/kernel-16k.sh`). `vulkaninfo --summary` should
  show `Virtio-GPU Venus (Apple M…)`. There is no swapchain
  (`VK_KHR_swapchain` is missing, with or without OpenGL): Vulkan apps
  can compute and render off screen, not show a window.

Two ways to run the guest's OpenGL, `[display] opengl_host`:

| | `apple` (default) | `angle` |
|---|---|---|
| Runs on | macOS's own OpenGL 4.1 | ANGLE (OpenGL ES 3.0) on Metal |
| The guest gets | OpenGL 4.1, core and compatibility | OpenGL 2.1, OpenGL ES 3.0 |
| `glxinfo -B` | `virgl (Apple M…)` | `virgl (ANGLE (Apple, ANGLE Metal Renderer: Apple M…))` |
| Checks | virglrenderer's | virglrenderer's, then ANGLE checks every call (built for WebGL) |

With `angle`, apps that need GL 3 won't start, and some that assume it
crash (glmark2 does, on its framebuffer scenes). Most desktop Linux apps
want GL 3.3 now, so `apple` is the default; see SECURITY.md for what
the extra ANGLE checks buy. Neither gets past GL 4.1 (no compute
shaders: that's GL 4.3, which macOS never had). `LIBGL_ALWAYS_SOFTWARE=1`
runs one app on llvmpipe instead, which has GL 4.5.

`[display] opengl = false` keeps Vulkan only (the guest's GL then runs on
llvmpipe). That keeps less host code in reach of the guest, and keeps
suspend working (see below).

Speed, Ubuntu 22.04 live desktop (X11), 1280×800, 4 vCPUs, M3 Max,
glmark2 2021.02 at its default 800×600 window, `opengl_host = "apple"`
against llvmpipe in the same guest:

| | llvmpipe (CPU) | `apple` |
|---|---|---|
| glmark2 score (full run) | 676 | 1059 |
| terrain | 13 fps | 388 to 448 fps |
| refract | 40 fps | 981 to 1071 fps |
| desktop (blur) | 159 fps | 892 to 1006 fps |

Small scenes stop near 1,100 fps either way: each frame makes a round
trip to the host, and the window shows a copy read back from the GPU.
Anything with real work is many times faster, and the guest's CPUs stay
free.

Why virgl and not OpenGL on top of Vulkan (Zink): Zink needs Vulkan's
`nullDescriptor` (robustness2), which MoltenVK can't offer on Metal.
Faking it on the host would mean rewriting descriptor sets in
virglrenderer's Venus decoder. virgl works with any guest kernel.

Snapshots and suspend: GPU state lives on the host GPU and can't be
saved, so snapshots and suspend are refused while 3D contexts exist.
With OpenGL on there always is one, even at a text console: Ubuntu's
udev opens a GL context at boot and keeps it. So shut the guest down
instead of suspending it. Closing the window then leaves the
VM paused with "suspend failed"; resume it and shut down from the guest,
or power it off. With `opengl = false` this only happens while a Vulkan
app runs. Windows guests are not covered: Windows has no virgl or Venus
driver.

The build: `build-angle.sh` fetches ANGLE at the commit Chrome's stable
release uses, with Google's depot_tools, about 3 GB while it builds
(removed afterwards), and leaves `libEGL.dylib` and `libGLESv2.dylib`
(6 MB) in `target/angle`. `build-virglrenderer.sh` builds libepoxy 1.5.10
(EGL on, patched to load ANGLE from its own directory, or Apple's OpenGL
when the VMM asks for it) into virglrenderer (3.4 MB), with our patches
from `third_party/virglrenderer/` (two of them make it work on macOS's
OpenGL 4.1). Both copy their results next to `target/*/vroom`, where the
VMM looks; ship the dylibs with the binary (ANGLE's two only for
`opengl_host = "angle"`).

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
host.

### Windows guests

Each share becomes a drive letter: the first gets Z:, the next Y:, and so
on. It takes three pieces, all set up by `vroom-setup.cmd` (run it as
administrator, again after adding your first share is fine):

- the viofs driver from virtio-win;
- [WinFsp](https://winfsp.dev), a file system library virtio-win's
  `virtiofs.exe` runs on. It isn't ours to ship (GPLv3), so setup takes
  `winfsp-2.1.25156.msi` from next to the script if it's there, or
  downloads that version from WinFsp's GitHub releases. Either way it
  checks the SHA-256 before installing. No internet in the guest: put the
  MSI on the tools disk (`WINFSP_MSI=path/to.msi
  scripts/make-windows-tools.sh tools.raw virtio-win.iso`);
- one service per share, `VroomShare_<tag>`, running
  `virtiofs.exe -t <tag> -m Z:`. The agent service checks at every boot:
  it adds services for new shares and deletes those of shares removed
  from the config. So both take effect after a reboot.

The drivers come from the tools disk: build it with the virtio-win ISO
(`scripts/make-windows-tools.sh tools.raw virtio-win.iso`); Windows can't
read the ISO itself from an NVMe disk.

A folder instead of a drive letter (the folder must not exist yet):

```bat
sc config VroomShare_host binPath= "\"C:\Program Files\Vroom\virtiofs.exe\" -t host -m C:\Shares\host"
sc stop VroomShare_host & sc start VroomShare_host
```

Setup never changes the service of a share that still exists, so your
`sc config` stays. A removed share's service is deleted at the next boot.

What to expect:

- Read-only shares refuse changes with "Access denied". Their files look
  read-only to Windows, so copies of them come out read-only, like files
  copied from a CD.
- Whether case matters is up to the host: on a Mac `README.txt` and
  `readme.txt` are one file, as Windows expects; on a Linux host, two.
- No symlinks, junctions or hard links can be made in a share. Symlinks
  already there show up but don't lead anywhere outside it.
- Paths are limited to 259 characters (virtiofs.exe's limit).
- Free space shown is the host disk's.
- `virtiofs.exe` never tells us when it is done with a file, so a share
  with more than about 260,000 files and folders opened makes us drop the
  ones it doesn't have open; it looks them up again by name.

## USB devices

```bash
brew install libusb          # the helper loads it at run time
vroom usb list               # what's plugged in, with its id
```

```toml
[[usb]]
id = "1209:000a"
```

In the app: the "USB devices" card, "Add device" lists what's plugged in.
The VM takes the device while it runs; unplugging and plugging it back
in works while the VM runs, and the guest sees a new device after a
suspend/resume or a snapshot revert (in-flight transfers can't be saved).

The device goes to the guest through a helper process (`vroom usb-host`,
one per device, in its own sandbox); the VM process itself never opens a
host device. See [SECURITY.md](SECURITY.md#usb-devices).

What works on macOS, as a normal user: devices no macOS driver holds.
That's vendor-specific devices: many dev boards, flashing and DFU modes,
debug probes, logic analyzers, SDR sticks, Android phones in adb or
fastboot mode. `vroom usb list` says when a macOS driver holds a device.
What doesn't:

- Keyboards, mice, game pads and FIDO security keys (HID), disks and
  card readers (mass storage), sound cards, webcams, USB Ethernet,
  iPhones, and the common USB serial chips (FTDI, CP210x, CH34x, PL2303,
  CDC-ACM): macOS has drivers for these and keeps them. Taking a device
  away from a macOS driver needs root, or Apple's
  `com.apple.vm.device-access` entitlement (Apple gives it to approved VM
  vendors only; vroom can't ship it). With either, the same code takes
  the device: libusb's "detach kernel driver" does it.
- Isochronous transfers (webcams, sound cards) aren't passed through
  yet: such a device shows up in the guest but doesn't stream.
- A device composed of several functions works for the ones macOS
  leaves alone; the guest sees the others but they fail.

Control, bulk and interrupt transfers work, at USB 1.1, 2.0 and 3.x speeds
(SuperSpeed devices show up on a USB 3 port). No USB 3 bulk streams (UAS
disks fall back to the older protocol).

On Linux hosts the helper uses usbfs: give your user access to the device
with a udev rule, e.g.
`SUBSYSTEM=="usb", ATTR{idVendor}=="1209", ATTR{idProduct}=="000a", MODE="0660", TAG+="uaccess"`.
Linux drivers bound to the device's interfaces are detached while the VM
has it.

Testing without hardware: `VROOM_USB_FAKE=acm` makes every helper serve
a pretend USB serial port that echoes what it's sent (CDC-ACM; Linux
guests see `/dev/ttyACM0` if they have the `cdc_acm` module, or use it
from userspace through `/dev/bus/usb`). `VROOM_USB_FAKE_UNPLUG=<secs>`
pulls it out once, for three seconds.

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
- Not possible while 3D contexts are active. With OpenGL on there always
  is one (see 3D acceleration).
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
- Windows guests: no 3D (no Venus or virgl driver). Shared folders need
  WinFsp (see above).
- 3D snapshots: not supported. Guest OpenGL stops at 2.1 / ES 3.0.
- NAT connections don't survive suspend/resume (the guest reconnects).
- USB passthrough: not yet tried with a real device (none was at hand);
  tested with the pretend device behind the real helper and sandbox. No
  isochronous transfers. On macOS only devices macOS has no driver for.
