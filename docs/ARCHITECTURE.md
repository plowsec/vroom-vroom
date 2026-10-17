# Architecture

## Processes

```
vroom-gui ──(socketpair: display protocol + shared framebuffer fd)──┐
                                                                      │
vroom run <vm.toml>   one process per VM, sandboxed after setup ◄───┘
   │
   ├─ vCPU threads (one per vCPU, own the hv vCPU handle)
   ├─ device workers (virtio queues, NAT, display, agent, ramfb)
   ├─ optional: unix socket to vroom-netd (root) for bridged/host-only
   ├─ optional: swtpm (vTPM), own sandbox, control + data socketpairs
   ├─ optional: vroom audio-out (sound, microphone), own sandbox, datagram socket
   ├─ optional: vroom usb-host (one per [[usb]] device), own sandbox,
   │  stream socket: transfer requests in, results and plug events out
   │  (a device a macOS driver holds: vroom-netd's root helper instead,
   │  asked for before the sandbox)
   └─ optional: vroom video-helper (video_decode), own sandbox, stream
      socket: bitstreams and buffer grants in; pictures written into the
      shared buffer window (VideoToolbox)

vroom-gui ── starts vroom usb-host for a device connected from the VM
             window, or asks vroom-netd to start it as root; passes its
             socket to the VMM (viewer socket, SCM_RIGHTS); the same for
             a vroom-netd port when an adapter goes bridged or host-only
vroom-netd   root daemon (launchd): vmnet ports, root USB helpers
```

The GUI never touches the guest: it shows a framebuffer the VMM shares
with it and forwards input. Closing it makes the VMM suspend the VM.

`vroom-netd` is the only privileged piece. It owns vmnet (macOS) or
tap+bridge (Linux) and hands each VMM an `AF_UNIX` datagram socket carrying
Ethernet frames, after policy checks (allow-listed interfaces, per-user
limits, source-MAC anti-spoofing). The VMM never holds an interface.
It also starts the USB helper as root (`vroom-netd usb-host`, sandboxed)
for a device a macOS driver holds, when the app asks for it by id, and
hands the app its socket. The app installs and removes it with macOS's
password prompt (`gui/src/rootd.rs`). Its protocol (`net/src/netd.rs`) is
one JSON line in: a network request (the VMM), a USB request or a status
question (the app).

## USB devices while the VM runs

The xHCI has 8 USB 2 + USB 3 port pairs for host devices whatever the
VM file lists (`PASS_PORTS`); the VM file's devices take the first ones.
The VM window's USB menu lists the Mac's devices (`usb/src/list.rs`,
IOKit registry). Connecting one: the app starts `vroom usb-host` for it
in its sandbox (or gets a root one from vroom-netd), and sends
`ToVmm::UsbAttach` with the helper's socket attached. The VMM
(`vmm/src/usb.rs`) puts it on a free pair (`Xhci::pass_plug`) and wires
it like the helpers it starts itself; the guest sees the port's connect
change once the helper says the device is there. `ToVmm::UsbDetach`
shuts the socket: the guest sees the device go (`pass_unplug`), the
helper ends. Each plug bumps a generation, so a helper that is going
away can't speak for the next device on the same pair. The VMM tells
the window what it has plugged in (`ToViewer::Usb`), and the window
tells the next one, if the app restarts. A saved VM keeps the number of
pairs it was saved with (the guest read it at driver start); older saves
had one pair per device in the VM file.

## USB disks

A USB disk can also go to the guest without its USB device (macOS
15.3 and later refuse USB storage without Full Disk Access). The app
does the host side (`gui/src/usbdisk.rs`, the menu in
`gui/src/console/usbmenu.rs`); the disk is found below the USB device
in the IOKit registry (`usb::list::disks`, `mounts`).

- Files: the app opens the volume's directory and issues a sandbox
  extension for it (`sandbox::issue_extension`), and sends both with
  `ToVmm::UsbFiles`. The VMM consumes the token (one consumed before
  `sandbox_init` doesn't carry into the sandbox it makes, and the VMM
  handles the viewer while it sets up: `vroom_sandbox` keeps such a
  token and consumes it right after the sandbox is applied) and puts a
  `vroom_fs::Share` made from the fd on a free removable share
  (`virtio::devices::fs::FsSlot`): a virtio-fs device whose tree can be
  swapped while the guest has it mounted. The worker takes the new tree
  at once (a poke), dropping every node and open file of the old one.
  No PCI hotplug, so a VM started on a Mac has two of them from the
  start (`vroom-usb0`, `vroom-usb1`), added after every other device so
  older VMs keep their device places; a save records how many it has
  (`usb.file_shares`) and a resume builds those. The VMM tells the agent
  (`Message::UsbShare`), which mounts it (Linux: `/media/vroom/NAME`
  through `systemd-run`; Windows: starts the share's virtiofs.exe
  service, made demand-start for these tags) and unmounts it when it
  goes. The viewer list shows it at `FILES_PORT0` + slot.
- Disk: the app unmounts the disk, opens `/dev/rdiskN` (itself, or
  through `authopen`) and sends the fd with `ToVmm::UsbDisk`. The VMM
  wraps it (`usb::rawdisk::FdStore`) and runs the helper's code in a
  thread of its own (`helper::serve` with `msd::StoreOpener`): the
  Bulk-Only/SCSI stick (`usb::msd`), on a free port pair like any
  helper. `UsbDetach` waits for that thread to flush and close the disk
  before the viewer hears of it; the app then waits for macOS to read
  the disk again and mount it (`usbdisk::remount`, also when the VM
  ends). macOS takes a disk's partitions away while a program has the
  raw disk open for writing, and reads them again when the last one
  closes it; `mountDisk` succeeds on a disk with no partitions, so the
  app checks a volume did mount, asks again for 30 seconds, and reads
  a disk it owns (a disk image) once more by opening it for writing
  and closing it. A stick that still shows no partitions gets a
  message: unplug it and plug it in again.

