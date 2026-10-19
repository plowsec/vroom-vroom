# Guide

## Prerequisites (macOS, Apple Silicon)

```bash
xcodebuild -downloadComponent MetalToolchain   # Xcode 26: a separate download
brew install qemu meson ninja bison pkg-config llvm llvm@15 molten-vk lld uv
cargo install cargo-xwin
scripts/app.sh --install     # vroom.app, in /Applications
```

Xcode itself (not just the command line tools), and rustup. qemu: only its
UEFI firmware and qemu-img. The rest builds 3D and the guest tools.

ANGLE compiles Metal shaders, and Xcode 26 no longer installs the `metal`
compiler with itself -- hence the download above. `xcrun metal --version`
says whether you have it.

The Mesa and virglrenderer code generators run on a pinned Python that uv
fetches into `target/pyenv` (`scripts/pyenv.sh`), not on whatever `python3`
is on PATH -- a Homebrew python can carry a `pyexpat` built against a newer
libexpat than the `/usr/lib` one it loads, and then `python3 -m venv` dies
in `ensurepip`.

`lld` and `llvm` have to be the same version: Homebrew's `lld` loads
Homebrew's `libLLVM`, so an `llvm` upgrade on its own leaves `lld-link`
aborting, and cargo-xwin cannot link the Windows agent. `brew upgrade lld`.

`scripts/build.sh [--release]` builds just `vroom` and `vroom-gui` into
`target/debug` (or `target/release`).

## The Mac app

`scripts/app.sh` builds `target/dist/vroom.app`; `--install` copies it to
`/Applications` (refused while VMs run from there). Everything is in it:
3D (ANGLE, DXMT, virglrenderer), the guest tools with the Windows 3D
drivers, virtio-win drivers and WinFsp (pinned downloads,
`scripts/fetch.sh`, in `target/downloads/`). Long builds rerun only when
their inputs change; `--no-build` packages what `target/` has.

- Open a VM file with vroom (Open With, or drop it on the Dock icon) to
  start it. Drop an ISO: New VM with that installer. Drop a `.vmwarevm`:
  import.
- Logs: `~/Library/Application Support/vroom/logs/` (`vroom-gui.log`, and
  each VM's last run: `⋯` → Show log).
- `Contents/MacOS/vroom` is the VMM with all its commands.
- Signed ad hoc: fine on the Mac that built it, blocked on others until
  allowed in Privacy & Security. Each rebuild may make macOS ask for
  camera and microphone again.

## The app

`vroom-gui` with no arguments opens the library.

![New VM](img/new-vm.png)

- New VMs: `~/Library/Application Support/vroom/VMs/<name>/` (Linux:
  `~/.local/share/vroom/VMs/`). "Add existing" leaves the file where it is.
- Settings save to the VM file at once, keeping comments. Hand edits show
  up within seconds.
- Running or suspended: hardware is locked (hover the lock). Network, USB
  and clipboard rules change live.
- Hover anything for what it does and its key in the file. All options:
  [CONFIG.md](CONFIG.md).
- `⌘K` (Ctrl+K on Linux): commands and settings by name.
- `VROOM_HOME=dir`: another data dir. `VROOM_SHOT=out.png`: save a picture
  of the window and quit.

### The VM window

The top bar: name and state, pause, snapshot, suspend, power menu,
Ctrl+Alt+Del (Windows); then CPU use, 3D, network, USB, guest tools,
Coherence, settings, full screen, VM list. In full screen the bar slides
in from the top edge. With the agent, the guest takes the window's size.

Pointer: the guest's own (hardware cursor on Linux, drawn by Windows
itself on Windows guests).

### Drag and drop

Needs the guest tools.

- Mac → guest: drop files or folders on the window. Windows and X11: a
  real drop wherever you let go. Wayland: files land on the Desktop.
- Guest → Mac: drag out of Explorer, the Windows desktop, or GNOME Files
  (X11) into Finder. Copied on drop; names that are taken get a number.
  Not on Wayland, not from inside zips.
- Names Windows can't take get `_`. Links inside folders are skipped.

### Coherence

