# Architecture

## Processes

```
vroom-gui ──(socketpair: display protocol + shared framebuffer fd;──┐
             macOS: a mach port for the screen's IOSurfaces)        │
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
   ├─ optional: vroom video-helper (video_decode), own sandbox, stream
   │  socket: bitstreams and buffer grants in; pictures written into the
   │  shared buffer window (VideoToolbox)
   └─ optional: vroom video-helper --encode (video_encode), own sandbox,
      stream socket: where raw frames are and buffer grants in; frames
      read from its buffer window, coded frames written into it

vroom-gui ── starts vroom usb-host for a device connected from the VM
             window, or asks vroom-netd to start it as root; passes its
             socket to the VMM (viewer socket, SCM_RIGHTS); the same for
             a vroom-netd port when an adapter goes bridged or host-only
vroom-netd   root daemon (launchd): vmnet ports, root USB helpers
```

- GUI never touches the guest: shows the framebuffer the VMM shares, forwards input. Closing it: VMM suspends the VM.
- `vroom-netd`: the only privileged piece.
  - Owns vmnet (macOS) or tap+bridge (Linux). Hands each VMM an `AF_UNIX` datagram socket of Ethernet frames after policy checks (allow-listed interfaces, per-user limits, source-MAC anti-spoofing). VMM never holds an interface.
  - Starts the USB helper as root (`vroom-netd usb-host`, sandboxed) for a device a macOS driver holds, when the app asks by id; hands the app its socket.
  - Installed/removed by the app with macOS's password prompt (`gui/src/rootd.rs`).
  - Protocol (`net/src/netd.rs`): one JSON line in: network request (VMM), USB request or status question (app).

## USB devices while the VM runs

- xHCI: 8 USB 2 + USB 3 port pairs for host devices regardless of the VM file (`PASS_PORTS`); the file's devices take the first.
- USB menu lists Mac devices (`usb/src/list.rs`, IOKit registry).
- Connect: app starts `vroom usb-host` in its sandbox (or gets a root one from vroom-netd), sends `ToVmm::UsbAttach` with the helper's socket. VMM (`vmm/src/usb.rs`) puts it on a free pair (`Xhci::pass_plug`), wires it like its own helpers. Guest sees the connect change once the helper reports the device.
- `ToVmm::UsbDetach`: socket shut, guest sees the device go (`pass_unplug`), helper ends.
- Each plug bumps a generation: a helper going away can't speak for the next device on the pair.
- VMM tells the window what's plugged (`ToViewer::Usb`); the window passes it to the next one if the app restarts.
- Saved VM keeps the pair count it was saved with (guest read it at driver start). Older saves: one pair per VM-file device.

## USB disks

USB disk to the guest without its USB device (macOS 15.3+ refuses USB storage without Full Disk Access). Host side in the app: `gui/src/usbdisk.rs`, menu `gui/src/console/usbmenu.rs`. Disk found below the USB device in IOKit (`usb::list::disks`, `mounts`).

Files:

- App opens the volume dir, issues a sandbox extension (`sandbox::issue_extension`), sends both with `ToVmm::UsbFiles`.
- Token: one consumed before `sandbox_init` doesn't carry into the new sandbox, and the VMM handles the viewer during setup: `vroom_sandbox` keeps it and consumes it right after the sandbox is applied.
- A `vroom_fs::Share` from the fd goes on a free removable share (`virtio::devices::fs::FsSlot`): a virtio-fs device whose tree can be swapped while mounted. Worker takes the new tree at once (a poke), dropping every node and open file of the old.
- Slots exist from start (PCI hot plug could add them later): a Mac-started VM has two (`vroom-usb0`, `vroom-usb1`), added after every other device so older VMs keep device places. Save records the count (`usb.file_shares`); resume builds those.
- VMM tells the agent (`Message::UsbShare`): mounts it (Linux `/media/vroom/NAME` via `systemd-run`; Windows starts the share's virtiofs.exe service, demand-start for these tags), unmounts on removal.
- Viewer list: at `FILES_PORT0` + slot.

Disk:

- App unmounts, opens `/dev/rdiskN` (itself or via `authopen`), sends the fd with `ToVmm::UsbDisk`.
- VMM wraps it (`usb::rawdisk::FdStore`), runs the helper code in its own thread (`helper::serve` with `msd::StoreOpener`): Bulk-Only/SCSI stick (`usb::msd`) on a free pair.
- `UsbDetach` waits for that thread to flush and close before telling the viewer. App then waits for macOS to reread and mount (`usbdisk::remount`, also at VM end).
- macOS removes a disk's partitions while a program holds the raw disk open for writing, rereads on last close. `mountDisk` succeeds on a disk with no partitions: app checks a volume mounted, retries 30 s, and for a disk it owns (disk image) opens it for writing and closes once more. Still no partitions: message to replug.

`[[usb]] mode = "files" | "disk"`: the app connects them once the VM has its USB controller; `vroom run` skips them. Tests without the app: `VROOM_USB_TEST_DISK` / `VROOM_USB_TEST_FILES`.

## Network adapters while the VM runs

Device (`virtio/src/devices/net.rs`):

- Each virtio-net keeps backend, MAC, cable in a `Link` shared with the VMM's `NetControl`. Worker locks the link per batch of frames.
- Cable out: status bit cleared, config interrupt; frames dropped both ways.
- New backend: old one taken off the worker's poller, new one registered (clone of its `mio::Registry`); link bounces (down, up 2 s later on the worker tick) so the guest re-DHCPs. Paused VM hears it on resume.
- `MacXlate` (`net/src/mac.rs`) wraps a backend whose port passes another MAC.

VMM (`vmm/src/net.rs`, `NetPorts`):

- Tracks each adapter's current backend, the start-time NAT kept while on another network (holds the port forwards' listening sockets), and what the sandbox allows (NAT only if an adapter had it at start; DNS read before the sandbox).
- Viewer asks `ToVmm::NetConnect`, `ToVmm::NetSet` (with a netd socket for bridged/host-only); told `ToViewer::Net` (replayed to a later window).

App (`gui/src/console/netmenu.rs`): keeps the VM file in step, maps file adapters to running ones (added-while-running has none; removed one is unplugged), asks netd for ports off the UI thread, gives the settings page what waits for next start.

Resume (`net::plan`): built with the saved config's adapters (layout must match the snapshot), set up as the file says now. Device snapshot keeps MAC, link state, network: other MAC now → translate; network changed → link bounces after resume. Adapters built = what the next save records.

## Layers

