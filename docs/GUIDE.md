# Guide

## Prerequisites (macOS, Apple Silicon)

```bash
brew install qemu            # only for the EDK2 firmware files and qemu-img
scripts/app.sh --install     # vroom.app, in /Applications
```

`qemu` is not used to run anything: vroom only reads
`/opt/homebrew/share/qemu/edk2-aarch64-code.fd` (UEFI) from it.
`scripts/build.sh` builds just the two programs, in `target/debug`
(`--release`: `target/release`), for use from the terminal.

## The Mac app

`scripts/app.sh` builds `target/dist/vroom.app`; `--install` also copies
it to `/Applications` (it refuses while VMs run from the copy there).
You can also drag it there yourself. Then vroom starts like any Mac app:
from Finder, Launchpad, Spotlight, or the Dock once you keep it there.

Build the 3D libraries before the app if you want 3D in its VMs
(`scripts/build-virglrenderer.sh`, then `scripts/build-dxmt.sh` for
Windows guests, see below). `app.sh` puts whatever was built in the app.

What it does that the bare `vroom-gui` doesn't:

- Open a VM file with it (right-click, Open With, vroom, or drop it on
  the Dock icon): the VM starts, as with `vroom-gui vm.toml`. vroom
  doesn't make itself the default app for `.toml` files; to double-click
  VM files, pick vroom in Finder's Get Info, "Open with", for that file.
- Drop an ISO on the Dock icon: New VM, with that installer. A VMware VM
  (`.vmwarevm`): the import.
- Camera and microphone questions name vroom, not Terminal.
- Its own messages go to `~/Library/Application Support/vroom/logs/vroom-gui.log`
  (there is no terminal to print to). Each VM's last run log is in
  `logs/` there too (`⋯` → Show log).

The terminal still works: `/Applications/vroom.app/Contents/MacOS/vroom`
is the VMM with all its commands (a symlink to it in your `PATH` is fine).

Inside the app:

```text
vroom.app/Contents/
  MacOS/vroom-gui        the app
  MacOS/vroom            the VMM, started by the app
  Frameworks/*.dylib     3D: virglrenderer, MoltenVK, DXMT, ANGLE
  Resources/vroom.icns   the icon (packaging/macos/)
  Info.plist             packaging/macos/Info.plist
```