`[[usb]] mode = "files" | "disk"` entries are the app's to connect once
the VM has its USB controller; `vroom run` skips them. Without the app,
`VROOM_USB_TEST_DISK` / `VROOM_USB_TEST_FILES` do the same for tests.

## Network adapters while the VM runs

Each virtio-net device keeps its backend, its MAC and its cable in a
`Link` shared with a `NetControl` the VMM holds (`virtio/src/devices/net.rs`).
The worker takes the link's lock for each batch of frames. Pulling the
cable clears the status bit and raises a config interrupt; while it is
out, frames are dropped both ways. A new backend is registered on the
worker's poller (a clone of its `mio::Registry`) after the old one is
taken off, and the link bounces: down, then up after two seconds on the
worker's tick, so the guest asks DHCP again. A paused VM hears about it
when it resumes. `MacXlate` (`net/src/mac.rs`) wraps a backend whose
port passes another MAC than the guest's.

`vmm/src/net.rs` owns the adapters (`NetPorts`): what each is now, the
NAT an adapter started with while it is on another network (it holds
the port forwards' listening sockets), and what the sandbox leaves
(NAT only if an adapter had it at start; DNS read before the sandbox).
The viewer asks (`ToVmm::NetConnect`, `ToVmm::NetSet`, with a netd
socket for bridged and host-only) and is told the result
(`ToViewer::Net`, replayed to a window that attaches later). On the app
side, `gui/src/console/netmenu.rs` keeps the VM file in step: it maps
the file's adapters to the running ones (an adapter added while running
has none, a removed one is unplugged), asks netd for ports off the UI
thread, and gives the settings page what waits for the next start.

A resumed VM is built with the adapters of the config it was saved
with, set up as the VM file says now (`net::plan`): the device layout
must match the snapshot. A device's snapshot keeps its MAC, its link
state and which network it was on: if the backend now passes another
MAC, it translates; if the network changed, the link bounces after the
resume. The adapters built are what the next save records.

## Layers

| Crate | Role |
|---|---|
| `hv` | `Hypervisor` / `Vm` / `Vcpu` traits. HVF backend (in-kernel GICv3, vtimer, EL2 for nested), KVM arm64 backend (vGICv3 with save/restore, in-kernel PSCI). Exits are decoded into `VcpuExit` (MMIO, hypercall, PSCI events). |
| `memory` | Guest RAM as guarded mmaps; every access bounds-checked (`GuestMemory`). |
| `arch` | Board layout (QEMU-virt compatible), FDT, ACPI tables (own AML encoder), SMBIOS, kernel image loader (Image, gzip, EFI zboot). |
| `devices` | Bus, PL011, PL031, fw_cfg (+DMA), CFI flash (NVRAM), PCI host (ECAM, MSI, MSI-X, INTx), NVMe, xHCI + USB HID (8 USB 2 + USB 3 port pairs for passthrough devices, plugged in and out while the VM runs; asynchronous transfers; isochronous TDs go out in batches, a few batches ahead), Intel HD Audio (controller + codec, streams moved by a timer thread). Parses hostile input only; `forbid(unsafe_code)`. |
| `video` | Hardware video decoding, shared by the VMM and `vroom video-helper`: their protocol (grants, epochs, resume state), H.264/HEVC NAL unit, access unit and parameter set parsing, picture order counts, VP9 frame headers and AV1 OBUs. Parses hostile input only; `forbid(unsafe_code)`. |
| `usb` | USB passthrough: the VMM/helper protocol, the helper's request checks, libusb (loaded with `dlopen`, helper only; the root helper only from its own copy), host device listing (IOKit registry / sysfs), taking a device from the host's drivers and giving it back, a USB stick (Bulk-Only, SCSI) on a RAM disk for tests or on a Mac disk's fd ("Use as a disk"), a pretend serial port and sound card for tests. |
| `virtio` | Split-queue handling (hardened), MMIO and modern PCI transports, devices (blk, net, console, rng, gpu, input, fs, sound, media), a pausable worker per device. `forbid(unsafe_code)`. |
| `fs` | Host side of shared folders: component-wise `openat(O_NOFOLLOW)` walks from a root fd, Linux errno mapping. The virtio-fs FUSE server lives in `virtio`; it serves Linux and virtio-win's virtiofs.exe (Windows), telling them apart by their INIT flags. |
| `audio` | Sound helper: datagram protocol, ring buffer, CoreAudio (macOS) / ALSA (Linux, `dlopen`) playback and recording, WAV test files. |
| `gpu3d` | virglrenderer loaded with `dlopen`, behind the `Renderer3d` trait the virtio crate defines. The one place with GPU FFI. `remote`: the same trait over a socket to the GPU process, and that process's loop. |
| `block` | raw and qcow2 (COW, compressed reads, backing chains with an allow-list, discard/zero clusters with hole punching, folding a layer into its child), sync modes, swappable layers, flock-based image locks. VMDK reader (read-only) for importing VMware disks. |
| `net` | Userspace NAT (smoltcp for the guest side, host sockets outward), DHCP/DNS, ICMP echo, fragments; netd wire protocol. |
| `snapshot` | `Snapshottable` trait, snapshot directories with per-blob SHA-256 and an atomic manifest. |
| `state` | A VM's state dir: one-VMM lock, snapshot list/delete, pruning disk layers nothing needs. Shared by the CLI and the app. |
| `vmm` | Builds the board from the config, runs vCPUs, PSCI, the agent channel, display arbitration (ramfb vs virtio-gpu), snapshots and suspend, disk layering. |
| `sandbox` | Seatbelt profile (macOS), seccomp + Landlock (Linux). |
| `gui` | The app (Slint): VM list, settings that edit the VM file in place (toml_edit, comments kept), New VM, `⌘K` palette, VM windows. All state changes go through one message queue on the UI thread. VM windows only see pixels and send input. Labels stay short; what a control does is in its tooltip: one `Tip` (`ui/controls.slint`) on Slint's `Tooltip` (hover delay, by the pointer, never takes a click), shown under its control on keyboard focus too. |
| `agent` / `proto` | Guest agent (Linux, Windows) and its framed protocol over a virtio-serial port. |

