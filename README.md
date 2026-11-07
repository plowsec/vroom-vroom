# vroom-vroom

Linux and Windows VMs on Apple Silicon Macs and arm64 Linux. Rust. The VMM sandboxes itself before the guest starts; every device treats the guest as hostile.

![The app](docs/img/app.png)

## Security

- VMM in Rust, sandboxed
- GPU in a separate sandboxed process
- shared memory per VM
- guest code can't reach your files, other apps or the network stack

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

- Guest breakout lands in the VMM sandbox: its own disks, nothing else.
- One 3D process per guest Vulkan or Direct3D app; one each for video decode and encode.
- Details: [SECURITY.md](docs/SECURITY.md).

## What works

- Ubuntu, Debian, Alpine, Windows 11 ARM64
- clipboard both ways, auto resolution, sharp on Retina
- 3D: Linux guests Vulkan 1.3 (~90% of native, windowed too) and OpenGL 4.1; OpenGL 4.4 per app via Zink; Windows guests Direct3D 11 and OpenGL 4.1
- Coherence: Windows and Linux (X11) apps as Mac windows
- snapshots, suspend/resume, reset
- NAT with port forwards, several adapters; bridged and host-only with DHCP (Mac ↔ VMs ↔ VMs) via a root helper (one password prompt)
- sound; microphone and camera opt-in
- shared folders (Linux, Windows)
- TPM 2.0, Secure Boot (Windows 11 wants both)
- nested VMs (M3+)
- import from VMware Fusion (ARM64)
- USB passthrough, live from the VM window; USB disks also as shared files or a plain disk
- drag and drop files both ways
- up to 4 guest monitors, full screen across Mac screens
- Linux guests, opt-in: hardware video decode (H.264, HEVC, VP9, AV1), encode (H.264, HEVC); Firefox, Chrome, FFmpeg use them
- disk and network I/O measured against QEMU: [PERFORMANCE.md](docs/PERFORMANCE.md)