The app is signed ad hoc: the VMM with the hypervisor entitlement, then
the bundle. That is enough on the Mac that built it. It isn't enough to
give it to other people: their Mac blocks it ("not from an identified
developer") until they allow it in System Settings, Privacy & Security.
A proper release needs an Apple Developer ID certificate, signing with
the hardened runtime (and then the camera and microphone entitlements,
`com.apple.security.device.camera` and `...device.audio-input`, next to
the hypervisor one), and Apple's notarization. None of that is set up.

macOS remembers the camera and microphone answers for the app's
signature. An ad hoc signature changes with each build, so after a
rebuild macOS may ask again.

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

### The VM window

A bar along the top of the window holds the VM's name and state, then
pause, snapshot, suspend, the power menu (shut down, restart, force power
off) and, for Windows, Ctrl+Alt+Del. On the right: how busy the VM is
(the host CPU time it uses, out of what its CPUs could use), "3D" or
"Direct3D 11" when 3D is on, USB devices (see [USB devices](#usb-devices)),
Coherence (below), the settings (the VM's page in the app),
full screen, and the VM list. The guest gets the rest of the window: a
guest with the agent takes exactly that size. In full screen the bar
goes away and slides in when the mouse touches the top edge.

The mouse pointer over the guest is one pointer, the guest's:

- A guest that hands its pointer to the GPU (Linux desktops that use the
  hardware cursor) gets the Mac's pointer drawn with the guest's picture,
  as in VMware. When the guest hides it, it's hidden.
- Windows draws its pointer into the screen picture itself (its virtio-gpu
  drivers, 2D and 3D, have no hardware pointer). Over a Windows guest the
  Mac's pointer hides and Windows' own shows, as fast as the guest draws.
  It's hidden during boot too, when Windows shows none.
- Any other guest that gives no pointer keeps the Mac's arrow.

### Drag and drop

Drag files or folders from Finder onto the VM window to copy them into
the guest. Needs the guest tools (the agent) of this version or later.

- **Windows:** the drop lands wherever you let go, as if you had dragged
  the files inside Windows: into an Explorer folder, onto the desktop, into
  a mail as an attachment, onto an app. While you hover, Windows shows
  its own drag picture and "Copy to Documents".
- **Linux:** the files go to the Desktop (a name that is taken gets a
  number: `photo (2).jpg`), and the desktop shows a notification.

While the files hover, the window's edge lights up and a line at the
bottom says what a drop does (or why the guest can't take them: no guest
tools, nobody logged in). Nothing reaches the guest until you drop: only
the names, so it can get ready. Then a bar shows the copy's progress.

Names Windows can't take (`:` `?` `*` and the like) get a `_` instead.
Links inside dropped folders are left out. Dragging from the guest to the
Mac isn't there yet.

### Coherence (Windows guests)

Coherence shows each Windows app as a window of its own on your Mac,
without the Windows desktop and taskbar around it. VMware calls it Unity.
Needs the guest tools of this version or later.

- Turn it on with the window button in the VM's bar, or
  `Ctrl+Alt+Shift+C`. The VM window goes away; each Windows window gets a
  Mac window where it was on the Windows screen.
- Windows' screen takes the size of your Mac screen (less the menu bar,
  the Dock and the bar below), so a maximized Windows app fills the screen.
- Every such window has a thin strip on top with the VM's name and the
  window's title. Drag the strip to move the window; drag its edges to
  resize it. The Windows app follows. Moving it by its own title bar works
  too.
- A bar at the bottom stands in for the taskbar: **Start** opens the Start
  menu, then one button per app window (minimized ones too: click to bring
  one back), and **Leave Coherence**. The bar can be dragged anywhere.
- `Cmd+W` on a Coherence window asks the Windows app to close (it may ask
  to save first).
- Leaving (the bar, the VM's shortcut, or closing the bar) brings back the
  VM window, the taskbar and the VM's old size. A VM that stops, suspends,
  or whose guest tools go away leaves Coherence too.

Not there yet: Linux guests, several Mac screens (the Windows screen sits
on the screen the VM window was on), app icons in the bar and the Dock.
A window tiling tool (Amethyst, yabai) moves Coherence windows like any
other, and the Windows apps follow it: tell it to leave `vroom-gui` alone.

### When the app crashes, or a VM stops answering

Each VM runs in a process of its own, so a crash of the app doesn't stop
it:

- The VM keeps running without its window. Start vroom again and the
  window comes back by itself, as it was (the guest may have turned its
  screen off meanwhile: press a key).
- If vroom isn't started again within 5 minutes, the VM suspends itself
  rather than run unseen. If it can't be saved (a Vulkan app runs, say),
  it stays up and its window comes back the next time vroom starts.
- In the list, such a VM shows as running, with "Running without a
  window"; Show window, pause, snapshot and the power menu all bring the
  window back first.
- A VM that doesn't answer any more (its window can't come back) gets "isn't
  answering". Force power off (power button, top right) stops it: vroom
  asks the process to end, and kills it if it's still there 3 seconds
  later. It only ever does that to a vroom process.
- A VM started from a terminal (`vroom run`) has its window there; the
  power button in the app can still force it off.

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
- **Vulkan** (Venus): needs `mesa-vulkan-drivers` (Mesa ≥ 24) and the
  user in the `render` group; stock Ubuntu kernels work. `vulkaninfo
  --summary` should show `Virtio-GPU Venus (Apple M…)`. With 3D on, vroom
  maps guest memory in 4 KiB steps instead of the Mac's 16 KiB, so the
  guest kernel can put GPU buffers where it likes (macOS 15 has this only
  as a private call; without it, 16 KiB steps and the guest needs a 16 KiB
  page kernel, see `examples/ubuntu/kernel-16k.sh`). Memory speed in the
  guest doesn't change measurably (fill and random reads, both ways). There is no swapchain
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

Snapshots and suspend work with OpenGL. GL state lives on the host GPU,
so vroom keeps track of what each GL app set up (its shaders, states and
bindings) from what it sends, reads every texture back when saving, and
builds it all again on resume. Measured on the 22.04 desktop with Settings
and glxgears open: about 105 MiB of GPU contents, 16 s to suspend, 17 s to
resume, glxgears still drawing afterwards. A few things come back empty:
multisampled buffers, and depth buffers with `opengl_host = "angle"`
(OpenGL ES can't read depth); apps redraw those every frame anyway.
Refused, with the reason shown: a running Vulkan app (Vulkan state isn't
tracked), and GPU memory shared with the guest (blobs). Closing the window
then leaves the VM paused with "Can't save this VM's state"; keep it
running, or shut it down from the guest. Windows guests with 3D are saved
another way (see below).

The build: `build-virglrenderer.sh` builds UTM's fork of virglrenderer
(the one with Direct3D for Windows, below), pinned by hash, with our
patches. `build-angle.sh` fetches ANGLE at the commit Chrome's stable
release uses, with Google's depot_tools, about 3 GB while it builds
(removed afterwards), and leaves `libEGL.dylib` and `libGLESv2.dylib`
(6 MB) in `target/angle`. `build-virglrenderer.sh` builds libepoxy 1.5.10
(EGL on, patched to load ANGLE from its own directory, or Apple's OpenGL
when the VMM asks for it) into virglrenderer (3.4 MB), with our patches
from `third_party/virglrenderer/` (two of them make it work on macOS's
OpenGL 4.1). Both copy their results next to `target/*/vroom`, where the
VMM looks; in vroom.app they go in `Contents/Frameworks`, where the VMM
looks too (`scripts/app.sh` and `scripts/dist.sh` do that). virglrenderer
and MoltenVK always, ANGLE's two only for `opengl_host = "angle"`,
DXMT's only for Windows guests.

A VM with 3D runs as two processes: the VM (`vroom run`) and its GPU
process (`vroom gpu-helper`), which holds virglrenderer and the GPU and
nothing else (see [SECURITY.md](SECURITY.md#the-gpu-process)). It comes
and goes with its VM. If it crashes, the guest loses 3D until the VM is
started again (`gpu: the GPU process is gone` in the log); the rest of
the VM keeps running.

## 3D in Windows guests (Direct3D 11)

```bash
brew install meson ninja molten-vk llvm@15
scripts/build-virglrenderer.sh
scripts/build-dxmt.sh             # Direct3D 11 on Metal (about a minute)
```

In the guest, the 3D driver replaces the plain display driver:
`viogpu3d-arm64x-signed.zip` from the
[releases of osy/kvm-guest-drivers-windows](https://github.com/osy/kvm-guest-drivers-windows/releases)
(UTM's author; Microsoft-signed, no test mode needed). Unpack it where
Windows can read it (a shared folder, or a small disk), then as
administrator:

```bat
pnputil /add-driver viogpu3d.inf /install
```

Shut down, set `[display] gpu = "virtio3d"`, start again. Windows then
draws its desktop with Direct3D 11 on the Mac's GPU: the guest driver
sends Direct3D calls to the host, where virglrenderer hands them to DXMT,
which runs them on Metal. Device Manager shows "Red Hat VirtIO GPU 3D
controller", and apps get Direct3D feature level 11_1.

What works and what doesn't (Windows 11 24H2, M3 Max):

- The lock screen, the desktop, the Start menu (with its blur), Explorer,
  Settings and Task Manager draw, and windows move smoothly.
- Once, on the first boot with 3D, Settings lost its text in parts it
  didn't just redraw (pointing at it brought it back). Not seen again in
  later runs; if it happens, closing and reopening the app fixes it.
- Windows follows the window's size and the Retina scale as with the 2D
  driver. The 3D driver only takes a new size from a monitor that is
  plugged in again, so on each resize vroom unplugs Windows' virtual
  monitor and plugs it back in: the screen goes black for a moment and
  windows stay where they were. At the login screen Windows uses its UI
  scale from before (100%), so with `hidpi` text there is small until you
  log in.
- With the 3D driver installed, `gpu = "virtio2d"` gets Windows' basic
  display driver (800x600, no resizing); install viogpudo again to use 2D.
- No Direct3D 12, no Vulkan or OpenGL for Windows apps.
- Suspend and snapshots work, a few seconds slower than without 3D.
  Direct3D's state lives on the Mac's GPU and can't be saved, so vroom
  first has the guest agent turn the 3D display device off, as in Device
  Manager: Windows goes on with its basic display for a moment and lets
  go of all its Direct3D objects. After the save (snapshot) or on resume
  (suspend), the agent turns it back on and Windows redraws in 3D. Your
  apps and session stay as they were; a game or 3D app that was running
  loses its GPU state (most rebuild it, like after a driver update).
  Measured: suspend 8 s, resume 6 s, with 8 GB of guest memory. Needs the
  current vroom tools in Windows (re-run `vroom-setup.cmd`); with older
  ones vroom says so and doesn't save.
- DXMT keeps the Metal versions of the shaders it has converted in the
  VM's state folder (`shaders/`), so apps start faster the second time.

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
```

In the VM window: the USB button in the bar lists the Mac's USB devices.
Click one to connect it to the VM, click it again to give it back to
the Mac, like VMware's "Connect (Disconnect from Host)". A connected
device has "Always connect to this VM" under it: tick it and it goes in
the VM file, so the VM takes it at every start. Without the tick it is
only for this run.

From the terminal, the same thing for every start:

```bash
vroom usb list               # what's plugged in, with its id
```

```toml
[[usb]]
id = "1209:000a"
```

The library's "USB devices" card edits that list too.

Unplugging a device and plugging it back in works while the VM runs.
After a suspend/resume or a snapshot revert the guest sees the device
plugged in anew (transfers in flight can't be saved). Devices connected
only for the run are given back on suspend; connect them again after.

Each device goes to the guest through a helper process (`vroom usb-host`,
one per device, in its own sandbox); the VM process itself never opens a
host device. For a device connected while the VM runs, the app starts
the helper and hands the VM its socket. See
[SECURITY.md](SECURITY.md#usb-devices).

### Devices macOS uses

Keyboards, mice, game pads and FIDO security keys (HID), disks and card
readers (mass storage), sound cards, webcams, USB Ethernet, and the
common USB serial chips (FTDI, CP210x, CH34x, PL2303, CDC-ACM) have a
macOS driver that holds them. The menu says so ("macOS uses it as a
disk"). Taking a device from a macOS driver needs root, or Apple's
`com.apple.vm.device-access` entitlement, which Apple gives to approved
VM vendors only.

vroom does it with vroom-netd, its one helper that runs as root (the
same one bridged networking needs, see below). The first time, the menu
shows "Set up (asks for your password)": macOS asks for an admin
password once, and from then on these devices connect like the others.
vroom-netd starts the USB helper as root for that one device; the app
and the VM stay unprivileged. Eject a disk in Finder before you connect
it: macOS lets go of it, mounted or not.

Devices that work without it: those no macOS driver holds. That's
vendor-specific devices: many dev boards, flashing and DFU modes, debug
probes, logic analyzers, SDR sticks, Android phones in adb or fastboot
mode.

### Limits

- A device composed of several functions works for the ones macOS
  leaves alone; the guest sees the others but they fail.

Control, bulk, interrupt and isochronous transfers work, at USB 1.1, 2.0
and 3.x speeds (SuperSpeed devices show up on a USB 3 port). Isochronous
is what sound cards and webcams stream with: once the guest has such a
device (on macOS, see above), it streams. The host sends the packets as
soon as it can, in order: a guest asking for an exact start frame gets
the next free one. No USB 3 bulk streams (UAS
disks fall back to the older protocol).

On Linux hosts the helper uses usbfs: give your user access to the device
with a udev rule, e.g.
`SUBSYSTEM=="usb", ATTR{idVendor}=="1209", ATTR{idProduct}=="000a", MODE="0660", TAG+="uaccess"`.
Linux drivers bound to the device's interfaces are detached while the VM
has it.

Testing without hardware: `VROOM_USB_FAKE=acm` makes every helper serve
a pretend USB serial port that echoes what it's sent (CDC-ACM; Linux
guests see `/dev/ttyACM0` if they have the `cdc_acm` module, or use it
from userspace through `/dev/bus/usb`). `VROOM_USB_FAKE=audio` serves a
pretend USB sound card instead: speakers (48 kHz, 16-bit stereo) and a
microphone (48 kHz, 16-bit mono) whose samples count up by one, so a
recording shows whether anything was lost. It runs in real time, one
packet per millisecond. Linux guests need the `snd-usb-audio` module (on
Ubuntu cloud images: `linux-modules-extra-$(uname -r)`). `arecord -l`
shows its card number (here 0); then, as root or in the `audio` group:
`arecord -D hw:0,0 -f S16_LE -r 48000 -c 1 -d 5 -t raw mic.raw`.
`VROOM_USB_FAKE_UNPLUG=<secs>` pulls the device out once, for three
seconds.
Started with `VROOM_USB_FAKE`, the app's USB menu also lists a pretend
device (1209:00aa) to connect.

## The root helper (vroom-netd)

Two things need root on a Mac: bridged and host-only networking (vmnet),
and taking a USB device from a macOS driver. vroom keeps both in one
small daemon, vroom-netd, so nothing else ever runs as root. What it can
do and why that's safe: [SECURITY.md](SECURITY.md#the-root-helper-vroom-netd).

### Set up from the app

The app offers it where it's needed: the network card of a VM with a
bridged or host-only adapter, starting such a VM, and the USB menu next
to a device macOS uses. Click "Set up (asks for your password)"; macOS
shows its own password prompt. It installs:

- `/usr/local/libexec/vroom/vroom-netd`, and a copy of libusb next to it
  (root owns both: a root process loads nothing a user can replace);
- `/etc/vroom/netd.toml`, its settings (kept if there is one already);
- `/Library/LaunchDaemons/org.vroom.netd.plist`, so it starts with the
  Mac; it logs to `/var/log/vroom-netd.log`.

The app needs `vroom-netd` next to `vroom-gui` (`scripts/build.sh`
builds it). Setting it up again (after an update) replaces the binary
and keeps your settings. `⌘K` → "Set up vroom-netd" does it from
anywhere; `⌘K` → "Remove vroom-netd" takes it away again (the password
once more).

### By hand

```bash
sudo mkdir -p /usr/local/libexec/vroom /etc/vroom
sudo install -o root -g wheel -m 755 target/release/vroom-netd /usr/local/libexec/vroom/
sudo install -o root -g wheel -m 644 "$(realpath /opt/homebrew/lib/libusb-1.0.0.dylib)" \
    /usr/local/libexec/vroom/libusb-1.0.0.dylib
sudo install -o root -g wheel -m 644 packaging/netd.toml /etc/vroom/
sudo install -o root -g wheel -m 644 packaging/org.vroom.netd.plist /Library/LaunchDaemons/
sudo launchctl bootstrap system /Library/LaunchDaemons/org.vroom.netd.plist
```

Remove it:

```bash
sudo launchctl bootout system/org.vroom.netd
sudo rm -rf /Library/LaunchDaemons/org.vroom.netd.plist /usr/local/libexec/vroom /etc/vroom
```

`/etc/vroom/netd.toml` says who may use it: members of `group` (`staff`,
every user of the Mac) for networking, members of `[usb] group`
(`admin`, the users who could use sudo anyway) for USB devices,
`[usb] enabled = false` turns USB off. `bridge_interfaces = ["en*"]`
allows every Ethernet and Wi-Fi port; list names (`["en0"]`) to allow
fewer. Restart it after a change:
`sudo launchctl kickstart -k system/org.vroom.netd`.

Linux: `packaging/vroom-netd.service` (systemd), same config file, the
`vroom` group. There the USB part is optional: a udev rule gives your
user the device, and the helper detaches Linux drivers itself.

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

The window shows "Microphone in use" in its bar, and in full screen a
small "Mic" pill at the top while the bar is hidden. macOS shows its own
orange dot in the menu bar too. Focus doesn't matter: a call keeps going while
you work in another window, the way a native app would.

The first time a guest records, macOS asks for the microphone
permission, for the app that started vroom: "vroom" when you use
vroom.app, Terminal (or iTerm, VS Code...) for `vroom run` and for
`vroom-gui` started from a terminal. Saying yes to Terminal lets
everything started from it use the microphone. If you said no, or never
saw the question, the guest records silence and the window says so after
two seconds; turn the app on in System Settings, Privacy & Security,
Microphone (or reset the question with `tccutil reset Microphone
org.vroom.app`, or `com.apple.Terminal`).
Sound input in the guest follows the Mac's default input device (System
Settings, Sound, Input).

On Linux hosts the helper plays and records through ALSA (`libasound`,
loaded at run time, "default" device): use a normal (glibc) build there;
a static musl build can't load it and stays silent.

Testing without speakers or a microphone: `VROOM_AUDIO_WAV_DIR=dir vroom
run ...` writes what the guest plays to `dir/audio.wav` and, with
`input = true`, feeds `dir/mic.wav` (16-bit PCM, looped) to the guest
instead of the microphone. The window gate still applies.

## Camera

`camera = true` (app: Settings, Sound and camera, Camera) gives the guest
a USB webcam fed by the Mac's camera. Windows and Linux use it with their
own drivers (it is a standard USB video class camera): the Camera app,
browsers (Google Meet, Teams, Zoom on the web), `cheese`, OBS.

Like the microphone, the camera is on only while the VM's window is open
and an app in the guest uses it; it goes off two seconds after the app
stops. The window shows "Camera in use" (or "Camera and microphone in
use") in its bar, and macOS lights the green dot next to the camera.

The guest picks one of 640x480, 1280x720 or 320x240, at 30 or 15 frames a
second, uncompressed. The Mac's picture (16:9) is cropped to the shape the
guest asked for, around its middle, then scaled.

The first time, macOS asks for the camera permission for the app that
started vroom (vroom with vroom.app; Terminal, iTerm... from a
terminal), as for the microphone. If you said no, the window says so;
turn the app on in System Settings, Privacy & Security, Camera.

Testing without a camera: `VROOM_CAMERA_TEST=1 vroom run ...` sends
moving color bars instead.

## Snapshots, suspend

- `Ctrl-A s` / `Ctrl+Alt+Shift+S`: snapshot to `<state>/snapshots/snap-<time>`.
- `vroom run vm.toml --restore <state>/snapshots/snap-…`: go back to it
  (the disks too).
- `Ctrl-A z` / `Ctrl+Alt+Shift+Z` / closing the GUI window: suspend; the
  next `run` resumes. `--fresh` ignores the suspend image.
- With 3D: works with OpenGL apps, not while a Vulkan app runs (see 3D
  acceleration).
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
- Bridged / host-only: needs vroom-netd (set up from the app, see
  above); not tested with real vmnet/tap yet.
- Windows 11 is installed with its hardware checks bypassed (no Secure
  Boot); `tpm = true` (needs `brew install swtpm`) gives it a real TPM 2.0,
  which Windows sees and uses.
- Windows guests: 3D is Direct3D 11 only (see above). Shared folders need
  WinFsp (see above).
- 3D snapshots: not while a Vulkan app runs. Guest OpenGL stops at 4.1
  (2.1 with `opengl_host = "angle"`).
- NAT connections don't survive suspend/resume (the guest reconnects).
- USB passthrough: not yet tried with a real device in a guest; tested
  with the pretend devices behind the real helper and sandbox
  (isochronous: the pretend sound card in a Linux guest), connected from
  the VM file and from the app's menu while the VM runs. Taking a device
  from a macOS driver through vroom-netd isn't tested as root yet (the
  request, the root helper's sandbox and its libusb were, as a normal
  user).