## Board (aarch64)

QEMU-virt compatible memory map so stock EDK2 (`edk2-aarch64-code.fd`) works:
flash at 0, GICv3 at 0x0800_0000 (+ a GICv2m MSI frame, emulated when the
backend lacks one), UART 0x0900_0000, RTC, fw_cfg, virtio-mmio slots at
0x0a00_0000, PCI ECAM/windows, RAM at 0x4000_0000. Virtio shared memory
windows (GPU host-visible memory) sit at 0x70_0000_0000.

UEFI boots get ACPI (MADT, GTDT, SPCR, MCFG, DSDT with PCI and virtio
devices) and SMBIOS through fw_cfg. Direct kernel boots get an FDT.

## vCPU loop

Each vCPU thread owns its hypervisor handle and reacts to commands
(power-on, pause, save/restore state, reset, stop). MMIO exits go to the
device bus; PSCI is handled in the VMM (HVF) or the kernel (KVM). WFI is
emulated with short sleeps bounded by the guest timer deadline.

Pausing is synchronous: every vCPU acknowledges, device workers park and
disk I/O threads finish their requests before anything reads guest
memory. That's what makes snapshots consistent.

Disks (virtio-blk, NVMe): a lone small request runs on the vCPU that
submitted it; when the guest keeps several in flight they go to a few
I/O threads per disk, and data moves between the image and guest RAM
without a bounce buffer. See [PERFORMANCE.md](PERFORMANCE.md).

## Snapshots and suspend

A snapshot is a directory: manifest (hashes), per-vCPU state, irqchip
state, one blob per device, sparse RAM (64 KiB chunks plus a bitmap). Disks
are frozen at the same moment: the current top layer becomes read-only and
a new qcow2 overlay starts on it. Reverting starts a fresh overlay on the
frozen layer. Suspend is a snapshot that ends the process; resuming checks
the disks didn't change in between. On restore, devices come back before
the vCPUs: a vCPU runs as soon as its state is back.

Host GPU objects can't be saved as they are. For GL (virgl) contexts the
GPU device follows each submitted command stream (`virgl_state.rs`): live
objects, and the latest of each setter, dropping a setter once a later one
writes everything it wrote (vrend's own rules: sampler views clear the
slots after them, sampler states don't). A snapshot reads every texture
level and buffer back through virglrenderer; restore makes the resources
again, writes the contents, makes the contexts and replays their state,
then returns the fenced commands the GPU had finished. Blobs of GL
contexts aren't tracked: snapshots refuse while one is in use. The GL
side also keeps the type a compositor gives a Vulkan window's blob
(`PIPE_RESOURCE_SET_TYPE`) and replays it before anything uses that blob.

Vulkan (Venus) contexts are saved by their render worker
(`third_party/virglrenderer/0010-venus-snapshots.patch`, `vkr_snap.c`).
The worker runs every command, from the virtqueue, a ring or a command
stream, through one function that notes what the command looked up (the
decoder's object lookups), made and destroyed (the context's object
table). It keeps, as the guest encoded them, the commands that made or
destroyed objects, bound memory, wrote descriptors or recorded command
buffers. Venus ids are the guest's own, so replaying the kept commands in
a fresh context gives the same objects under the same ids. What goes:

- a command buffer's recording, when it begins again or is reset (also
  through its pool);
- a descriptor write that a later one writes over completely, unless a
  recording still holds it (it bound that set while the old value was
  there: the replay must record it the same way); a copy of descriptors
  stops this for everything older;
- now and then, everything nothing alive needs. An object is needed if it
  is alive or a needed command uses it; a command is needed if it makes a
  needed object, destroys one (the replay must too), or, making nothing,
  uses only needed objects. So a pipeline keeps the shader module it was
  made from, even after the app destroyed it.

Past 4M commands or 1 GiB kept after that clean-up, the context isn't
followed any more and comes back lost.

A snapshot (`RENDER_CONTEXT_OP_SAVE` from the GPU process's proxy, in turn
from `Renderer3d::venus_save`) first stops every thread of the context at a
command boundary: dispatching threads hold a gate per command, and a ring
stores its head inside it, so the saved head is exactly what ran. A
command blocked in a wait (`vkWaitVirtqueueSeqnoMESA`, `vkWaitForFences`)
lets go of the gate while it waits; it runs again from the start after a
restore. Then each queue gets an empty submission with a fence, and the
worker waits for those. It writes, into one shared memory object: the
shared memory the context made (rings, reply streams), the kept commands,
the contents of each device memory object (host-visible memory from its
shared memory; device-local memory a buffer was bound to, through a
staging buffer), the contents of each image (every level, layer and
aspect, copied to a staging buffer with `VK_IMAGE_LAYOUT_GENERAL`:
MoltenVK keeps no layouts, Metal has none; images get transfer usage
added when they are made, where the format allows it), fence and event
states, timeline semaphore values and which binary semaphores are left
signaled (followed from submissions), each ring's place and creation
parameters, and the reply stream positions. If the threads don't stop in
5 s, the GPU doesn't finish in 10 s, or the context had already failed,
only the rings are written: the context comes back lost.