Each guest app as its own Mac window (VMware's Unity). Windows guests, or
Linux in an X11 session. Needs the guest tools.

- On: the bar's window button or `Ctrl+Alt+Shift+C`.
- A bottom bar replaces the taskbar: Start, an app launcher field (type
  part of a name, Enter), one button per window, Leave Coherence. Drag the
  bar to another Mac screen to move the guest's screen there.
- `Cmd+W` asks the guest app to close.
- Linux: X11 only (Ubuntu: "Ubuntu on Xorg" at login), an EWMH window
  manager (GNOME, KDE, Xfce, Cinnamon, MATE), the session helper running.
  GNOME's top bar hides via the tools' Shell extension (next login).
- Tiling tools (Amethyst, yabai): tell them to float `vroom`.

### When the app crashes, or a VM stops answering

Each VM is its own process.

- App crash: the VM keeps running. Start vroom again and its window comes
  back. After 5 minutes without the app, the VM suspends itself.
- "Isn't answering": Force power off (asks, then kills after 3 s).

## Moving a VM from VMware

ARM64 VMs from VMware Fusion (or Workstation on ARM). VMware's files are
only read.

```bash
vroom import "~/Virtual Machines.localized/Win11.vmwarevm" --dry-run
vroom import "~/Virtual Machines.localized/Win11.vmwarevm" --dir ~/VMs
```

App: "Import from VMware" in `⋯` or `⌘K`, or drop the `.vmwarevm`.

- Disks → qcow2 (used parts only); snapshot chains and linked clones are
  flattened to the current state.
- CPUs, memory, shared folders, sound, TPM setting carry over. Every NIC
  becomes NAT. ISOs stay where they are, attached as CDs.
- Before: shut down in VMware and quit it (a suspended VM imports with
  `--yes`, memory lost). No x86 VMs. Remove disk encryption first; TPM
  contents don't come along, so have the BitLocker recovery key.

Linux guests boot as they are; remove `open-vm-tools` if you like.

### Windows guests from VMware

Windows boots (from NVMe) but has no virtio drivers yet: no network, basic
display, clock off by your time zone. To fix:

1. `scripts/app.sh --install` (the app carries the tools with drivers).
2. Start the VM in the app, log in. After 90 s the guest tools button
   offers the tools as a USB stick, `VRTOOLS`. (Or attach
   `target/guest-tools/vroom-tools.img` as a disk: `readonly = true`,
   `format = "raw"`, `bus = "nvme"`.)
3. In Windows, run `vroom-setup.cmd` from that drive (it asks for admin).
   It installs the virtio drivers, the agent, WinFsp for shared folders,
   and fixes the clock.
4. Restart Windows. The window's status says when the agent connects.
5. Remove the tools disk from the VM file if you attached one. Uninstall
   VMware Tools in the guest.

- Installed on SATA in VMware: may not boot from NVMe
  (INACCESSIBLE_BOOT_DEVICE). Not handled yet.
- Windows may ask to reactivate. `--keep-hardware-ids` keeps VMware's
  UUID and SMBIOS names (the guest can then tell it was VMware).

## Guest tools

The vroom agent: clipboard, resize with the window, UI scale, time sync,
clean shutdown, drag and drop, Coherence, USB disks' files. Ready-made VMs
have it. Built by `scripts/make-guest-tools.sh` (`app.sh` runs it);
`vroom guest-tools info` shows the app's version.

The VM window's box icon shows when there's something to do:

- **Newer tools here** (dot): click; the agent installs them itself, no
  restart.
- **No agent after 90 s, or too old to update**: click; the tools go in
  as a USB stick, `VRTOOLS`, removed once the agent says hello.
  - Linux: `sudo mount -o ro LABEL=VRTOOLS /mnt && sudo sh /mnt/linux/install-agent.sh`
    (also works on a live ISO).
  - Windows: run `vroom-setup` from the VRTOOLS drive.
  - No USB (`[boot] type = "kernel"`): add `vroom-tools.img` as a disk.

Linux: a root systemd service plus an unprivileged session helper
(clipboard, through X11, also on Wayland via Xwayland). Windows: the
`VroomAgent` service plus a per-logon helper.

Commands in a running guest, no guest network needed:
`vroom guest exec <vm.toml | name> [-p] [-t secs] -- ipconfig /all`
(cmd.exe as SYSTEM, `-p` PowerShell; Linux: sh as root). Exits with the
command's code. Files: `vroom guest push <vm> <file> <guest path>`,
`vroom guest pull <vm> <guest path> <file or dir>` (size and SHA-256
checked). vroom-lab: `guest.exec`, `guest.upload`, `guest.download`.
Newer tools without a window:
`vroom guest update-tools <vm>` (vroom-lab: `guest.update_tools`).

## Ready-made VMs

New VM makes a VM that comes up installed, with the tools, your user and
optionally your SSH key (any `~/.ssh/*.pub`, with a port forward):

- **Ubuntu 24.04**: leave the installer empty (downloads the cloud image,
  checked, cached in `~/Library/Caches/vroom`). Ready in a minute;
  "Desktop" adds about 10.
- **Windows 11 ARM64**: pick the ISO (from microsoft.com). About 20
  minutes, unattended. Afterwards remove the install disk from the VM file
  and delete `install.img`. ISOs with a 4 GB+ image need
  `brew install wimlib`.

```bash
VROOM_NEW_PASSWORD=... vroom new web --os ubuntu --ready --ssh-key ~/.ssh/id_ed25519.pub [--desktop] [--image noble.img]
VROOM_NEW_PASSWORD=... vroom new win --os windows --ready --iso Win11_ARM64.iso --ssh-key ~/.ssh/id_ed25519.pub
```

`--dir` for where, `--user` for another user. Windows' answer file holds
the password in clear in `install.img`: delete it after.

## Ubuntu 24.04 (cloud image, direct kernel boot)

```bash
mkdir -p images/ubuntu && cd images/ubuntu
curl -L -o noble.img https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img
curl -L -o vmlinuz https://cloud-images.ubuntu.com/noble/current/unpacked/noble-server-cloudimg-arm64-vmlinuz-generic
curl -L -o initrd https://cloud-images.ubuntu.com/noble/current/unpacked/noble-server-cloudimg-arm64-initrd-generic
../../target/debug/vroom disk create disk.qcow2 --backing noble.img --size 20G
hdiutil makehybrid -iso -joliet -default-volume-name cidata -o seed.iso seeddir   # user-data, meta-data
cd ../.. && target/debug/vroom run examples/ubuntu.toml
```

- Guest tools: copy `target/guest-tools/linux/` in, `sudo sh install-agent.sh`.
- No RTC driver in the cloud image: wrong date until NTP or the agent.
  `linux-modules-extra-$(uname -r)` has it.

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

`vroom-gui desktop.toml`.

Ubuntu 22.04: repeated `tracker-extract-3` crash dialogs are a tracker bug
(harmless): `systemctl --user mask tracker-extract-3`.

## Windows 11 ARM64

The New VM sheet does all this ([Ready-made VMs](#ready-made-vms)). By hand:

1. Get a Windows 11 ARM64 ISO.
2. `scripts/make-windows-stick.sh win11-arm64.iso winusb.raw`
   (FAT32/MBR install stick with drivers, unattend file, setup script and
   agent; an ISO can't boot Setup on NVMe).
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

4. `vroom-gui win11.toml`. Setup wipes **disk 0**, logs in as
   `vroom`/`vroom` (change it), and installs drivers and agent.
5. **Remove the stick from the config afterwards**, or a failed boot falls
   through to an unattended reinstall.
6. Add `[display] gpu = "virtio2d"`.

Agent upgrades: the guest tools button ([Guest tools](#guest-tools)), or
re-run `vroom-setup.cmd`.

## Several monitors

```toml
[display]
gpu = "virtio3d"     # or virtio2d
monitors = 2         # up to 4
```

One window per monitor; the agent lines them up. Full screen spreads them
over the Mac's screens. Closing a window turns that monitor off ("Show all
displays" brings it back).

- Windows: one GPU per monitor; only the first does Direct3D. Needs both
  viogpudo and viogpu3d installed. Before login only the first monitor
  has a pointer.
- One UI scale for all. Coherence uses the first monitor only.

## Retina screens

`[display] hidpi = true` (default): the guest gets real pixels and a 200%
UI scale. Switchable live. Needs the current agent; GNOME and Windows
only. Windows' 2D driver tops out around 2560x1600 per monitor.

## 3D acceleration (Linux guests)

`scripts/app.sh` builds it (alone: `scripts/build-angle.sh`, then
`scripts/build-virglrenderer.sh`). `[display] gpu = "virtio3d"`. Stock Ubuntu works.

- **OpenGL** (virgl): up to 4.1 on macOS's OpenGL.
- **Vulkan** (Venus): 1.3. Needs `mesa-vulkan-drivers` (Mesa ≥ 24) and the
  user in `render`. Check: `vulkaninfo --summary` → `Virtio-GPU Venus`.
  Mesa 25.2 crashes on the Asahi driver: `install-agent.sh` disables it,
  or set `VK_LOADER_DRIVERS_DISABLE='*asahi*'`.

| `opengl_host` | `apple` (default) | `angle` |
|---|---|---|
| Guest gets | OpenGL 4.1 | OpenGL 2.1, ES 3.0 |
| Notes | | stricter checks; GL 3+ apps fail, Chrome has no GPU |

- `opengl = false`: Vulkan only (smaller attack surface).
- More than 4.1: run the app on Zink (below), or
  `LIBGL_ALWAYS_SOFTWARE=1`.
- Snapshots and suspend work with GL and Vulkan apps running. Not saved:
  multisampled buffers, query results; OpenGL blobs and leftover Vulkan
  window memory block the save. `VKR_SNAPSHOT_LOST=1` drops Vulkan state
  on save, to test.
- `VROOM_NO_SURFACES=1`: turn off the zero-copy display path.
- 3D runs in `vroom gpu-helper`, plus one `vroom gpu-worker` per Vulkan or
  Direct3D app ([SECURITY.md](SECURITY.md#the-gpu-process)). A crash
  costs 3D (or that app), not the VM. Windows desktop frozen after one:
  Win+Ctrl+Shift+B.

Numbers: [PERFORMANCE.md](PERFORMANCE.md).

### Chrome and Chromium

GPU-accelerated out of the box (`chrome://gpu`: "Hardware accelerated")
with `opengl_host = "apple"`. Not on `angle`.

### OpenGL 4.4 for one app (Zink)

```bash
MESA_LOADER_DRIVER_OVERRIDE=zink glxinfo -B
```

OpenGL 4.4 and ES 3.1 on Venus. Xorg: also `LIBGL_KOPPER_DRI2=1`. A bit
slower than virgl for plain drawing: use it for apps that need compute
shaders. Transform feedback works only for `glDrawArrays`; packed 10-bit
vertex formats read wrong.

## 3D in Windows guests (Direct3D 11)

`scripts/app.sh` builds it (DXMT, Direct3D 11 on Metal, and the guest
side).

In the guest, install `viogpu3d-arm64x-signed.zip` from
[osy/kvm-guest-drivers-windows](https://github.com/osy/kvm-guest-drivers-windows/releases),
as admin:

```bat
pnputil /add-driver viogpu3d.inf /install
```

Shut down, `[display] gpu = "virtio3d"`, start. Device Manager: "Red Hat
VirtIO GPU 3D controller", feature level 11_1.

- Resizing blinks the screen (the driver needs a monitor replug).
- No Direct3D 12.
- Suspend and snapshots work (needs the current tools): the agent turns
  the 3D display off for the save. Apps keep running; 3D apps lose their
  GPU state. If the screen stays black after a resume, suspend and resume
  again.

## OpenGL and Vulkan in Windows guests

In the guest tools (`scripts/app.sh`). `vroom-setup.cmd` installs them; apps get Vulkan 1.3 (Venus) and OpenGL
4.4 (Zink; virgl 4.1 fallback). ARM64 and x64 apps; 32-bit apps don't.

- `GALLIUM_DRIVER=virgl`: the virgl driver for one app.
- On suspend/snapshot these apps lose their GPU (Vulkan: device lost;
  OpenGL: blank window until restarted).
- Logs: `VROOM_GL_LOG=C:\path\gl.log`; `MESA_LOG_LEVEL=debug`,
  `VN_DEBUG=init`.

## Video decoding

Linux guests, Mac hardware decoder:

```toml
[display]
video_decode = true
```

V4L2 decoder `/dev/video0`: H.264, HEVC (Main, Main 10), VP9 (0, 2), AV1
(M3+). Up to 4096x2304.

```bash
sudo sh install-virtio-media.sh    # scripts/guest/, builds with DKMS
v4l2-ctl --list-devices            # Video decoder (platform:virtio-media)
sudo usermod -aG video $USER       # log in again
```

```bash
gst-launch-1.0 filesrc location=movie.mp4 ! qtdemux ! h264parse ! v4l2h264dec ! autovideosink
ffmpeg -c:v h264_v4l2m2m -i movie.mp4 -f null -
```

- Up to 8 decoders. They survive snapshots (resume at the next key frame).
- FFmpeg 6.1's `v4l2m2m` decoders are flaky; GStreamer and VA-API are fine.
- Runs in sandboxed `vroom video-helper`
  ([SECURITY.md](SECURITY.md#video-decoding)).

### Browsers: VA-API

```bash
sudo sh install-vaapi.sh     # scripts/guest/, after virtio-media
vainfo                       # Driver version: vroom (VideoToolbox ...)
```

H.264, HEVC, VP9 (not AV1). Needs `gpu = "virtio3d"`. Log in again after.

- Firefox (snap or deb): works by default. Check `about:support` → HWDEC.
  If YouTube sends AV1: `media.av1.enabled = false`.
- Chrome / Chromium: works by default with `opengl_host = "apple"`. Check
  `chrome://media-internals` → `VaapiVideoDecoder`. Under X11 it needs
  `--render-node-override=/dev/dri/renderD128`; the script adds it (a
  `google-chrome` wrapper in `/usr/local/bin`, `CHROMIUM_FLAGS` for the
  snap). WebRTC H.264 and VP9 decode on the Mac too.
- Re-run the script after updates to it.

## Video encoding

```toml
[display]
video_encode = true
```

V4L2 encoder: NV12/P010 in, H.264 or HEVC out (no VP8/VP9/AV1). Same
virtio-media driver as decoding.

```bash
ffmpeg -i in.mp4 -pix_fmt nv12 -c:v h264_v4l2m2m -b:v 4M out.mkv
ffmpeg -vaapi_device /dev/dri/renderD128 -i in.mp4 -vf 'format=nv12,hwupload' -c:v h264_vaapi -b:v 4M out.mp4
```

- Bitrate, CBR/VBR/CQ, GOP, profile. No B-frames. CBR = low latency
  (~5 ms a frame).
- VA-API HEVC rounds height to 16 (1080 → 1088); use `hevc_v4l2m2m` for
  exact sizes.
- Chrome WebRTC encodes H.264 on the Mac (needs
  `--enable-features=AcceleratedVideoEncoder`, added by
  `install-vaapi.sh`). VP8/VP9 calls stay in software. Check
  `chrome://webrtc-internals` → `VaapiVideoEncodeAccelerator`.
- Up to 8 encoders; survive snapshots (a few frames lost).
- Sandboxed in `vroom video-helper --encode`
  ([SECURITY.md](SECURITY.md#video-encoding)).

## Shared folders

```toml
[[share]]
path = "/Users/me/shared"
tag = "host"
readonly = false     # default true
```

Linux: `sudo mount -t virtiofs host /mnt` (fstab:
`host /mnt virtiofs defaults 0 0`). No symlinks, hard links or device
nodes created; existing symlinks aren't followed on the host.

### Windows guests

Drive letters from Z: down. `vroom-setup.cmd` sets up the viofs driver,
[WinFsp](https://winfsp.dev) (its MSI is on the tools disk; no internet
needed) and one `VroomShare_<tag>` service per share. New or removed
shares take effect after a reboot.

Mount on a folder instead (must not exist yet):

```bat
sc config VroomShare_host binPath= "\"C:\Program Files\Vroom\virtiofs.exe\" -t host -m C:\Shares\host"
sc stop VroomShare_host & sc start VroomShare_host
```

No links; 259-character paths; case-insensitive on a Mac host.

## Network adapters

The bar's network button: per adapter, plug/unplug the cable, or switch
NAT / Bridged / Host-only live (same MAC, re-DHCP). Saved to the file.

In settings: new adapters hot-plug on UEFI VMs. Port forwards, MAC
changes and NAT on a VM started without it wait for the next start.
[NETWORKING.md](NETWORKING.md#changing-adapters-while-the-vm-runs).

## USB devices

```bash
brew install libusb
vroom usb list
```

```toml
[[usb]]
id = "1209:000a"
```

The bar's USB button connects a Mac device for this run; "Always connect
to this VM" saves it to the file. Each device runs through its own
sandboxed `vroom usb-host` ([SECURITY.md](SECURITY.md#usb-devices)).
Suspend and revert replug devices; run-only ones are handed back on
suspend.

### Devices macOS uses

HID, disks, audio, webcams, USB Ethernet and common serial chips are held
by macOS drivers. Taking them needs the root helper: the menu's "Set up
(asks for your password)".

USB disks as a raw device on macOS 15.3+ also need Full Disk Access for
`/usr/local/libexec/vroom/vroom-netd` (again after each update). Usually
you want [USB disks](#usb-disks) instead.

A device left taken by an older vroom: replug it, or
`vroom usb reset 18a5:0302`.

### USB disks

Clicking a disk asks:

- **Share its files**: stays mounted on the Mac; the guest sees it as a
  shared folder. No password.
- **Use as a disk**: the guest gets the whole disk (admin password). Mac
  remounts it after. Close files on it first.
- **Pass the USB device**: needs Full Disk Access (above).

"Remember for this device" keeps the choice (`mode` in
[CONFIG.md](CONFIG.md#usb-host-usb-devices-up-to-8)).

- Linux with the agent: `/media/vroom/<volume>`. Without:
  `sudo mount -t virtiofs vroom-usb0 /mnt`.
- Windows: a drive letter, like shared folders.
- Two disks at a time. Mac hosts only. Suspend disconnects them.
- macOS may ask about removable volumes: Privacy & Security, Files &
  Folders.

Testing: `hdiutil attach -nomount disk.dmg`, then
`VROOM_USB_TEST_DISKS=disk6 vroom-gui`. On `vroom run`:
`VROOM_USB_TEST_DISK=<file>`, `VROOM_USB_TEST_FILES=<dir>`
(`VROOM_USB_TEST_AFTER`, `VROOM_USB_TEST_FOR`).

### Limits

- Without the root helper, composite devices only get the functions macOS
  leaves alone.
- All transfer types, USB 1.1 to 3.x. No bulk streams (UAS falls back).
- Linux hosts: usbfs, needs a udev rule, e.g.
  `SUBSYSTEM=="usb", ATTR{idVendor}=="1209", ATTR{idProduct}=="000a", MODE="0660", TAG+="uaccess"`.

Fake devices: `VROOM_USB_FAKE=acm|audio|disk|disk-broken`,
`VROOM_USB_FAKE_UNPLUG=<secs>`.

## The root helper (vroom-netd)

Root is needed for bridged/host-only networking and for taking USB devices
from macOS drivers. Only vroom-netd runs as root
([SECURITY.md](SECURITY.md#the-root-helper-vroom-netd)).

From the app: "Set up (asks for your password)" where needed, or `⌘K` →
"Set up vroom-netd" / "Remove vroom-netd". Logs:
`/var/log/vroom-netd.log`.

By hand:

```bash
sudo mkdir -p /usr/local/libexec/vroom /etc/vroom
sudo install -o root -g wheel -m 755 target/release/vroom-netd /usr/local/libexec/vroom/
sudo install -o root -g wheel -m 644 "$(realpath /opt/homebrew/lib/libusb-1.0.0.dylib)" \
    /usr/local/libexec/vroom/libusb-1.0.0.dylib
sudo install -o root -g wheel -m 644 packaging/netd.toml /etc/vroom/
sudo install -o root -g wheel -m 644 packaging/org.vroom.netd.plist /Library/LaunchDaemons/
sudo launchctl bootstrap system /Library/LaunchDaemons/org.vroom.netd.plist
```

Remove:

```bash
sudo launchctl bootout system/org.vroom.netd
sudo rm -rf /Library/LaunchDaemons/org.vroom.netd.plist /usr/local/libexec/vroom /etc/vroom
```

`/etc/vroom/netd.toml`: `group` (networking, default `staff`),
`[usb] group` (default `admin`), `[usb] enabled`, `bridge_interfaces`
(default `["en*"]`), `[[host_only]]` (default `lab`, 192.168.77.1/24:
[NETWORKING.md](NETWORKING.md#host-only)). Restart:
`sudo launchctl kickstart -k system/org.vroom.netd`.

Linux: `packaging/vroom-netd.service`, group `vroom`.

## Sound

```toml
[audio]
output = true
input = true     # microphone, off by default
```

- Windows: Intel HD Audio, inbox driver.
- Linux: virtio-snd (cloud images: `linux-modules-extra-$(uname -r)`), or
  `device = "hda"`.
- Linux hosts: ALSA, needs a glibc build.
- Testing: `VROOM_AUDIO_WAV_DIR=dir` writes `audio.wav`, feeds `mic.wav`.

### Microphone

Live only while the VM's window is open, the VM runs, and the guest
records. The bar shows "Microphone in use". macOS asks permission for the
app that started vroom (vroom, or your terminal). Denied: the window says
so; fix in Privacy & Security, Microphone, or
`tccutil reset Microphone org.vroom.app`. Follows the Mac's default input.

## Camera

`camera = true`: a standard USB webcam fed by the Mac's camera. Same gating
and permission rules as the microphone. 640x480, 1280x720 or 320x240 at 30
or 15 fps. Testing: `VROOM_CAMERA_TEST=1` sends color bars.

## Snapshots, suspend

- Snapshot: `Ctrl-A s` / `Ctrl+Alt+Shift+S` → `<state>/snapshots/snap-<time>`.
- Revert: `vroom run vm.toml --restore <state>/snapshots/snap-…`.
- Suspend: `Ctrl-A z` / `Ctrl+Alt+Shift+Z` / closing the window. Next
  `run` resumes; `--fresh` ignores it.
- `vroom snapshot list|delete|prune vm.toml` (VM stopped): deleting also
  drops and merges unused disk layers.
- A snapshot's disk layers go read-only. A save whose layers changed
  since (another program wrote one) is refused: restoring it corrupts
  the guest's disk.

## Disks

- Under 2 GiB free in the state dir: the VM pauses. Free space, resume
  (`Ctrl-A p`).
- TRIM gives space back. `vroom disk compact vm.qcow2` (VM off) shrinks
  the file.
- `sync = "fsync"` (default, survives host crash), `"full"` (survives
  power loss, slow), `"none"` (scratch only).

### Read-only disks

`readonly = true`: nothing the guest does changes the image.

- Linux sees it read-only on virtio and NVMe.
- Windows sees it on virtio ("write protected"), not on NVMe: it mounts
  read-write and writes fail at flush (Setup copes). Use `bus = "virtio"`
  once the drivers are in.

## Known limitations

- x86_64 hosts build but can't run VMs.
- Bridged tested on vmnet (Wi-Fi); host-only DHCP on vmnet untested by a
  guest yet; tap (Linux) untested.
- Windows 11 installs with hardware checks bypassed (no Secure Boot);
  `tpm = true` (`brew install swtpm`) gives a real TPM 2.0.
- Windows 3D: Direct3D 11 only.
- NAT connections don't survive suspend.
- USB: tested with fake devices; real USB sticks via vroom-netd untested
  with Full Disk Access.
