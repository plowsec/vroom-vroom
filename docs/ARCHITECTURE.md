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
   └─ optional: vroom usb-host (one per [[usb]] device), own sandbox,
      stream socket: transfer requests in, results and plug events out
```

The GUI never touches the guest: it shows a framebuffer the VMM shares
with it and forwards input. Closing it makes the VMM suspend the VM.

`vroom-netd` is the only privileged piece. It owns vmnet (macOS) or
tap+bridge (Linux) and hands each VMM an `AF_UNIX` datagram socket carrying
Ethernet frames, after policy checks (allow-listed interfaces, per-user
limits, source-MAC anti-spoofing). The VMM never holds an interface.

## Layers

| Crate | Role |
|---|---|
| `hv` | `Hypervisor` / `Vm` / `Vcpu` traits. HVF backend (in-kernel GICv3, vtimer, EL2 for nested), KVM arm64 backend (vGICv3 with save/restore, in-kernel PSCI). Exits are decoded into `VcpuExit` (MMIO, hypercall, PSCI events). |
| `memory` | Guest RAM as guarded mmaps; every access bounds-checked (`GuestMemory`). |
| `arch` | Board layout (QEMU-virt compatible), FDT, ACPI tables (own AML encoder), SMBIOS, kernel image loader (Image, gzip, EFI zboot). |
| `devices` | Bus, PL011, PL031, fw_cfg (+DMA), CFI flash (NVRAM), PCI host (ECAM, MSI, MSI-X, INTx), NVMe, xHCI + USB HID (passthrough devices get a USB 2 and a USB 3 root port each and asynchronous transfers), Intel HD Audio (controller + codec, streams moved by a timer thread). Parses hostile input only; `forbid(unsafe_code)`. |
| `usb` | USB passthrough: the VMM/helper protocol, the helper's request checks, libusb (loaded with `dlopen`, helper only), host device listing (IOKit registry / sysfs), a pretend serial port for tests. |
| `virtio` | Split-queue handling (hardened), MMIO and modern PCI transports, devices (blk, net, console, rng, gpu, input, fs, sound), a pausable worker per device. `forbid(unsafe_code)`. |
| `fs` | Host side of shared folders: component-wise `openat(O_NOFOLLOW)` walks from a root fd, Linux errno mapping. The virtio-fs FUSE server lives in `virtio`; it serves Linux and virtio-win's virtiofs.exe (Windows), telling them apart by their INIT flags. |
| `audio` | Sound helper: datagram protocol, ring buffer, CoreAudio (macOS) / ALSA (Linux, `dlopen`) playback and recording, WAV test files. |
| `gpu3d` | virglrenderer loaded with `dlopen`, behind the `Renderer3d` trait the virtio crate defines. The one place with GPU FFI. |
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

## 3D

`gpu = "virtio3d"` adds two protocols to virtio-gpu. Venus: guest Vulkan
(Mesa's venus driver) serializes commands, virglrenderer replays them on
MoltenVK. virgl: guest OpenGL (Mesa's virgl driver) sends Gallium
commands, virglrenderer's vrend replays them on macOS's OpenGL 4.1 (CGL),
or with `opengl_host = "angle"` on OpenGL ES from ANGLE's Metal backend.
Both capsets are offered; Mesa picks per API. Guest memory a renderer may
touch is passed as host iovecs; host-visible buffers are
mapped into the guest's shared memory window with the hypervisor.

GL details (`gpu3d/src/gl.rs`): we create the CGL or EGL contexts
virglrenderer asks for (one share group, no surfaces) and keep the right
one bound on whichever thread calls in (setup thread, then the GPU
worker, which changes on device reset). GL fences have no completion thread (ANGLE
serializes a share group's contexts, so a thread blocked on a fence
would stall submits): the worker polls them every millisecond while any
is pending. A GL scanout is copied back on flush: from a context of our
own, into a pixel buffer on the GPU, and shown when the copy lands while
the worker goes on.

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