The VMM stores that next to the GL state, with the context's blobs (how
each was made, where the guest had it mapped). Restore: make the context,
make its shared-memory blobs (blob id 0) again with their ids,
`RENDER_CONTEXT_OP_RESTORE` (the worker copies the shared memory back,
replays the commands with replies off, fills memory and images, sets the
states, makes the rings at their old head, alive), then the memory blobs
(blob id = the memory object's id, so they export the new memory),
`RENDER_CONTEXT_OP_RESUME` (start the rings), map every blob where the
guest had it, and only then the GL contexts. A context the worker can't
build again gets plain shared memory for its memory blobs, so the guest's
pages stay backed, and its rings marked fatal: Mesa's Venus driver stops
that app ("ring fatal error"), the rest of the guest goes on.

Nested-virtualization EL2 state is best effort.

Layers form a tree under `<state>/disks`. `vroom snapshot delete/prune`
(VM stopped) removes layers nothing references and folds a layer that only
one child still needs into that child (copying up the clusters the child
lacks, then re-pointing its header), so chains stay short. A state dir is
locked by its VMM; every image is locked exclusive (writer) or shared
(read-only layer), and a snapshot downgrades the frozen layer to shared.

Shared folders keep their FUSE node table as paths, so a snapshot carries
it and open files are reopened by path on restore. Sound hands back
in-flight buffers before saving.

## Several monitors

`[display] monitors` (1 to 4). The shared framebuffer has a slot per
monitor; each monitor's sink (`FbSink`) writes its slot and tells the
viewer (`Mode`/`Damage` for the first, `HeadMode`/`HeadDamage` with the
monitor's number for the others). ramfb only shares the first.

virtio-gpu has a scanout per sink. A resource can show on several
scanouts at different places (X11 makes one framebuffer for all
monitors): a flush goes to each scanout that shows it, clipped to its
part; a GL flush reads the union of those parts back once and hands
each scanout its piece. GET_DISPLAY_INFO reports each scanout, and a
window that closes unplugs its monitor (a display event).

Linux gets one GPU with N scanouts. Windows' drivers drive one scanout
each, so a Windows guest gets one GPU per monitor, added after the other
devices (their PCI slots don't move) and numbered in the PCI subsystem
id (0x1100 + n), which the agent reads to tell them apart. Only the
first has 3D: two 3D GPUs would need to share frames (Windows draws a
monitor on one GPU and shows it on another), which the Direct3D driver
can't do across renderers. The others are 2D; Windows' 2D driver clears
"its framebuffer" in BAR 0 with stores the hypervisor can't decode, so
those GPUs have plain memory there and their registers in BAR 2.

The viewer sends each window's size (`HeadResize`, 0x0 when it closes)
and pointer (`HeadPointer`). The agent service turns the sizes into one
layout, monitors side by side, top edges lined up (`SetLayout`, applied
with xrandr, Mutter's DisplayConfig, or ChangeDisplaySettingsEx); the
display thread maps a monitor's pointer onto that desktop for the
tablet. Windows maps the tablet to its main monitor only: there the
tablet, elsewhere the agent moves the pointer (`Mouse`, SendInput).

## Drag and drop

Finder drags reach the app's window as winit events (no position: the
app polls the mouse while files hover and moves the guest's pointer with
it). The app sends the names (`ToVmm::DragEnter`); the VMM's agent
service passes them on (`Message::DragEnter`), and the guest agent's
session side (`agent/src/drop.rs`) makes a staging folder with them. On
Windows (`dnd_win.rs`) it then shows a window under the pointer, brought
in front of the active app and composed before it answers (Windows routes
no click to a window before that), and answers `DragReady { live }`. The
app presses the guest's left button: the window gets it and starts a
shell drag (`SHDoDragDrop`) of the staged names, which Windows tracks as
the pointer moves. On X11 desktops (`dnd_x11.rs`) the window is an
InputOnly one and the agent is the source of an XDND drag (it owns
`XdndSelection` and hands out the staged files as `text/uri-list`).
Wayland has no way for an outside program to drag: the files go to the
Desktop. On the drop the app opens a socket pair, sends one end
to the VMM with `ToVmm::Drop` (SCM_RIGHTS), and writes the files into the
other as agent frames; the VMM forwards them, checked, from a thread of
its own. When the guest says `DropDone`, the app lets go of the button
and the app under the pointer takes the files. Leaving the window sends
`DragCancel`, Escape and the button up.

The other way (guest to Mac, `gui/src/dragout.rs`): when the mouse
leaves the guest screen with the guest's button held since a press on
it, the app asks once (`ToVmm::DragOut`, agent `DragOutQuery`). The
agent (`agent/src/dragout.rs`) puts a small window under the guest's
pointer, now at the screen edge, and makes it a drop target: an OLE
`IDropTarget` on Windows, an `XdndAware` window on X11. It nudges the
pointer so the drag going on finds it; if that drag carries files
(CF_HDROP, or `text/uri-list` with file URIs), it learns their paths and
answers their names (`DragOutItems`, `ToViewer::DragOutItems`). The app
then starts an AppKit drag of file promises (`dragout_mac.rs`) from the
VM window while the Mac's button is still down, and gives up the guest's
drag (Escape, button up: the guest drops nothing). When something takes
the drop, AppKit asks each promise where to write; for each item the app
sends a pipe to the VMM (`ToVmm::DragOutGet`, SCM_RIGHTS), the VMM asks
the guest (`DragOutGet` with a stream number of its own) and passes the
guest's DropEntry, DropData and DropEnd frames on, checked, with the
drag's id. The app writes the item where the promise says. When all are
written (or the drag ends with no drop) the app sends `DragOut` with
`end`, and the agent forgets the paths.

## Coherence

The app asks the guest for a screen the size of the Mac screen's free
area (`ToVmm::Resize`, the usual path) and sends `ToVmm::Coherence(true)`;
the VMM's agent service passes `Message::Coherence` on, the guest's
service hands it to the session helper (`agent/src/coherence.rs`, with
`coh_win.rs` for Windows and `coh_x11.rs` for X11).

The helper looks at the windows when its desktop says they changed, at
most every 10 ms, and every 500 ms anyway in case an event got lost:

- Windows: `SetWinEventHook` (out of context, on a thread of its own with
  a message loop) for foreground, menus, move and size, minimize, and
  top-level windows created, destroyed, shown, hidden, moved, renamed,
  cloaked. The callback only sets a flag and wakes the session loop.
  The helper hides the taskbar, makes the whole screen the work area,
  and walks the windows front to back (`GetTopWindow`/`GW_HWNDNEXT`:
  `EnumWindows` leaves out Store apps and the Start menu), with
  per-monitor DPI awareness so it reads real pixels.
- X11: a second connection selects structure and property events on the
  root and on every managed window. The list is `_NET_CLIENT_LIST`, put
  in the order of the root's children (the window manager's frames), plus
  override-redirect windows above the lowest frame (menus, tooltips).
  What can be seen of a window is the client area, plus
  `_NET_FRAME_EXTENTS`, less `_GTK_FRAME_EXTENTS` (GTK's shadows).
  Windows of other workspaces, the desktop and docks are left out.
  Actions are the client messages a taskbar sends (`_NET_ACTIVE_WINDOW`,
  `WM_CHANGE_STATE`, `_NET_CLOSE_WINDOW`, `_NET_MOVERESIZE_WINDOW` with
  static gravity). Ubuntu's dock is set to hide (`gsettings`, put back
  after; what it was is noted in `$XDG_RUNTIME_DIR` for a helper that
  died). GNOME's top bar can only be hidden from inside GNOME Shell (no
  setting does it): the guest tools install a small extension
  (`examples/linux/gnome-shell`, `vroom-coherence@vroom`; one file for
  GNOME 42 to 44, one for 45 and later) that takes the top bar out of
  the struts and hides it while enabled. The helper runs
  `gnome-extensions enable` when Coherence goes on and `disable` when it
  goes off (and at start). GNOME Shell only finds a new extension when
  it starts, so it works from the login after the install. On Wayland
  the helper answers `CoherenceNo` with the reason.

When the list changes the helper sends `Message::Windows`: id (window
handle or X window), what can be seen of it, flags (in front, minimized,
app window, maximized), title. App windows' icons go as `WindowIcon`
(32x32 RGBA) when new and when they change (looked at every 2 s):
Windows draws the window's icon (`WM_GETICON`, the class icon, or its
program's) twice, on black and on white, which gives the alpha of any
kind of icon; X11 takes the size nearest 32 of `_NET_WM_ICON`. Back come
`WindowDo` requests: activate, minimize, close (`SC_CLOSE` /
`_NET_CLOSE_WINDOW`), move (to the visible bounds, borders added back).
Only windows of the last list are touched, in the VMM and the agent.
A Store app's window (in `ApplicationFrameHost`, or a packaged program)
has no icon of its own and its program none either: the helper finds
its package (`GetPackageFullName` on the process,
`GetPackagePathByFullName`), the app's entry in `AppxManifest.xml` (by
its program), its `Square44x44Logo`, and of that logo's files the
unplated one at the size nearest 32 (`appx.rs`), a PNG decoded in the
guest.

