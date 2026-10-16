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
   └─ optional: vroom video-helper (video_decode), own sandbox, stream
      socket: bitstreams and credits in, pictures out (VideoToolbox)

vroom-gui ── starts vroom usb-host for a device connected from the VM
             window, or asks vroom-netd to start it as root; passes its
             socket to the VMM (viewer socket, SCM_RIGHTS)
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

## Layers

| Crate | Role |
|---|---|
| `hv` | `Hypervisor` / `Vm` / `Vcpu` traits. HVF backend (in-kernel GICv3, vtimer, EL2 for nested), KVM arm64 backend (vGICv3 with save/restore, in-kernel PSCI). Exits are decoded into `VcpuExit` (MMIO, hypercall, PSCI events). |
| `memory` | Guest RAM as guarded mmaps; every access bounds-checked (`GuestMemory`). |
| `arch` | Board layout (QEMU-virt compatible), FDT, ACPI tables (own AML encoder), SMBIOS, kernel image loader (Image, gzip, EFI zboot). |
| `devices` | Bus, PL011, PL031, fw_cfg (+DMA), CFI flash (NVRAM), PCI host (ECAM, MSI, MSI-X, INTx), NVMe, xHCI + USB HID (8 USB 2 + USB 3 port pairs for passthrough devices, plugged in and out while the VM runs; asynchronous transfers; isochronous TDs go out in batches, a few batches ahead), Intel HD Audio (controller + codec, streams moved by a timer thread). Parses hostile input only; `forbid(unsafe_code)`. |
| `video` | Hardware video decoding, shared by the VMM and `vroom video-helper`: their protocol (credits, epochs), H.264/HEVC NAL unit, access unit and parameter set parsing, picture order counts. Parses hostile input only; `forbid(unsafe_code)`. |
| `usb` | USB passthrough: the VMM/helper protocol, the helper's request checks, libusb (loaded with `dlopen`, helper only; the root helper only from its own copy), host device listing (IOKit registry / sysfs), a pretend serial port and sound card for tests. |
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
| `gui` | The app (Slint): VM list, settings that edit the VM file in place (toml_edit, comments kept), New VM, `⌘K` palette, VM windows. All state changes go through one message queue on the UI thread. VM windows only see pixels and send input. |
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
the disks didn't change in between.

Host GPU objects can't be saved as they are. For GL (virgl) contexts the
GPU device follows each submitted command stream (`virgl_state.rs`): live
objects, and the latest of each setter, dropping a setter once a later one
writes everything it wrote (vrend's own rules: sampler views clear the
slots after them, sampler states don't). A snapshot reads every texture
level and buffer back through virglrenderer; restore makes the resources
again, writes the contents, makes the contexts and replays their state,
then returns the fenced commands the GPU had finished. Vulkan (Venus)
contexts and blob resources aren't tracked: snapshots refuse while they
exist. Nested-virtualization EL2 state is best effort.

Layers form a tree under `<state>/disks`. `vroom snapshot delete/prune`
(VM stopped) removes layers nothing references and folds a layer that only
one child still needs into that child (copying up the clusters the child
lacks, then re-pointing its header), so chains stay short. A state dir is
locked by its VMM; every image is locked exclusive (writer) or shared
(read-only layer), and a snapshot downgrades the frozen layer to shared.

Shared folders keep their FUSE node table as paths, so a snapshot carries
it and open files are reopened by path on restore. Sound hands back
in-flight buffers before saving.

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
  died). On Wayland the helper answers `CoherenceNo` with the reason.

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
`StartApp` (Windows) presses the Windows key, types the name as
characters (`KEYEVENTF_UNICODE`, any keyboard layout) and Enter.

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
has focus). Once a second the app looks at which screen the bar is on
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
decoder: H.264 or HEVC buffers in (OUTPUT queue), NV12 pictures out
(CAPTURE queue), with the source-change event, the LAST buffer and
decoder STOP/START the V4L2 decoder interface describes. Buffers are
allocated in one shared memory window (region 0, 1 GiB, mapped into the
guest where the guest puts it; `vmm/src/video.rs`), and the guest maps
them with virtio-media's MMAP command.

The decoding happens in `vroom video-helper` (`cli/src/video.rs`,
`cli/src/vt.rs`). The VMM copies each bitstream buffer out of the window
and sends it; the helper splits it into access units and parameter sets
(`video/src/nal.rs`), makes a VideoToolbox decompression session for
the parameter sets (a new one when the size changes, announced to the
VMM before its first picture), decodes, and puts the pictures back in
display order: VideoToolbox hands them out in decoding order, so the
helper works out each picture's order count from its slice header, as
the standards define it, and holds as many as the stream's SPS says it
may reorder (`video/src/poc.rs`). Pictures go back only against credits:
one per picture buffer the guest has queued, under an epoch the VMM bumps
when those buffers change (size change, seek, stream off); the VMM drops
pictures from an older epoch. The VMM then copies the picture into the
guest's buffer and returns it with the timestamp of the bitstream buffer
it came from. Bitstream buffers go back to the guest as soon as they are
copied out.

A pause (snapshot) first answers every command the guest waits on (its
driver gives up after 5 seconds). Decoders don't survive a restore: the
device saves only which sessions were open and fails them afterwards.

## 3D

`gpu = "virtio3d"` adds three protocols to virtio-gpu. Venus: guest Vulkan
(Mesa's venus driver) serializes commands, virglrenderer replays them on
MoltenVK. virgl: guest OpenGL (Mesa's virgl driver) sends Gallium
commands, virglrenderer's vrend replays them on macOS's OpenGL 4.1 (CGL),
or with `opengl_host = "angle"` on OpenGL ES from ANGLE's Metal backend.
Neptune: Windows' viogpu3d driver sends Direct3D 11 calls, virglrenderer
(UTM's fork) replays them on DXMT, Direct3D on Metal. All capsets are
offered; Mesa picks per API, Windows takes Neptune. Venus and Neptune go
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
is pending. With GL on, the render server's fences are polled the same
way (it only has a fence thread of its own with GL off). A GL scanout is copied back on flush: from a context of our
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
an older one plus `#extension` lines macOS's compiler rejects. virglrenderer
logs through our logger (`virgl:` lines).

What MoltenVK lacks, the Venus decoder makes up (patches 0007 and 0008):

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
- For Zink only (its instance says "mesa zink"): transform feedback taken
  and dropped (queries of it are occlusion pools never started, results
  zero), and packed 10-bit vertex formats and RGB32 texel buffers mapped
  to the nearest Metal has. These are what lift Zink to OpenGL 4.4.

A Vulkan window reaches the screen through the guest's compositor, which
imports the frame's dma-buf with OpenGL (virgl, `PIPE_RESOURCE_SET_TYPE`).
On macOS's OpenGL (no EGL, no memory objects) vrend makes a texture of its
own, maps the Venus blob's shm read only (layout checked against its size)
and copies it in with `glTexSubImage2D` the first time each command stream
samples, blits or copies from it. ANGLE takes UTM's path instead (a Metal
texture on the same memory, no copy). When a guest context goes away,
commands still waiting on its fences go back to the guest: virglrenderer
drops those fences.
