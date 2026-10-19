# vroom-vroom

Run Linux and Windows VMs on your Mac (Apple Silicon) or on arm64 Linux.
Written in Rust. The VM process locks itself down before the guest starts,
and every virtual device treats the guest as hostile.

![The app](docs/img/app.png)

## Security

Better than the big three's, at least on paper:

- a sandboxed VMM written in Rust;
- a separate sandboxed GPU process;
- per-VM shared memory;
- guest code can’t reach your files, other apps or the network stack.

```text
 ┌──────────────────────────── your Mac ─────────────────────────────┐
 │                                                                   │
 │   ┌───────────┐                                                   │
 │   │ vroom-gui │◄──── window · keyboard · clipboard · drops        │
 │   └─────┬─────┘                                                   │
 │         │                                                         │
 │   ╔═════╧═══════════════ VMM ════════════════╗                    │
 │   ║                                          ╟──── disks          │
 │   ║   ┌──────────── guest ──────────────┐    ╟──── shared folders │
 │   ║   │  Linux / Windows      (hostile) │    ╟──── NAT            │
 │   ║   └─────────────────────────────────┘    ║                    │
 │   ╚═╤═══════╤═══════╤═══════╤═══════╤═══════╤╝                    │
 │     │       │       │       │       │       │                     │
 │  ╔══╧═══╗╔══╧═══╗╔══╧═══╗╔══╧═══╗╔══╧═══╗╔══╧═══╗  ┌──────────┐   │
 │  ║  3D  ║║audio ║║camera║║video ║║ USB  ║║ TPM  ║  │   netd   │   │
 │  ╚══╤═══╝╚══╤═══╝╚══╤═══╝╚══╤═══╝╚══╤═══╝╚══════╝  │  (root)  │   │
 │  ╔══╧═══╗ sound  camera  codecs  device            └────┬─────┘   │
 │  ║ apps ║ (mic)                                        LAN        │
 │  ╚══╤═══╝                                                         │
 │    GPU                                                            │
 └───────────────────────────────────────────────────────────────────┘
   ═══ own sandbox     ─── no sandbox     camera, mic, video: opt-in
```

A guest that breaks out lands in the VMM's sandbox: its own disks, nothing
else of yours. Each guest Vulkan or Direct3D app gets its own 3D process;
video decoding and encoding get one each. [SECURITY.md](docs/SECURITY.md).

## What works

- Ubuntu, Debian, Alpine, Windows 11 ARM64
- Clipboard both ways, auto resolution, sharp on Retina
- 3D: Vulkan 1.3 (about 90% of native speed, in windows too) and OpenGL 4.1 in Linux guests, OpenGL 4.4 per app through Zink, Direct3D 11 and OpenGL 4.1 in Windows guests
- Coherence: Windows and Linux (X11) apps as windows of their own on the Mac
- Snapshots, suspend/resume, reset
- NAT with port forwards, several adapters; bridged, and host-only with DHCP (Mac ↔ VMs ↔ VMs), via a root helper (one password prompt)
- Sound; microphone and camera, opt-in
- Shared folders (Linux and Windows guests)
- TPM 2.0 (Windows 11 needs it)
- Nested VMs (M3 or newer)
- Import from VMware Fusion (ARM64 VMs)
- USB passthrough, live from the VM window; USB disks also as shared files or a plain disk
- Drag and drop files both ways
- Up to 4 guest monitors, full screen across Mac screens
- Hardware video decode (H.264, HEVC, VP9, AV1) and encode (H.264, HEVC) for Linux guests, opt-in; Firefox, Chrome, FFmpeg use them
- Fast disk and network I/O, measured against QEMU ([numbers](docs/PERFORMANCE.md))

Not yet: x86_64 hosts (it builds, can't run VMs yet) and Secure Boot.

## Try it

```bash
xcodebuild -downloadComponent MetalToolchain
brew install qemu meson ninja bison pkg-config llvm llvm@15 molten-vk lld uv
cargo install cargo-xwin
scripts/app.sh --install     # vroom.app with 3D and guest tools, in /Applications
```

The first build fetches and builds the 3D libraries and Windows drivers
(about an hour); later ones redo only what changed.

Open vroom, New VM, pick Ubuntu (downloaded for you) or a Windows 11
ARM64 ISO: it comes up installed, with the guest tools. Any installer ISO
works too.

Or from the terminal:

```bash
scripts/build.sh --release   # builds and signs vroom + vroom-gui
target/release/vroom new dev --os linux --disk 32G --iso ubuntu-24.04-desktop-arm64.iso
target/release/vroom-gui dev.toml
```

Windows by hand: [GUIDE.md](docs/GUIDE.md#windows-11-arm64).

## Cheat sheet

```bash
vroom-gui                         # the app: VM list, settings, snapshots
vroom-gui dev.toml                # open one VM in a window
vroom run dev.toml                # no window: serial console in this terminal
vroom check dev.toml              # validate a config, print it with defaults
vroom new NAME --os linux|windows --disk 64G --iso FILE
vroom new NAME --os ubuntu|windows --ready [--iso WIN.iso] --ssh-key ~/.ssh/id_ed25519.pub
vroom guest-tools info            # the guest tools this vroom has
vroom import ~/Virtual\ Machines.localized/X.vmwarevm   # from VMware Fusion
vroom snapshot list|delete|prune dev.toml
vroom disk create x.qcow2 --size 64G
vroom disk compact x.qcow2        # give freed space back to the host
vroom usb list                    # host USB devices, the id for [[usb]]
vroom net                         # host-only networks, each running VM's address
```

| In the VM window | Keys |
|---|---|
| snapshot / suspend / pause | `Ctrl+Alt+Shift` + `S` / `Z` / `P` |
| reset / shut down / power off | `Ctrl+Alt+Shift` + `R` / `D` / `Q` |
| Coherence (guest apps as Mac windows) | `Ctrl+Alt+Shift` + `C` |
| toolbar in full screen | move the mouse to the top edge |
| close the window | suspends the VM, next start resumes it |

| In `vroom run` | `Ctrl-A` then |
|---|---|
| power off / shut down / suspend | `x` / `d` / `z` |
| snapshot / pause / reset | `s` / `p` / `r` |
| clipboard to / from guest, screenshot | `c` / `v` / `g` |

App: `⌘K` for VMs, actions, settings; drop an ISO to make a VM.

A VM is one TOML file; the app edits it in place, keeping comments:

```toml
name = "dev"
cpus = 4
memory_mib = 4096

[boot]
type = "uefi"
vars = "dev.state/efivars.fd"

[[disk]]
path = "dev.qcow2"

[[net]]
mode = "nat"
port_forwards = [{ host_port = 2222, guest_port = 22 }]

[display]
gpu = "virtio2d"
```

## Docs

- [GUIDE.md](docs/GUIDE.md): install Ubuntu and Windows, 3D, shares, sound, disks
- [CONFIG.md](docs/CONFIG.md): every config option
- [NETWORKING.md](docs/NETWORKING.md): NAT, bridged, host-only
- [SECURITY.md](docs/SECURITY.md): what is isolated and how, known gaps
- [ARCHITECTURE.md](docs/ARCHITECTURE.md): how the code is laid out
- [PERFORMANCE.md](docs/PERFORMANCE.md): numbers against QEMU, how to measure

Apache-2.0.