`StartApp` (`apps.rs`) reads the menu again, on a thread, and starts
the app the name fits best: its name (whole, then its start, then a
word's start) before its other names; of equal ones, apps that run in
a terminal last, then a shorter name first. Windows (`apps_win.rs`): the `.lnk` files of both Start menu folders (named as
the shell shows them, localized through `desktop.ini`), then the apps
of `shell:AppsFolder` no shortcut has the name of (Store apps by
AppUserModelID; enumerated through `IShellItem` and
`IEnumShellItems`); opened with `ShellExecute` (the `.lnk`, or
`shell:AppsFolder\<id>`). Windows may open the new app behind the
window in front, so the helper waits up to 10 s for its first window
and brings it forward. No app by that name: if the name is plain, the
old way (the Windows key, the name typed as characters, Enter). Linux
(`apps_xdg.rs`): the desktop entries of the XDG data dirs (the user's,
`XDG_DATA_DIRS`, Flatpak and Snap), in precedence order by desktop file
id, without NoDisplay, Hidden, OnlyShowIn/NotShowIn and missing TryExec
ones; names in the session's language (`LC_ALL`, `LC_MESSAGES`,
`LANG`), other names from GenericName, Keywords and the file name.
Started with `gio launch <file>` (GLib 2.68 and later), or when that
fails with the Exec line split into words (quoting rules of the spec,
field codes out: no files) run directly, in its own process group,
from the home folder.

While Coherence is on the helper also sends the apps it can start
(`Apps`: name and 32x32 icon, at most 512, sorted), read on a thread of
its own when Coherence goes on and again every minute, sent when it
changed. Icons: Windows asks the shell (`SHGetFileInfo`) for shortcuts
and the package logo (as above) for Store apps; Linux looks the entry's
icon up in the icon themes (the user's GTK theme, Yaru, Adwaita,
hicolor..., PNG files only, the size nearest 32) or takes the file it
names. About 3 s on Windows (the shell draws every icon), 70 ms on
Ubuntu.