| Crate | Role |
|---|---|
| `hv` | `Hypervisor` / `Vm` / `Vcpu` traits. HVF backend (in-kernel GICv3, vtimer, EL2 for nested), KVM arm64 backend (vGICv3 with save/restore, in-kernel PSCI). Exits decoded into `VcpuExit` (MMIO, hypercall, PSCI events). |
| `memory` | Guest RAM as guarded mmaps; every access bounds-checked (`GuestMemory`). |
| `arch` | Board layout (QEMU-virt compatible), FDT, ACPI tables (own AML encoder), SMBIOS, kernel loader (Image, gzip, EFI zboot). |
| `devices` | Bus, PL011, PL031, fw_cfg (+DMA), CFI flash (NVRAM), PCI host (ECAM, MSI, MSI-X, INTx), PCIe root ports with native hot plug, NVMe, xHCI + USB HID (8 USB 2 + USB 3 pairs for passthrough, hot plugged; async transfers; isochronous TDs in batches, a few ahead), Intel HD Audio (controller + codec, streams moved by a timer thread). Hostile input only; `forbid(unsafe_code)`. |
| `video` | Hardware video, shared by VMM and `vroom video-helper`: their protocol (grants, epochs, resume state), H.264/HEVC NAL unit, access unit, parameter set parsing, picture order counts, VP9 frame headers, AV1 OBUs. Hostile input only; `forbid(unsafe_code)`. |
| `usb` | Passthrough: VMM/helper protocol, helper request checks, libusb (`dlopen`, helper only; root helper only from its own copy), host device listing (IOKit / sysfs), taking a device from host drivers and back, USB stick (Bulk-Only, SCSI) on a RAM disk (tests) or a Mac disk fd ("Use as a disk"), fake serial port and sound card (tests). |
| `virtio` | Split queues (hardened), MMIO and modern PCI transports, devices (blk, net, console, rng, gpu, input, fs, sound, media), a pausable worker per device. `forbid(unsafe_code)`. |
| `fs` | Shared folders host side: component-wise `openat(O_NOFOLLOW)` walks from a root fd, Linux errno mapping. FUSE server lives in `virtio`; serves Linux and virtio-win's virtiofs.exe, told apart by INIT flags. |
| `audio` | Sound helper: datagram protocol, ring buffer, CoreAudio (macOS) / ALSA (Linux, `dlopen`) play and record, WAV test files. |
| `gpu3d` | virglrenderer via `dlopen`, behind the `Renderer3d` trait (defined in `virtio`). The only GPU FFI. `remote`: same trait over a socket to the GPU process, and that process's loop. |
| `block` | raw, qcow2 (COW, compressed reads, backing chains with allow-list, discard/zero clusters with hole punching, folding a layer into its child), sync modes, swappable layers, flock image locks. VMDK reader (read-only) for VMware import. |
| `net` | Userspace NAT (smoltcp guest side, host sockets out), DHCP/DNS, ICMP echo, fragments; netd wire protocol. |
| `snapshot` | `Snapshottable` trait; snapshot dirs with per-blob SHA-256, atomic manifest. |
| `state` | VM state dir: one-VMM lock, snapshot list/delete, pruning unneeded disk layers. Shared by CLI and app. |
| `vmm` | Builds the board from config, runs vCPUs, PSCI, agent channel, display arbitration (ramfb vs virtio-gpu), snapshots, suspend, disk layering. |
| `sandbox` | Seatbelt profile (macOS), seccomp + Landlock (Linux). |
| `gui` | Slint app: VM list, settings editing the VM file in place (toml_edit, comments kept), New VM, `⌘K` palette, VM windows. All state changes through one UI-thread message queue. VM windows see pixels, send input. Short labels; detail in a tooltip: one `Tip` (`ui/controls.slint`) on Slint's `Tooltip` (hover delay, by the pointer, never takes a click), also shown under its control on keyboard focus. |
| `agent` / `proto` | Guest agent (Linux, Windows); framed protocol over a virtio-serial port. |
| `tools` | Guest tools host side: locating them (`Contents/Resources/guest-tools`, `target/guest-tools`), agent version from its binary, updates as agent frames, ready-made VMs (cloud-init seeds, Windows install sticks). |

## Board (aarch64)

QEMU-virt compatible map, so stock EDK2 (`edk2-aarch64-code.fd`) works:

| Address | |
|---|---|
| 0 | flash |
| 0x0800_0000 | GICv3 (+ GICv2m MSI frame, emulated if the backend lacks one) |
| 0x0900_0000 | UART; then RTC, fw_cfg |
| 0x0a00_0000 | virtio-mmio slots; then PCI ECAM/windows |
| 0x4000_0000 | RAM |
| 0x70_0000_0000 | virtio shared memory windows (GPU host-visible memory) |

- UEFI: ACPI (MADT, GTDT, SPCR, MCFG, DSDT with PCI and virtio devices) and SMBIOS via fw_cfg.
- Direct kernel boot: FDT.

### PCI hot plug

PCI VMs (UEFI, or `virtio_transport = "pci"`): 4 PCIe root ports after all other devices, so those keep their slots (`devices::pci::port`, Red Hat QEMU root port IDs 1b36:000c).

- Each port: bridge, one empty slot, native hot plug: attention button, power controller, indicators, presence detect, Data Link Layer Link Active reporting, MSI (or INTx) for slot events.
- Guest drivers: Linux pciehp (ACPI grants it via `_OSC`; device trees always do), Windows pci.sys. No ACPI hot plug; DSDT unchanged.
- Resources: EDK2's PciHotPlugInitDxe (stock build) reserves a 2 MiB window per port; Linux reassigns and pads hot plug bridges likewise; Windows keeps firmware's.
- ECAM: config cycle for bus ≠ 0 goes to the port whose secondary bus it is, reaching device 0 behind it while the slot has power. Slot device BARs mapped as on the root bus.
- In: `PciRoot::plug` puts a device in a free slot, raises Presence Detect Changed + Attention Button Pressed (as QEMU). Guest powers the slot, link up, scans.
- Out: `PciRoot::unplug` presses the attention button; guest releases and powers the slot off; that write removes the device (dropped once no lock is held). 15 s timeout (`hotplug::Hotplug`), then surprise removal (both guests cope).
- Command Completed raised per Slot Control write, except Windows' 32-bit status-clearing writes carrying Slot Control unchanged (answering those set Command Completed again at once; Windows kept clearing it).
- Guest reset: device stays in its slot (like a real card), reset to its plug-in state; one on its way out goes.
- Snapshots: port blob carries its device's. Save records adapter → slot (`net.hotplug` in the manifest); resume builds those devices in those slots before restoring; mismatched slot set refuses. Device on its way out: pulled before the save. Pre-port snapshots: ports empty.
- Used by virtio-net only (`ToVmm::NetAdd`, `ToVmm::NetRemove`). Ports take any virtio device (`Hotplug::add` takes a `VirtioDevice`): disks or a USB files share would work the same, with saves recording them like adapters.

## vCPU loop

- Each vCPU thread owns its hypervisor handle; commands: power-on, pause, save/restore state, reset, stop.
- MMIO exits → device bus. PSCI: in the VMM (HVF) or kernel (KVM). WFI: emulated with short sleeps bounded by the guest timer deadline.
- Pause is synchronous: every vCPU acknowledges, device workers park, disk I/O threads finish requests, before anything reads guest memory. That makes snapshots consistent.
- Disks (virtio-blk, NVMe): lone small request runs on the submitting vCPU; several in flight → a few I/O threads per disk; no bounce buffer. [PERFORMANCE.md](PERFORMANCE.md).

## Snapshots and suspend

Snapshot = directory: manifest (hashes), per-vCPU state, irqchip state, a blob per device, sparse RAM (64 KiB chunks + bitmap).