Not yet: x86_64 hosts (builds, can't run VMs).

## Build

```bash
xcodebuild -downloadComponent MetalToolchain
brew install qemu meson ninja bison pkg-config llvm llvm@15 molten-vk lld uv
cargo install cargo-xwin
scripts/app.sh --install     # vroom.app with 3D and guest tools, in /Applications
```

First build fetches and builds the 3D libraries and Windows drivers: ~1 h. Later builds redo only what changed.

App: New VM → Ubuntu (downloaded for you) or a Windows 11 ARM64 ISO: comes up installed, with guest tools. Any installer ISO works.

Terminal:

```bash
scripts/build.sh --release   # builds and signs vroom + vroom-gui
target/release/vroom new dev --os linux --disk 32G --iso ubuntu-24.04-desktop-arm64.iso
target/release/vroom-gui dev.toml
```

Windows by hand: [GUIDE.md](docs/GUIDE.md#windows-11-arm64).

## Cheat sheet

```bash
vroom-gui                         # the app: VM list, settings, snapshots
vroom-gui dev.toml                # one VM in a window
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

| VM window | Keys |
|---|---|
| snapshot / suspend / pause | `Ctrl+Alt+Shift` + `S` / `Z` / `P` |
| reset / shut down / power off | `Ctrl+Alt+Shift` + `R` / `D` / `Q` |
| Coherence | `Ctrl+Alt+Shift` + `C` |
| toolbar in full screen | mouse to the top edge |
| close the window | suspends; next start resumes |

| `vroom run` | `Ctrl-A` then |
|---|---|
| power off / shut down / suspend | `x` / `d` / `z` |
| snapshot / pause / reset | `s` / `p` / `r` |
| clipboard to / from guest, screenshot | `c` / `v` / `g` |

App: `⌘K` for VMs, actions, settings. Drop an ISO to make a VM.

A VM is one TOML file. The app edits it in place, keeping comments:

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

## Agents

Use these. Don't rebuild them: no SSH, scp, serial-console scraping, or your own socket client.

| Need | Use |
|---|---|
| run a command in a running VM | `vroom guest exec` |
| copy a file in / out | `vroom guest push` / `pull` |
| guest tools too old or missing | `vroom guest update-tools` |
| a VM with the OS and tools installed | `vroom new --ready` |
| a guest's address (host-only) | `vroom net` |
| debugger: memory, registers, breakpoints, kernel symbols, snapshots | `vroom-lab` + `vroom-rpc` |

Installed app: `vroom` is `/Applications/vroom.app/Contents/MacOS/vroom`. Built from source: `target/release/vroom`.

### Guest tools: `vroom guest`

Talks to the guest agent through the VMM. No guest network, no SSH needed.

```bash
vroom guest exec dev -- ipconfig /all                  # dev = app VM name or path to dev.toml
vroom guest exec dev -p -t 300 -- Get-Service          # -p PowerShell (Linux: pwsh), -t timeout s (60)
vroom guest exec dev -- systemd-run --wait --pipe --collect sh -c 'apt-get install -y jq'
vroom guest push dev ./tool.exe 'C:\tools\tool.exe'
vroom guest pull dev /var/log/syslog ./                # to a dir: same name
vroom guest update-tools dev
vroom guest-tools info                                 # tools this vroom ships, versions
```

- VM must be running (`dev isn't running`), agent connected (`no guest agent is running`; window status shows it).
- `exec` runs as Windows `cmd.exe` SYSTEM / Linux `sh` root. Prints stdout/stderr, exits with the command's code (low 8 bits). `124` = timed out. Output capped at 1 MiB each (`--max-out`).
- Linux `exec` runs in the agent's sandbox: read-only system, Unix sockets only. `apt`, Docker, services: wrap in `systemd-run --wait --pipe --collect`.
- `push`/`pull`: size and SHA-256 checked; target replaced only once the whole file arrived.
- `the guest agent is too old ...`: `update-tools` first.
- More: [GUIDE.md](docs/GUIDE.md#guest-tools).

### Debugger: `vroom-rpc`

`vroom-lab` runs a VM under the kernel debugger (Windows ARM64, research build). `vroom-rpc` makes one JSON-RPC call to its socket and prints the JSON result.

```bash
research/build.sh --release                               # target/research/release/vroom-lab (signed)
cargo build --release -p vroom-kernel --bin vroom-rpc      # target/release/vroom-rpc
vroom-lab LAB.toml [--restore SNAP] --agent-socket lab.sock --daemon   # prints pid; kill PID stops it
R() { vroom-rpc --socket lab.sock "$@"; }
R describe
R commands.list '{"category":"kernel"}'                   # every command: params, mutates, risk
R run vm.resume                                           # lab starts paused
R run guest.exec '{"command":"whoami"}'                   # also guest.upload / download / update_tools
R run vm.pause
R run kernel.symbols                                      # once per lab session
R run memory.read_virtual '{"cpu":1,"address":"0xfffff80275ff70a8","length":16}'
R run debug.breakpoints '{"addresses":["0x..."]}'
R events.poll '{"wait_ms":10000}'                          # next stop, fault, shutdown
R traces follow                                           # tracepoint/hwatch records, JSON lines
R run vm.snapshot '{"name":"hit"}'
```

- Always `--daemon`: agent shells kill what they start; a dead lab = `Connection refused`.
- Socket path under 104 bytes: use a relative one.
- `kernel.*`, `memory.*`, `cpu.*`, `debug.*` need a paused VM. A paused guest answers nothing on the network.
- `-32601` = wrong name (error lists the nearest), not a missing feature.
- Gate writes on each command's `risk` (read, control, write, destructive).
- Lab and app can't run the same VM at once.
- All methods, commands, a Python adapter: [research/README.md](research/README.md#agents).

## Docs

| | |
|---|---|
| [GUIDE.md](docs/GUIDE.md) | install Ubuntu and Windows, 3D, shares, sound, disks |
| [CONFIG.md](docs/CONFIG.md) | every config option |
| [NETWORKING.md](docs/NETWORKING.md) | NAT, bridged, host-only |
| [SECURITY.md](docs/SECURITY.md) | what is isolated and how, known gaps |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | code layout |
| [PERFORMANCE.md](docs/PERFORMANCE.md) | numbers against QEMU, how to measure |

Apache-2.0.