The VMM forwards the list to the app coalesced (`ToViewer::Windows`, like
damage: a busy guest costs one queued list), icons the same way per
window (at most 128 waiting). The app (`gui/src/console/coherence.rs`)
keeps one frameless Slint window per guest window on screen
(`ui/coherence.slint`): our strip on top, then the guest window's part
of the shared framebuffer, copied where damage touches it. Guest pixels
and Mac points convert through `Map` (the free area and the guest's
mode, which may be smaller than asked: Windows' driver caps it). Pointer
events map back through the picture's own size. Moves either way: the
guest's list places the Mac windows; a Mac window moved or resized by
the user sends `WindowDo::Move` (kept on the guest's screen) and is left
alone by guest reports for half a second. The guest's order is applied
with `orderFront:` while vroom is the active app; new windows open
behind other apps' windows when it isn't. The guest's window in front
gets the keyboard on the Mac while one of ours has it. The bar
(`CoherenceBar`) lists app windows with their icons, sends the Windows
key for Start, and has the app field (its keys stay in the bar while it
has focus). As one types there the bar shows the three apps of the
guest's list the text fits best (`suggest`) in place of the window
buttons; Enter sends the one picked, or the text if none fits. Once a second the app looks at which screen the bar is on
and at that screen's free area; if either changed (the bar dragged to
another screen, the Dock moved, a screen unplugged), the guest's screen
moves there: new area, a resize, the bar at the bottom. Leaving sends
`Coherence(false)` and the VM window's size; a viewer that goes away
(the app crashed) turns Coherence off in the VMM, and the agent puts the
taskbar back when its session ends or it starts again. A session helper
replaced by another (started twice) is told to leave Coherence first.

## Video decoding

`video_decode = true` (app: Settings, Display, Hardware video decoding) adds a virtio-media device (`virtio/src/devices/media`):
V4L2 carried over virtio, the guest's kernel forwarding each ioctl of
its `/dev/videoN` on the command queue. The device plays a V4L2 stateful
decoder: H.264, HEVC, VP9 or AV1 buffers in (OUTPUT queue; the codecs
the helper said this Mac decodes), NV12 or P010 pictures out (CAPTURE
queue), with the source-change event, the LAST buffer and decoder
STOP/START the V4L2 decoder interface describes. Buffers are allocated in
one shared memory window (region 0, 1 GiB, mapped into the guest where
the guest puts it; `vmm/src/video.rs`), and the guest maps them with
virtio-media's MMAP command.

The decoding happens in `vroom video-helper` (`cli/src/video.rs`,
`cli/src/vt.rs`). At start the VMM hands it the window's shared memory
object (it maps it) and the helper answers with the codecs VideoToolbox
decodes here. The VMM copies each bitstream buffer out of the window and
sends it; the helper takes it apart (`video/src/nal.rs` for H.264 and
HEVC: access units and parameter sets; `video/src/vpx.rs` for VP9 frame
headers and AV1 OBUs), makes a VideoToolbox decompression session for
the stream's format (a new one when the size, depth or a key frame's
format changes, announced to the VMM before its first picture), decodes,
and puts the pictures back in display order: VideoToolbox hands H.264 and
HEVC pictures out in decoding order, so the helper works out each
picture's order count from its slice header, as the standards define it,
and holds as many as the stream's SPS says it may reorder
(`video/src/poc.rs`).

Pictures don't cross the socket. The VMM grants the helper each guest
picture buffer it may fill (offset, length, NV12 or P010) under an
epoch it bumps when those buffers change (size change, seek, stream
off); the helper writes the picture from VideoToolbox's buffer straight
into the granted guest buffer (converting 10 to 8 bits if the guest
wanted NV12) and says which one. The VMM checks that it is the oldest
grant of the current epoch and of the stream's size, and returns the
buffer to the guest with the timestamp of the bitstream buffer it came
from. Bitstream buffers go back to the guest as soon as they are copied
out.

With V4L2's display delay set to 0 (`V4L2_CID_MPEG_VIDEO_DEC_DISPLAY_DELAY`,
enabled), a session runs in decoding order: no reordering, each picture
out as soon as VideoToolbox has it, and a picture that failed comes back
as a buffer with the error flag. That is for our VA-API driver.

A pause (snapshot) first answers every command the guest waits on (its
driver gives up after 5 seconds). The device saves each decoder's V4L2
side (formats, buffers and where they are in the window, queues) and the
helper's resume state (it sends the stream's parameter sets, VP9 key
frame header or AV1 sequence header whenever they change). On restore,
the new helper opens the stream from that state and the decoder goes on
from the next key frame; bitstream buffers that hadn't reached the helper
go back to the guest (the window's contents aren't saved).

### The guest's VA-API driver

Browsers decode through VA-API, which hands the driver pictures already
parsed. `scripts/guest/vaapi/vroom_drv_video.c` (a libva driver, in C,
built in the guest) turns them back into a bitstream for the V4L2
decoder, one buffer per picture, in decoding order:

- H.264: SPS and PPS written from VA-API's picture parameters and scaling
  lists, sent when they change; the PPS reference counts slices leave to
  it are read from the slice headers. The SPS says no reordering, so
  VideoToolbox outputs each picture at once.
- HEVC: VPS, SPS and PPS likewise. VA-API doesn't pass the SPS's
  reference picture sets that slice headers point to, so each slice
  header is rewritten to carry its picture's set (from VA-API's
  reference list); the rest of the header and the slice data are copied.
- VP9: the frame as it came (VA-API passes all of it).