- Disks frozen at the same moment: top layer → read-only, new qcow2 overlay on it. Revert: fresh overlay on the frozen layer.
- Suspend = snapshot that ends the process; resume checks the disks didn't change meanwhile.
- Restore: devices before vCPUs (a vCPU runs as soon as its state is back).
- Nested-virtualization EL2 state: best effort.
- Disk layers: a tree under `<state>/disks`. `vroom snapshot delete/prune` (VM stopped) removes unreferenced layers, folds a layer needed by one child only into it (copy up clusters the child lacks, repoint its header): short chains.
- Locks: state dir locked by its VMM; images exclusive (writer) or shared (read-only layer); snapshot downgrades the frozen layer to shared.
- Shared folders: FUSE node table kept as paths, so it's in the snapshot; open files reopened by path on restore.
- Sound hands back in-flight buffers before saving.

GL (virgl) contexts (`virgl_state.rs`): host GPU objects can't be saved as is.

- GPU device follows each submitted command stream: live objects, latest of each setter; a setter dropped once a later one overwrites all it wrote (vrend's rules: sampler views clear later slots, sampler states don't).
- Snapshot reads every texture level and buffer back through virglrenderer.
- Restore: remake resources, write contents, make contexts, replay state, return the fenced commands the GPU had finished.
- GL context blobs not tracked: snapshots refuse while one is in use.
- Keeps the type a compositor gives a Vulkan window's blob (`PIPE_RESOURCE_SET_TYPE`), replays it before anything uses that blob.

Vulkan (Venus) contexts: saved by their render worker (`third_party/virglrenderer/0010-venus-snapshots.patch`, `vkr_snap.c`).

- Every command (virtqueue, ring or command stream) goes through one function noting what it looked up (decoder lookups), made and destroyed (context object table).
- Kept, as the guest encoded them: commands that made/destroyed objects, bound memory, wrote descriptors, recorded command buffers. Venus ids are the guest's: replay in a fresh context gives the same objects under the same ids.
- Dropped:
  - a command buffer's recording when it begins again or is reset (also via its pool)
  - a descriptor write fully overwritten later, unless a recording still holds it (bound while the old value was there: replay must record it the same way); a descriptor copy stops this for everything older
  - periodically, everything nothing alive needs. Object needed = alive or used by a needed command. Command needed = makes a needed object, destroys one (replay must too), or makes nothing and uses only needed objects. So a pipeline keeps its shader module even after the app destroyed it.
- Past 4M commands or 1 GiB kept after clean-up: context no longer followed, comes back lost.

Venus save (`RENDER_CONTEXT_OP_SAVE` from the GPU process proxy, from `Renderer3d::venus_save`):

1. Stop every context thread at a command boundary: dispatching threads hold a gate per command; a ring stores its head inside it, so the saved head is exactly what ran. A command blocked in a wait (`vkWaitVirtqueueSeqnoMESA`, `vkWaitForFences`) releases the gate while waiting; reruns from the start after restore.
2. Empty submission with a fence on each queue; wait for them.
3. Write into one shared memory object: the context's shared memory (rings, reply streams), kept commands, each device memory object's contents (host-visible from its shm; device-local bound to a buffer via a staging buffer), each image's contents (every level, layer, aspect, copied to staging with `VK_IMAGE_LAYOUT_GENERAL`: MoltenVK keeps no layouts, Metal has none; images get transfer usage added at creation where the format allows), fence and event states, timeline semaphore values, which binary semaphores remain signaled (followed from submissions), each ring's place and creation parameters, reply stream positions.
4. Threads not stopped in 5 s, GPU not done in 10 s, or context already failed: only rings written; context comes back lost.

Venus restore (VMM stores the save beside the GL state, with the context's blobs: how made, where mapped):

1. Make the context; remake its shared-memory blobs (blob id 0) with their ids.
2. `RENDER_CONTEXT_OP_RESTORE`: worker copies shm back, replays commands (replies off), fills memory and images, sets states, makes rings at their old head, alive.
3. Memory blobs (blob id = memory object id, so they export the new memory).
4. `RENDER_CONTEXT_OP_RESUME`: start rings.
5. Map every blob where the guest had it; then GL contexts.
6. Context the worker can't rebuild: plain shm for its memory blobs (guest pages stay backed), rings marked fatal. Mesa Venus stops that app ("ring fatal error"); rest of the guest continues.

## Several monitors

`[display] monitors` (1–4).

- Shared framebuffer: a slot per monitor. Each monitor's sink (`FbSink`) writes its slot and tells the viewer (`Mode`/`Damage` for the first, `HeadMode`/`HeadDamage` + monitor number for others). ramfb shares only the first.
- virtio-gpu: a scanout per sink. A resource can show on several scanouts at different places (X11: one framebuffer for all monitors): a flush goes to each scanout showing it, clipped to its part; a GL flush reads the union back once and hands each scanout its piece.
- GET_DISPLAY_INFO reports each scanout; a window closing unplugs its monitor (display event).
- Linux: one GPU, N scanouts.
- Windows: drivers drive one scanout each → one GPU per monitor, added after other devices (PCI slots don't move), numbered in the PCI subsystem id (0x1100 + n), which the agent reads.
  - Only the first has 3D: two 3D GPUs would need to share frames (Windows draws a monitor on one GPU, shows it on another); the D3D driver can't across renderers.
  - Others 2D. Windows' 2D driver clears "its framebuffer" in BAR 0 with stores the hypervisor can't decode: those GPUs have plain memory in BAR 0, registers in BAR 2. The 3D one too ([Direct3D state can't be saved](#direct3d-state-cant-be-saved)).
- Viewer sends each window's size (`HeadResize`, 0x0 on close) and pointer (`HeadPointer`).
- Agent service makes one layout: monitors side by side, top edges aligned (`SetLayout`, applied with xrandr, Mutter's DisplayConfig, or ChangeDisplaySettingsEx). Display thread maps a monitor's pointer onto that desktop for the tablet.
- Windows maps the tablet to its main monitor only: tablet there; elsewhere the agent moves the pointer (`Mouse`, SendInput).

## Drag and drop

Mac → guest:

1. Finder drag reaches the window as winit events (no position: app polls the mouse while files hover, moves the guest pointer).
2. App sends names (`ToVmm::DragEnter`) → VMM agent service (`Message::DragEnter`) → agent session side (`agent/src/drop.rs`) makes a staging folder.
3. Windows (`dnd_win.rs`): shows a window under the pointer, brought in front of the active app and composed before answering (Windows routes no click to a window before that), answers `DragReady { live }`. App presses the guest's left button: the window starts a shell drag (`SHDoDragDrop`) of the staged names, tracked by Windows as the pointer moves.
4. X11 (`dnd_x11.rs`): InputOnly window; agent is the XDND source (owns `XdndSelection`, offers staged files as `text/uri-list`).
5. Wayland: outside programs can't drag; files go to the Desktop.
6. Drop: app opens a socket pair, sends one end to the VMM (`ToVmm::Drop`, SCM_RIGHTS), writes the files into the other as agent frames; VMM forwards them, checked, from its own thread.
7. Guest says `DropDone`: app releases the button; the app under the pointer takes the files.
8. Leaving the window: `DragCancel`, Escape, button up.

Guest → Mac (`gui/src/dragout.rs`):

1. Mouse leaves the guest screen with the guest button held since a press on it: app asks once (`ToVmm::DragOut`, agent `DragOutQuery`).
2. Agent (`agent/src/dragout.rs`) puts a small window under the guest pointer (now at the screen edge) as a drop target: OLE `IDropTarget` (Windows), `XdndAware` window (X11). Nudges the pointer so the ongoing drag finds it.
3. Drag carries files (CF_HDROP, or `text/uri-list` with file URIs): agent learns paths, answers names (`DragOutItems`, `ToViewer::DragOutItems`).
4. App starts an AppKit drag of file promises (`dragout_mac.rs`) from the VM window while the Mac button is down; abandons the guest drag (Escape, button up: guest drops nothing).
5. Drop taken: AppKit asks each promise where to write; per item the app sends a pipe (`ToVmm::DragOutGet`, SCM_RIGHTS); VMM asks the guest (`DragOutGet` + its own stream number), passes DropEntry, DropData, DropEnd frames on, checked, with the drag id. App writes where the promise says.
6. All written (or no drop): app sends `DragOut` with `end`; agent forgets the paths.

## Guest tools

Build:

- `scripts/make-guest-tools.sh` → `target/guest-tools/` (`linux/`, `windows/`, `vroom-tools.img` = FAT32 disk with both). `scripts/app.sh` copies it into the app. `vroom_tools::dir()` finds it.
- Secure Boot firmware likewise: `scripts/make-firmware.sh` → `target/firmware/`; app `Resources/firmware/`; `firmware.rs` looks next to the running program.

Versions:

- `agent/build.rs` makes `0.1.0+348.e314084680` (crate version + commits on the branch + commit), embedded after `\0VROOM_AGENT_VERSION=`.
- Agent hello: `vroom-agent 0.1.0+348.e314084680 linux`.
- Older = lower crate version, then fewer commits (`vroom_proto::version`).
- App reads the tools' version from the agent binary it carries.

Update (`gui/src/console/tools.rs`): window learns the guest agent from the VMM status line "guest agent connected (...)" and its features. Tools button shows when the agent is older or none came within 90 s.

- Agent with `features::UPDATE`: over the agent channel.
  - App writes agent frames into a pipe (`vroom_tools::bundle`: `UpdateBegin` with each file's path, size, SHA-256; then `UpdateData`, ≤256 KiB each), sends it with `ToVmm::AgentUpdate`. VMM `forward_update` passes exactly what was announced.
  - Agent service (`agent/src/update.rs`) stages and checks files, installs (Linux: `vroom-agent-update.path` sees `ready`, runs the bundle's `install-agent.sh --update`; Windows: `winupdate.rs`), answers `UpdateDone`.
  - Service restarts; session helpers see their file changed when the service goes, start the new one (exec on Linux, new process on Windows). New agent's hello ends it.
- Otherwise: `vroom-tools.img` plugged as read-only USB stick (`ToVmm::UsbDisk` with the file's fd, as for a Mac disk); app says what to run (Linux: command also on the Mac clipboard). Up-to-date agent hello: stick removed.

Ready-made VMs (`vroom_tools::ready`; `vroom new --ready`, New VM sheet): tools at install time. Ubuntu: on the cloud-init seed (`seed.rs`), installed by `runcmd`. Windows: on the install stick (`winstick.rs`), drivers added in the specialize pass, `vroom-setup.cmd` at first logon.

## Coherence

Start: app asks for a screen the size of the Mac screen's free area (`ToVmm::Resize`), sends `ToVmm::Coherence(true)` → VMM agent service `Message::Coherence` → guest service → session helper (`agent/src/coherence.rs`; `coh_win.rs`, `coh_x11.rs`).

Window tracking: on desktop change events, ≤ every 10 ms; every 500 ms anyway (lost events).

- Windows:
  - `SetWinEventHook` (out of context, own thread + message loop): foreground, menus, move/size, minimize; top-level windows created, destroyed, shown, hidden, moved, renamed, cloaked. Callback sets a flag, wakes the session loop.
  - Hides the taskbar, whole screen = work area.
  - Walks windows front to back with `GetTopWindow`/`GW_HWNDNEXT` (`EnumWindows` misses Store apps and Start menu); per-monitor DPI aware (real pixels).
- X11:
  - Second connection selects structure and property events on root and every managed window.
  - List: `_NET_CLIENT_LIST` in the order of root's children (WM frames), plus override-redirect windows above the lowest frame (menus, tooltips).
  - Visible area: client area + `_NET_FRAME_EXTENTS` − `_GTK_FRAME_EXTENTS` (GTK shadows). Other workspaces, desktop, docks left out.
  - Actions: taskbar client messages (`_NET_ACTIVE_WINDOW`, `WM_CHANGE_STATE`, `_NET_CLOSE_WINDOW`, `_NET_MOVERESIZE_WINDOW` with static gravity).
  - Ubuntu dock set to hide (`gsettings`, restored after; old value noted in `$XDG_RUNTIME_DIR` for a helper that died).
  - GNOME top bar: only hideable from inside GNOME Shell. Tools install an extension (`examples/linux/gnome-shell`, `vroom-coherence@vroom`; one file for GNOME 42–44, one for 45+) removing the top bar from struts and hiding it while enabled. Helper runs `gnome-extensions enable` on Coherence on, `disable` on off (and at start). GNOME Shell finds new extensions only at start: works from the login after install.
  - Wayland: helper answers `CoherenceNo` with the reason.

Messages:

- `Message::Windows` on change: id (window handle or X window), visible area, flags (in front, minimized, app window, maximized), title.
- `WindowIcon` (32x32 RGBA) for app windows when new or changed (checked every 2 s). Windows: draws the icon (`WM_GETICON`, class icon, or its program's) twice, on black and white → alpha for any icon kind. X11: `_NET_WM_ICON` size nearest 32.
- Store app windows (`ApplicationFrameHost`, or a packaged program) have no icon and their program none: helper finds the package (`GetPackageFullName`, `GetPackagePathByFullName`), the app's `AppxManifest.xml` entry (by program), its `Square44x44Logo`, the unplated file nearest 32 (`appx.rs`), PNG decoded in the guest.
- `WindowDo` back: activate, minimize, close (`SC_CLOSE` / `_NET_CLOSE_WINDOW`), move (to visible bounds, borders added back). Only windows of the last list are touched, in VMM and agent.

`StartApp` (`apps.rs`): rereads the menu on a thread, starts the best name match: name (whole, then prefix, then word prefix) before other names; ties: terminal apps last, then shorter name.

- Windows (`apps_win.rs`): `.lnk` files of both Start menu folders (shell names, localized via `desktop.ini`), then `shell:AppsFolder` apps no shortcut names (Store apps by AppUserModelID; via `IShellItem`, `IEnumShellItems`). Opened with `ShellExecute` (the `.lnk` or `shell:AppsFolder\<id>`). Windows may open it behind the front window: helper waits ≤10 s for its first window, brings it forward. No match and a plain name: old way (Windows key, typed name, Enter).
- Linux (`apps_xdg.rs`): desktop entries of XDG data dirs (user's, `XDG_DATA_DIRS`, Flatpak, Snap), precedence by desktop file id; skips NoDisplay, Hidden, OnlyShowIn/NotShowIn, missing TryExec. Names in session language (`LC_ALL`, `LC_MESSAGES`, `LANG`); other names from GenericName, Keywords, file name. Started with `gio launch <file>` (GLib 2.68+); on failure the Exec line split per spec quoting, field codes removed (no files), run directly in its own process group from home.

`Apps` while on: name + 32x32 icon, ≤512, sorted; read on its own thread at start and every minute, sent when changed. Icons: Windows `SHGetFileInfo` for shortcuts, package logo for Store apps; Linux icon themes (user's GTK theme, Yaru, Adwaita, hicolor…, PNG only, nearest 32) or the named file. ~3 s on Windows (shell draws every icon), 70 ms on Ubuntu.

VMM → app: list coalesced (`ToViewer::Windows`, like damage: busy guest = one queued list); icons coalesced per window (≤128 waiting).

App (`gui/src/console/coherence.rs`, `ui/coherence.slint`):

- One frameless Slint window per on-screen guest window: our strip on top, then the guest window's part of the shared framebuffer, copied where damage touches.
- Guest pixels ↔ Mac points via `Map` (free area + guest mode, which may be smaller than asked: Windows' driver caps it). Pointer events map back through the picture's own size.
- Moves: guest list places Mac windows; a user-moved/resized Mac window sends `WindowDo::Move` (kept on the guest screen), ignores guest reports for 0.5 s.
- Order applied with `orderFront:` while vroom is active; otherwise new windows open behind other apps'.
- Guest's front window gets the keyboard while one of ours has it on the Mac.
- Bar (`CoherenceBar`): app windows with icons, Windows key for Start, app field (keys stay in the bar while focused). Typing shows the 3 best guest-list matches (`suggest`) in place of window buttons; Enter sends the pick, or the text if none fits.
- Every second: bar's screen and its free area checked; changed (bar dragged, Dock moved, screen unplugged) → guest screen moves there: new area, resize, bar at bottom.
- Leave: `Coherence(false)` + VM window size. Viewer gone (app crash): VMM turns Coherence off; agent restores the taskbar when its session ends or it restarts. A session helper replaced by another (started twice) is told to leave Coherence first.

## Video decoding

`video_decode = true` (app: Settings, Display, Hardware video decoding): a virtio-media device (`virtio/src/devices/media`).

- V4L2 over virtio: guest kernel forwards each `/dev/videoN` ioctl on the command queue.
- Device = V4L2 stateful decoder: H.264, HEVC, VP9 or AV1 in (OUTPUT; codecs the helper says this Mac decodes), NV12 or P010 out (CAPTURE); source-change event, LAST buffer, decoder STOP/START per the V4L2 decoder interface.
- Buffers in one shared memory window (region 0, 1 GiB, mapped where the guest puts it; `vmm/src/video.rs`); guest maps them with virtio-media's MMAP command.

Helper `vroom video-helper` (`cli/src/video.rs`, `cli/src/vt.rs`):

- At start VMM hands it the window's shm object (it maps it); helper answers with the codecs VideoToolbox decodes here.
- VMM copies each bitstream buffer out of the window and sends it. Helper parses (`video/src/nal.rs`: H.264/HEVC access units, parameter sets; `video/src/vpx.rs`: VP9 frame headers, AV1 OBUs), makes a VideoToolbox session per stream format (new one on size, depth or key-frame format change, announced to the VMM before its first picture), decodes.
- Display order: VideoToolbox outputs H.264/HEVC in decode order; helper computes each picture's order count from its slice header (per the standards), holds as many as the SPS allows reordering (`video/src/poc.rs`).

Pictures don't cross the socket:

- VMM grants each guest picture buffer the helper may fill (offset, length, NV12/P010) under an epoch, bumped when buffers change (size change, seek, stream off).
- Helper writes from VideoToolbox's buffer straight into the granted guest buffer (10 → 8 bit if the guest wanted NV12), says which.
- VMM checks it's the oldest grant of the current epoch and stream size, returns it with the timestamp of its bitstream buffer.
- Bitstream buffers go back to the guest as soon as copied out.

Display delay 0 (`V4L2_CID_MPEG_VIDEO_DEC_DISPLAY_DELAY`, enabled): decode order, no reordering, each picture out as soon as VideoToolbox has it; a failed picture returns with the error flag. For our VA-API driver.

Snapshot:

- Pause first answers every command the guest waits on (its driver gives up after 5 s).
- Saved: each decoder's V4L2 side (formats, buffers and their place in the window, queues) + helper resume state (helper sends parameter sets, VP9 key frame header or AV1 sequence header on change).
- Restore: new helper opens the stream from that state, decoder resumes at the next key frame. Bitstream buffers that hadn't reached the helper go back to the guest (window contents not saved).

### The guest's VA-API driver

Browsers decode via VA-API, which hands the driver parsed pictures. `scripts/guest/vaapi/vroom_drv_video.c` (libva driver, C, built in the guest) turns them back into a bitstream for the V4L2 decoder, one buffer per picture, decode order:

| Codec | |
|---|---|
| H.264 | SPS, PPS written from VA-API picture parameters and scaling lists, sent on change. PPS reference counts left to slices read from slice headers. SPS says no reordering: VideoToolbox outputs each picture at once |
| HEVC | VPS, SPS, PPS likewise. VA-API omits the SPS reference picture sets slice headers point to: each slice header rewritten to carry its picture's set (from VA-API's reference list); rest of header and slice data copied |
| VP9 | frame as is (VA-API passes all of it) |

Surfaces and dma-bufs:

- Each VA surface has its own picture copy, filled when its decoded picture returns (matched by the timestamp the driver gave the bitstream buffer).
- dma-buf players (Firefox): a surface also gets two GBM buffers on the virtio-gpu render node (luma, chroma), filled via their guest pages and one transfer each on export.
- Chrome exports each surface once, at creation, before decode, and keeps showing that dma-buf (a GPU decoder writes in place). So a surface exported while empty is marked; from then on `vaEndPicture` waits for its picture and fills its buffers before returning (Chrome shows it once that returns). Firefox exports after decoding: old behaviour.
- Stream format set again (HEVC seek): Chrome destroys surfaces, keeps the dma-bufs, makes new surfaces from them (`DRM_PRIME` memory, naming only the first plane's buffer). A destroyed live surface's two buffers are kept 5 s (≤32); such a surface gets them back by its luma buffer handle. Other dma-bufs refused.

Chrome on X11 (DRI3 pixmaps take only BGRA):

- Asks for video processing (`VAProfileNone`, `VAEntrypointVideoProc`): NV12/P010 → BGRA.
- Makes BGRA surfaces with `vaCreateSurfaces`, exports them, then imports decoded and BGRA surfaces back as dma-bufs. An import becomes a surface standing for the exporter, found again by GEM handle on each use (Chrome may have remade it from the same buffers).
- `vaEndPicture` on a processing context converts on the CPU (NEON, 16 pixels × 2 rows, nearest-pixel scaling) straight into the BGRA buffer's guest pages, one transfer: Chrome doesn't sync before showing.
- A decoded surface feeding only processing no longer fills its own GBM buffers.
- Colour matrix: from VP9 key frames; H.264/HEVC don't pass theirs through VA-API: guessed from size and bit depth.
- Transfers to the host carry no stride (kernel refuses one for these non-blob resources); host reads rows packed, as GBM lays them out.

## Video encoding

`video_encode = true` (app: Settings, Display, Hardware video encoding): a second virtio-media device, the encoder (`virtio/src/devices/media/enc.rs`). Same transport, command and event queues; own buffer window (1 GiB, only touched pages are real memory); V4L2 stateful encoder.

- OUTPUT: raw NV12, or P010 for HEVC Main 10, at the guest's own size (rows of frame width, chroma right after the last row). CAPTURE: H.264 or HEVC, formats the helper said it encodes.
- Frame intervals from `S_PARM`; the rest from controls (`G/S/TRY_EXT_CTRLS`, `QUERY_EXT_CTRL`, old single-control ioctls): bitrate, peak bitrate, rate control (VBR, CBR, constant quality), GOP size and H.264 I-frame period, force key frame, profile, level, entropy coding.
- B-frames: 0 only. Parameter sets joined with each key frame.
- `ENCODER_CMD` STOP drains: every queued frame comes back coded, then an empty buffer with LAST and the EOS event.

Helper `vroom video-helper --encode` (`cli/src/venc.rs`, `cli/src/vtenc.rs`), with the encoder's window:

- Queues start: device sends `EncConfig` (codec, raw format and layout, size, frame rate, rate control, bitrate, key frame interval, profile, flags). Changed rates → running session; anything else → new VideoToolbox session with a key frame.
- Each queued raw frame → `Encode` message naming its place in the window; helper copies it into a compression session pixel buffer (IOSurface); guest gets its buffer back.
- No frames over the socket: coded frames into CAPTURE buffers the device granted (same grants and epochs as decode).
- VideoToolbox gives length-prefixed NAL units, parameter sets apart; helper writes Annex B with parameter sets before each key frame (`video/src/nal.rs`).
- No reordering: coded frames in input order; device gives each its frame's time, KEYFRAME or PFRAME flag, size.
- CBR → VideoToolbox low-latency mode (real time, FaceTime's rate control; one frame in, one out; ~5 ms for 720p on M3 Max), as calls want. Other modes: ordinary encoder, pipelined for throughput.
- Key frame on request = frame option. GOP size = VideoToolbox key frame interval (0: only the first frame and requested ones).

Snapshot:

- Kept: each encoder's V4L2 side (formats, controls, buffers, queues).
- Lost (window not saved): raw frames still queued, at the helper, or coded but not returned. After restore: raw buffers back to the guest; per lost frame, an empty coded buffer with the error flag and that frame's time (a waiting player moves on).
- New helper gets the session settings; stream resumes with a key frame.

### The VA-API driver's encoder

`vroom_drv_video.c` offers `VAEntrypointEncSlice` for H.264 (Constrained Baseline, Main, High) and HEVC (Main, Main 10) when the VM has the encoder.

- VA-API encoders assume hardware coding player-described slices with player-written headers (packed headers). Ours codes whole frames with its own headers: driver reports no packed headers (Chrome, FFmpeg then leave headers to the driver), 1 reference frame, no B-frames.
- Mapping:

  | VA-API | encoder |
  |---|---|
  | sequence parameters | picture size (incl. H.264 frame cropping), bitrate, frame rate (VUI timing) |
  | misc parameters | rate control (target, peak bitrate), frame rate |
  | picture parameters | coded buffer to fill; IDR → forced key frame |
  | config rate control mode | CBR, VBR, or CQP as constant quality from the key frames' QP |

- `vaEndPicture`: input surface (our copy; Chrome fills it via `vaDeriveImage`, FFmpeg via `vaDeriveImage` or `vaPutImage`) copied into a V4L2 raw buffer, queued; coded frame lands in the VA coded buffer on return.
- `vaSyncSurface` (input), `vaSyncBuffer`, `vaMapBuffer` (coded buffer) wait for it.
- Rate changes → control changes only when different. Reference surfaces and player slice headers ignored.

## 3D

`gpu = "virtio3d"` adds three protocols to virtio-gpu:

| Protocol | Guest | Host |
|---|---|---|
| Venus | Vulkan (Mesa venus driver) serializes commands | virglrenderer replays on MoltenVK |
| virgl | OpenGL (Mesa virgl driver) sends Gallium commands | vrend replays on macOS OpenGL 4.1 (CGL), or with `opengl_host = "angle"` on OpenGL ES from ANGLE's Metal backend |
| Neptune | Windows viogpu3d driver sends Direct3D 11 calls | virglrenderer (UTM's fork) replays on DXMT (Direct3D on Metal) |

- All capsets offered; Mesa picks per API.
- Windows: Neptune for Direct3D, Venus for Vulkan and for OpenGL via Zink, virgl for OpenGL without Vulkan.

Windows drivers, one Mesa build (`third_party/mesa/`, `scripts/build-windows-gl.sh`):

- `vroom_vk_*.dll`: Venus as its own Vulkan driver (D3DKMT, no Direct3D runtime).
  - viogpu3d in WDDM 2 mode (GpuMmu): render packets carry no allocation lists.
  - A mappable blob's window in the host-visible region is placed by the kernel driver, mapped into the process from its RES_INFO escape (D3DKMTLock would give staging pages the host never sees). Maps/unmaps by resource id.
  - Command streams in D3DKMT render packets; fences = CPU events the kernel driver signals when a packet is done.
- `vroom_gl_*.dll`: Mesa OpenGL, Zink on that Vulkan first, else virgl with our winsys: a virgl context via viogpu3d escapes, command streams in render packets, resources = the driver's 3D allocations (guest pages go to the host once they have a GPU address and are resident).

Windows display:

- Compositor runs on Neptune, can't see Venus or vrend images: frames come back to the guest, drawn with GDI (Vulkan: Mesa CPU swapchain; virgl: read-back).
- 3D display device off (vroom does it before a save): kernel driver drops contexts, unmaps windows. Our Venus driver puts plain memory where a window was (exception handler), marks itself lost: waits end, submissions and presents fail `VK_ERROR_DEVICE_LOST`; Zink OpenGL keeps running without drawing.

Render server:

- Venus and Neptune go through virglrenderer's proxy to its render server. Ours (`gpu3d/src/server.rs`, `vroom gpu-server`) starts one `vroom gpu-worker` per guest context, running it with virglrenderer's own worker code (patch 0009 exports it), sending buffers back as shm it creates.
- GL contexts stay in the renderer: vrend shares one GL context group between them.
- Host-visible buffers mapped into the guest's shared memory window with the hypervisor.

GPU process (`gpu3d/src/remote.rs`, `vroom gpu-helper`; [SECURITY.md](SECURITY.md#the-gpu-process)):

- VMM virtio-gpu talks to a `Remote`: same `Renderer3d` trait as in-process `Virgl`, over a Unix socket. One call at a time, each answered, in worker order: device code can't tell the difference.
- Second socket: a byte when fences may be done (worker's backend fd).
- With 3D, guest RAM is shared memory (`MmapRegion::new_shared`) the GPU process maps too; guest buffers sent as (region, offset, length), checked against its mapping.
- Reset: new zeroed RAM objects; `GuestMemory::generation` changes; next call naming guest memory sends the new ones first.
- Guest-mapped buffers (Venus, Neptune) come back as shm fds the VMM maps.
- Read-back frames land in a shared frame buffer the VMM made (over the socket cost a third of glmark2's score; now within a few % of in-process).
- `VROOM_GPU_IN_PROCESS=1`: all in the VMM.

GL (`gpu3d/src/gl.rs`):

- We create the CGL or EGL contexts virglrenderer asks for (one share group, no surfaces), keep the right one bound on whichever thread calls (setup thread, then GPU worker, which changes on device reset).
- GL fences: no completion thread (ANGLE serializes a share group's contexts; a thread blocked on a fence would stall submits). Worker polls every 250 µs while any is pending. At 1 ms, frames no longer waiting for the GPU waited for fences instead: glmark2 light scenes capped near 1,200 fps.
- Render server contexts (Vulkan, Direct3D): always a fence thread in the GPU process, woken by their worker (`0013-proxy-fence-thread-always.patch`). Polled at 1 ms like GL: each Zink frame waited 0.5 ms more on average; glmark2 on Zink 547 vs 963.
- GL scanout copied back on flush: from our own context into a GPU pixel buffer, shown when the copy lands while the worker continues. macOS, VMM started by the app: GL scanouts from IOSurfaces instead ([below](#the-screen-straight-from-the-gpu-macos)).
- Windows Direct3D frames: blobs in render server shm the guest never maps; a flush reads them via a read-only mapping made on first use.
- Windows lists only the modes a monitor offered at plug-in, and viogpu3d reports a plug only when a scanout's enabled flag changes. So once a Neptune context exists, a resize reports scanout 0 disabled, then enabled at the new size (checked and retried until Windows shows it); then the agent is asked for the mode.
- IMMUTABLE buffers/textures: guest creates them without data and fills them right after with an update. D3D forbids writing those, DXMT orders nothing on them: a copy or draw could read one first (Explorer's glyph atlas lost its white texel, copied from a 1x1 IMMUTABLE texture; text and boxes went missing). Our virglrenderer patch creates them DEFAULT (`third_party/virglrenderer/`).

Viewer reattach:

- The VMM a window starts also listens on `$TMPDIR/vroom/<hash of the state dir>.sock` (0600, dir 0700; state dir's `viewer` file names it; socket paths too short for state dirs).
- Window gone without suspend request (app crash): VM keeps running. Next app connects, gets the framebuffer and what it missed (mode, pointer, state, agent), then as usual.
- One viewer at a time. No viewer 5 min: VMM suspends.
- State lock holds the VMM pid: app can stop a hung one (only a process whose executable is `vroom`).

### Direct3D state can't be saved

"Parking". Suspend or snapshot of a Windows guest using Direct3D:

1. Guest agent turns the 3D display device off (SetupAPI, like Device Manager).
2. VMM waits until the guest has no D3D context left and the agent confirms.
3. Windows settles on its basic display for 3 s.
4. Save.
5. Device back on: after a snapshot, and whenever an agent connects (resumed or killed VM gets it back).

What makes it work:

- Kernel driver stops before Windows has destroyed every process's D3D device, and destroys the rest when it can no longer tell the host. Contexts still present 3 s after the agent said "off" belong to the stopped driver: GPU worker drops them (driver resets the GPU on restart anyway). virglrenderer patch 0017 sets "fatal" in each ring's status when a Neptune context goes: a process waiting on one gives up at once.
- Driver unmaps its shared memory (the UMD's rings) from every process while their UMD threads still use it: each D3D process crashed at its next draw. `vroom_d3d.dll` (`scripts/guest/d3d/`), registered by the agent in place of the driver's UMD and loading it, adds an exception handler: UMD-code access to vanished memory (freed, or sometimes left reserved) gets zeroed memory, "fatal" where a ring status would be. UMD fails the call, Windows reports device removed (as after any GPU loss). Unhandled fault: a line in the process's `%TEMP%\vroom_d3d.log`.
- Stopped driver hands Windows' basic display a 1024x768 framebuffer at BAR 0, which was the virtio registers: basic display wrote pixels there (garbled registers; once an undecodable access stopped a CPU, the save held a dead CPU, resumed guest hung). Now Windows' 3D GPU: 8 MiB plain memory in BAR 0 (like the extra GPUs), host window in BAR 2, registers in BAR 4. Older saves keep the old layout (snapshot meta `gpu.bar0_vram`).
- A CPU stopped on a fault isn't saved: save fails.
- Stopped driver leaves blobs: host blobs no context uses are left out of the save.
- Host-visible window with no buffer mapped: backed by reserved anonymous memory, so a guest touching a returned buffer reads memory instead of stopping the VM.

Why not save Direct3D (studied Oct 2026):

- Neptune forwards D3D11, D3D12, DXGI COM calls (~800 methods, 107 create objects, 195 object types; 142 on the D3D11 device context alone) over rings several guest threads share.
- Venus snapshots (patch 0010, 3000 lines) replay creation calls and copy memory back. Neptune would need: every creation call with data (resources with initial data, shaders, states, views, swapchains, fences, cross-process shared handles: the compositor opens every app's surfaces), immediate context bound state (no explicit state: read back with Get*), texture contents via staging (multisampled and depth can't: need a resolve or shader), deferred contexts and command lists, queries and fences with in-flight values, D3D12 on top. All in virglrenderer C against DXMT behaviour, guest UMD ids kept.
- Months of work, long tail: parking stays.

### macOS GL and Vulkan gaps

- GL on: 2D resources (dumb buffers) live in virglrenderer too, as in QEMU: Xorg glamor draws into them with GL; a context touching a resource virglrenderer doesn't know is broken for good (every window black). GL off: plain CPU buffers.
- HVF maps at the host's 16 KiB by default; 4 KiB-page guests place GPU buffers at 4 KiB steps. With 3D the VM asks for 4 KiB stage-2 pages (`_hv_vm_config_set_ipa_granule`, private on macOS 15, public from 26); GPU mappings round to that.
- MoltenVK lacks `VK_KHR_external_memory_fd` (Mesa requires it): our virglrenderer patch advertises it and shares mappings by pointer (`third_party/virglrenderer/`).
- virglrenderer logs through our logger (`virgl:` lines).

vrend on macOS OpenGL 4.1, patches:

| Patch | Fixes |
|---|---|
| (two) | multisample textures without `glTexStorage*Multisample` (GL 4.3); shaders at host GLSL 4.10, not older + `#extension` lines macOS rejects |
| 0011 | texture clears without `glClearTexSubImage` (GL 4.4): uploads the guest's texel over the box, 4 MiB of rows at a time, via vrend's normal upload. Without it guest Mesa 25.2 clears through a surface the host never saw (handle 0), vrend errors the whole context; Xwayland under GNOME Wayland drew nothing (glamor clears pixmaps this way) |
| 0014 | real sample count (4) on macOS GL. UTM's fork reported 1 (Mesa fakes MSAA), from before multisample textures worked on GL 4.1; 1 sample left guest ES 3.0 below spec, and ANGLE (so Chrome's GPU process) refused ES 3.0. `opengl_host = "angle"`: still 1 (ANGLE ES 3.0 has no multisample textures) |
| 0015 | vrend pauses transform feedback after each draw, ends it only when the guest drops its buffers; macOS GL crashed in `glEndTransformFeedback` if the program it began with was unbound (any GL app using transform feedback, at its end: GPU process died). Binds that program again for the end, or leaves the object to the context's end if the program is gone |

Venus decoder makes up for MoltenVK (patches 0007, 0008, 0012):

- `nullDescriptor`: guest may leave descriptors or vertex buffers null. Per device, on first need: 64 KiB read buffer, write buffer, 1x1 images for sampling and storage, their views; zeroed once on the device's first queue. Null handles in descriptor writes, push descriptors, vertex buffer binds point there.
- `VK_EXT_depth_clip_enable`: depth clip off → depth clamp.
- Sync fd import into semaphores: `vkImportSemaphoreResourceMESA` (guest waited for the fd; mark signaled) → empty submission signaling it; `vkWaitSemaphoreResourceMESA` → one waiting on it. Guest Venus needs this for `VK_KHR_swapchain` and `synchronization2`.
- Images/buffers the guest may export as dma-bufs see only host-visible memory types (their memory is shm imported as host memory; device-local can't be). Guest picking device-local anyway gets host-visible under it (never maps it).
- 0012: no host image copy for depth/stencil formats. MoltenVK offers it but keeps depth images in private Metal storage → no memory type at all. Zink asks for it on every texture it may CPU-copy into: every depth texture failed.
- Zink only (instance says "mesa zink"): transform feedback; packed 10-bit vertex formats and RGB32 texel buffers mapped to Metal's nearest. These lift Zink to GL 4.4.

Transform feedback in the vertex shader (0016, `vkr_xfb.c`):

- Vertex shader module with `Xfb` execution mode gets a second host module: before each return it stores captured outputs (`XfbBuffer`, `XfbStride`, `Offset` decorations) via buffer device addresses from 48 bytes of push constants at offset 256.
- Every pipeline layout gets room (vertex range → 304 bytes; a layout already using that range gets none, its pipelines don't capture).
- Zink devices: `bufferDeviceAddress`, `shaderInt64`, vertex stores on; xfb buffers get device address usage; every allocation the device address flag.
- Each command buffer tracks bound buffers (address, size), what was written to each, bound pipeline topology, xfb state. Before each draw with such a pipeline: push where vertices go, or zeros (nothing to write).
- Vertex n of instance i → slot i * count + n: matches xfb order for non-indexed point, line, triangle lists only; other draws capture nothing.
- Slots capped to what fits in every buffer, whole primitives: never writes past a bound range.
- Counter buffers (pause/resume across render passes, `glDrawTransformFeedback`) and query pools for xfb and pipeline statistics (`GL_PRIMITIVES_GENERATED`; MoltenVK has neither): kept by the host at record time; correct when command buffers run in record order (Zink does).
- Before: a Zink app asking for `GL_PRIMITIVES_GENERATED` got a refused query pool, its context failed.
- Apps without xfb: unchanged within noise (glmark2 on Zink, 3 runs each: terrain 270 → 260 fps, refract 596 → 637).

Vulkan window to screen:

- Guest compositor imports the frame's dma-buf with GL (virgl, `PIPE_RESOURCE_SET_TYPE`).
- macOS GL (no EGL, no memory objects): vrend makes its own texture, maps the Venus blob's shm read-only (layout checked against size); first time each command stream samples, blits or copies from it, the frame goes into the texture.
- BGRA frames ≥0.6 MP: on the GPU (`0030-vrend-venus-frames-through-an-iosurface.patch`, `vrend_shm_surface.m`): Metal takes the shm as a buffer (`newBufferWithBytesNoCopy`), blits into an IOSurface; GL sees it as a rectangle texture (CGL takes IOSurfaces only as rectangle textures, BGRA only), copies into vrend's texture.
- Smaller/other frames: `glTexSubImage2D`. Metal round trip costs the GL thread 0.2–0.3 ms at any size; CPU copy 0.45 ms at 1280x800, 1.8 ms at 2560x1600 (Apple's driver retiles on the CPU).
- `VROOM_SHM_COPY=1`: always CPU copy.
- No single shared IOSurface (zero copy) because: under Xorg the guest Vulkan driver presents via a "prime blit" into a linear exported buffer whose layout isn't known when its memory is made (can't be an IOSurface); and GL can't sample an IOSurface as the 2D texture guest shaders expect. A GPU copy stays either way; this way nothing new crosses processes.
- ANGLE: UTM's path (Metal texture on the same memory, no copy).
- Guest context gone: commands still waiting on its fences go back to the guest (virglrenderer drops those fences).

### The screen straight from the GPU (macOS)

Before: each GL frame copied 4× on the CPU (GPU process readback mapped into the VMM frame buffer; VMM copy into its shadow and the shared frame buffer; app RGBA conversion; UI toolkit texture upload). At 2560x1600: ~1 core in the app, ½ in the VMM, half the frame rate full screen.

Now, when the app starts the VMM (not on reattach):

- Mach ports:
  - App makes a receive port, starts `vroom run` with a send right as its registered port (`mach_ports_register` around the spawn, `vroom_ipc::mach`): no bootstrap name, nothing others can look up.
  - VMM takes it before starting anything; starts the GPU process the same way with its own port.
  - Needs posix_spawn (fork+exec gives a right to another port): the GPU process now gets its fence socket and guest RAM over its call socket (stdin) instead of inheriting them.
- Frames:
  - App sends `ToVmm::Surfaces(true)` (message 110).
  - First monitor's GL scanout: a flush asks the GPU process to blit it into one of 3 monitor-sized IOSurfaces (`gl.rs`, `Present`; on the GPU, alpha forced opaque), made on size change, sent to the VMM as mach ports with `[head, generation, slot, width, height]`.
  - VMM checks the sender is its GPU process and numbers in range; passes each port to the app unopened (its sandbox has no IOSurface).
  - Blit done: VMM sends `ToViewer::Surface` (message 110: generation, slot, changed rect).
  - ≤240 frames/s into surfaces; in-between frames skipped (the last always goes); guest never waits.
- App (`console/surfaces.rs`):
  - Checks each surface's sender (VMM pid, from the kernel), size, format; shows the latest in a CALayer over the screen's place in the window.
  - Anything of the app's drawn over the screen (menus, messages, pause veil, fullscreen bar, a drop): hides the layer, asks for pixels (`Surfaces(false)`), first copying the last surface into its own picture (no stale flash); VMM then reads the next frame back whole.
  - Thumbnails, `VROOM_SHOT`: read the surface on the CPU.
  - Pixels instead: Coherence, other monitors, reattached apps. `VROOM_NO_SURFACES=1` (app env): off.
- Windows Direct3D scanouts: blobs read by the VMM, not GL scanouts: still pixels.