Each VA surface has its own copy of its picture, filled when its
decoded picture comes back (matched by the timestamp the driver gave the
bitstream buffer). For players that want dma-bufs (Firefox), a surface
also gets two GBM buffers on the virtio-gpu render node (luma, chroma),
filled through their guest pages and one transfer each when exported.
Chrome exports each surface once, when it makes it, before anything is
decoded into it, and keeps showing that dma-buf; a GPU decoder writes
into it in place. So a surface exported while still empty is marked, and
from then on `vaEndPicture` waits for its picture and fills its buffers
before returning (Chrome shows a picture once that returns). Firefox
exports after decoding and keeps the old behaviour. When the stream's
format is set again (an HEVC seek), Chrome destroys its surfaces, keeps
the dma-bufs and makes new surfaces from them (`DRM_PRIME` memory, which
names only the first plane's buffer). A destroyed live surface's two
buffers are kept for 5 seconds (32 at most), and such a surface gets
them back by its luma buffer's handle; other dma-bufs are refused.

Chrome under X11 shows video through X pixmaps (DRI3), which take only
BGRA buffers, so it asks for video processing (`VAProfileNone`,
`VAEntrypointVideoProc`): NV12 or P010 into BGRA. It makes the BGRA
surfaces with `vaCreateSurfaces` and exports them, then imports both the
decoded surfaces and the BGRA ones back as dma-bufs. Such an import
becomes a surface that stands for the one that exported the buffer,
found again by its GEM handle at each use (Chrome may have remade it
from the same buffers meanwhile). `vaEndPicture` on a processing
context converts the picture on the CPU (NEON, 16 pixels by 2 rows,
nearest-pixel scaling) straight into the BGRA buffer's guest pages and
sends one transfer: Chrome doesn't sync before showing it. A decoded
surface that only feeds processing no longer fills its own GBM buffers.
The colour matrix comes from VP9 key frames; H.264 and HEVC don't pass
theirs through VA-API, so the driver guesses it from the size and bit
depth. Transfers to the host give no stride: the kernel refuses one for
these (non-blob) resources, and the host reads the rows packed, as GBM
lays them out.

## 3D

`gpu = "virtio3d"` adds three protocols to virtio-gpu. Venus: guest Vulkan
(Mesa's venus driver) serializes commands, virglrenderer replays them on
MoltenVK. virgl: guest OpenGL (Mesa's virgl driver) sends Gallium
commands, virglrenderer's vrend replays them on macOS's OpenGL 4.1 (CGL),
or with `opengl_host = "angle"` on OpenGL ES from ANGLE's Metal backend.
Neptune: Windows' viogpu3d driver sends Direct3D 11 calls, virglrenderer
(UTM's fork) replays them on DXMT, Direct3D on Metal. All capsets are
offered; Mesa picks per API, Windows takes Neptune for Direct3D and
virgl for OpenGL: our OpenGL driver for Windows (`vroom_gl.dll`, Mesa's
virgl built for Windows with a winsys of ours, `third_party/mesa/`) makes
a virgl context through viogpu3d's escapes and sends its command streams
in D3DKMT render packets. Its resources are the driver's 3D allocations,
which get the guest pages behind them to the host when they have a GPU
address and are resident. Each frame is read back into the window's
buffer and drawn with GDI, since Windows' compositor runs on Neptune and
can't see vrend's textures. Venus and Neptune go
through virglrenderer's proxy to its render server: ours
(`gpu3d/src/server.rs`, `vroom gpu-server`) starts one `vroom gpu-worker`
per guest context, which runs that context with virglrenderer's own
worker code (patch 0009 exports it) and sends buffers back as shared
memory it creates. GL contexts stay in the renderer: vrend shares one GL
context group between them. Host-visible buffers are mapped into the
guest's shared memory window with the hypervisor.

The renderer runs in the GPU process (`gpu3d/src/remote.rs`,
`vroom gpu-helper`; see SECURITY.md): the VMM's virtio-gpu device talks
to a `Remote`, the same `Renderer3d` trait the in-process `Virgl`
implements, over a Unix socket. One call at a time, each answered, in
the worker's order, so the device code can't tell the difference. A
second socket carries a byte when fences may be done (the worker's
backend fd). With 3D, guest RAM is shared memory (`MmapRegion::new_shared`)
the GPU process maps too; guest buffers go over as (region, offset,
length), checked against its mapping. A reset gives the guest new RAM
objects (zeroed); `GuestMemory::generation` changes and the next call
that names guest memory first sends the new ones. Buffers the guest maps
(Venus, Neptune) come back as shared memory fds the VMM maps itself;
frames read back land in a shared frame buffer the VMM made (sending
them over the socket cost a third of glmark2's score; now within a few
percent of in-process). `VROOM_GPU_IN_PROCESS=1` keeps it all in the VMM.

GL details (`gpu3d/src/gl.rs`): we create the CGL or EGL contexts
virglrenderer asks for (one share group, no surfaces) and keep the right
one bound on whichever thread calls in (setup thread, then the GPU
worker, which changes on device reset). GL fences have no completion thread (ANGLE
serializes a share group's contexts, so a thread blocked on a fence
would stall submits): the worker polls them every millisecond while any
is pending. The render server's contexts (Vulkan, Direct3D) always get a
fence thread in the GPU process, woken by their worker process
(`0013-proxy-fence-thread-always.patch`): polled once a millisecond like
GL, each Zink frame waited half a millisecond more on average, and
glmark2 on Zink scored 547 instead of 963. A GL scanout is copied back on flush: from a context of our
own, into a pixel buffer on the GPU, and shown when the copy lands while
the worker goes on. Windows' Direct3D frames are blobs in the render
server's shared memory that the guest never maps: a flush reads them
through a read-only mapping of that memory, made on first use.
Windows only lists the modes a monitor offered when it was plugged in,
and viogpu3d reports a plug only when the scanout's enabled flag changes:
once a Neptune context exists, a resize reports scanout 0 disabled, then
enabled at the new size (checked and retried until Windows shows it), and
the guest agent is asked for the mode after that.

The guest creates IMMUTABLE buffers and textures without data and fills
them right after with an update. Direct3D forbids writing those, and DXMT
orders nothing on them, so a copy or draw could read one before its
contents landed: in Explorer the glyph atlas lost its white texel (copied
from a 1x1 IMMUTABLE texture), and text and boxes drawn with it went
missing. Our virglrenderer patch creates such resources DEFAULT
(`third_party/virglrenderer/`).

The viewer can come and go. The VMM a window starts also listens on a
socket in the user's temp dir (`$TMPDIR/vroom/<hash of the state dir>.sock`,
0600 in a 0700 dir; the state dir's `viewer` file names it; socket paths
are too short for state dirs). A window that goes without asking for a
suspend (the app crashed) leaves the VM running; the next app connects,
gets the framebuffer and what it missed (mode, pointer, state, agent),
then everything as usual. One viewer at a time. No viewer for 5 minutes:
the VMM suspends. The state lock holds the VMM's pid, so the app can stop
one that hangs (only a process whose executable is `vroom`).

Direct3D state can't be saved. A suspend or snapshot of a Windows guest
using it first asks the guest agent to turn the 3D display device off
(SetupAPI, like Device Manager); the VMM waits until the guest has no
Direct3D context left and the agent confirms, saves, and asks for the
device back on (after a snapshot, and whenever an agent connects, so a
resumed or killed VM gets it back). A stopped driver leaves blobs behind;
host blobs no context uses are left out of the save. Where no buffer is
mapped, the host-visible window is backed by reserved anonymous memory,
so a guest touching a buffer it already gave back reads memory instead of
stopping the VM.

With GL on, 2D resources (dumb buffers) live in virglrenderer too, as in
QEMU: Xorg's glamor draws into them with GL, and a context that touches
a resource virglrenderer doesn't know is marked broken for good (every
window turns black). Without GL they stay plain CPU buffers.

macOS constraints: HVF maps at the host's 16 KiB by default; a 4 KiB-page
guest kernel places GPU buffers at 4 KiB steps, which can't be mapped that
way. With 3D, the VM asks for 4 KiB stage-2 pages
(`_hv_vm_config_set_ipa_granule`, private on macOS 15, public from 26),
and GPU mappings round to that. MoltenVK lacks
`VK_KHR_external_memory_fd`, which Mesa requires; our virglrenderer patch
advertises it and shares mappings by pointer instead
(`third_party/virglrenderer/`). Two more patches there make vrend work on
macOS's OpenGL 4.1: multisample textures without `glTexStorage*Multisample`
(GL 4.3), and shaders written at the host's GLSL version (4.10) instead of
an older one plus `#extension` lines macOS's compiler rejects. A third
(0011) clears textures without `glClearTexSubImage` (GL 4.4): it uploads
the guest's texel over the box, 4 MiB of rows at a time, through vrend's
normal upload path. Without it, the guest's Mesa 25.2 clears through a
surface the host never saw (handle 0), vrend puts the whole context in
error, and Xwayland under GNOME on Wayland drew nothing (its glamor clears
pixmaps this way). UTM's fork told the guest the Mac had no multisampling
(one sample, so Mesa fakes it), from before multisample textures worked
on GL 4.1; 0014 keeps the real count (4) on macOS's OpenGL. One sample
left the guest's OpenGL ES 3.0 below its spec, and ANGLE, so Chrome's
GPU process, refused to make an ES 3.0 context on it. With
`opengl_host = "angle"` it is still one: ANGLE's ES 3.0 has no
multisample textures. vrend pauses transform feedback after each draw
and ends it only when the guest drops its buffers; macOS's GL crashed
in `glEndTransformFeedback` when the program it began with wasn't
bound any more (any GL app using transform feedback, at its end: the
GPU process died). 0015 binds that program again for the end, or leaves
the object to the context's end if the program is gone. virglrenderer
logs through our logger (`virgl:` lines).

What MoltenVK lacks, the Venus decoder makes up (patches 0007, 0008 and
0012):

- `nullDescriptor`: the guest may leave a descriptor or vertex buffer
  null. Each device gets, on first need, a 64 KiB buffer for reads, one
  for writes, 1x1 images for sampling and storage, and views of them, all
  cleared to zero once on the device's first queue; null handles in
  descriptor writes, push descriptors and vertex buffer binds point there.
- `VK_EXT_depth_clip_enable`: depth clip off becomes depth clamp.
- Sync fd import into semaphores: `vkImportSemaphoreResourceMESA` (the
  guest waited for the fd, mark the semaphore signaled) becomes an empty
  submission that signals it, `vkWaitSemaphoreResourceMESA` one that waits
  on it. The guest's Venus needs this for `VK_KHR_swapchain` and
  `synchronization2`.
- Images and buffers the guest may export as dma-bufs only see host
  visible memory types: their memory is shm imported as host memory, which
  device local memory can't be. A guest that picks device local anyway
  gets host visible memory under it (it never maps it).
- Host image copy isn't offered for depth and stencil formats
  (0012): MoltenVK offers it, but keeps depth images in private Metal
  storage, so such an image gets no memory type at all. Zink asks for it
  on every texture it may copy into from the CPU, and every depth
  texture failed.
- For Zink only (its instance says "mesa zink"): transform feedback, and
  packed 10-bit vertex formats and RGB32 texel buffers mapped to the
  nearest Metal has. These are what lift Zink to OpenGL 4.4.

Transform feedback, which MoltenVK lacks, is done by the vertex shader
itself (0016, `vkr_xfb.c`). A vertex shader module with the `Xfb`
execution mode gets a second host module: before each return it stores
its captured outputs (their `XfbBuffer`, `XfbStride` and `Offset`
decorations) through buffer device addresses read from 48 bytes of push
constants at offset 256. Every pipeline layout gets room for them (the
vertex range grows to 304 bytes; a layout already using that range gets
none, and its pipelines don't capture). Zink devices get
`bufferDeviceAddress`, `shaderInt64` and vertex stores turned on,
transform feedback buffers the device address usage, and every memory
allocation the device address flag. Each command buffer keeps the bound
buffers (address, size), what was written to each, the bound pipeline's
topology and the transform feedback state; before every draw with such
a pipeline the host pushes where its vertices go, or zeros (nothing to
write). Vertex n of instance i goes to slot i * count + n, which is the
order transform feedback writes in for non-indexed point, line and
triangle lists only; other draws capture nothing. The host caps the slots
at what fits in every buffer, in whole primitives, so a shader never
writes past a buffer's bound range. Counter buffers (pause and resume
across render passes, `glDrawTransformFeedback`) and the query pools for
transform feedback and pipeline statistics (`GL_PRIMITIVES_GENERATED`;
MoltenVK has neither) are kept by the host as commands are recorded:
values are right when command buffers run in the order they were
recorded, which is what Zink does. Before this, a Zink app asking for
`GL_PRIMITIVES_GENERATED` got a query pool the host refused and its
context failed. Apps without transform feedback run as before, within
noise (glmark2 on Zink, three runs each: terrain 270 then 260 fps,
refract 596 then 637).

A Vulkan window reaches the screen through the guest's compositor, which
imports the frame's dma-buf with OpenGL (virgl, `PIPE_RESOURCE_SET_TYPE`).
On macOS's OpenGL (no EGL, no memory objects) vrend makes a texture of its
own, maps the Venus blob's shm read only (layout checked against its size)
and copies it in with `glTexSubImage2D` the first time each command stream
samples, blits or copies from it. ANGLE takes UTM's path instead (a Metal
texture on the same memory, no copy). When a guest context goes away,
commands still waiting on its fences go back to the guest: virglrenderer
drops those fences.
