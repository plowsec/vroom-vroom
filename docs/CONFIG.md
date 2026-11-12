# VM config reference

- One TOML file per VM. The app edits it and picks up hand edits.
- Unknown keys are errors (a typo can't turn off a security setting).
- Relative paths: relative to the file.
- `vroom check vm.toml`: validate, print with defaults.

## Top level

| Key | Default | What it does |
|---|---|---|
| `name` | required | VM name. 1-64 chars of `A-Z a-z 0-9 . _ -`. |
| `os` | guessed | `ubuntu`, `windows`, `debian`, `fedora`, `alpine`, `linux`, `other`. Only picks icons and defaults. |
| `cpus` | 2 | Virtual CPUs, 1-64. |
| `memory_mib` | 2048 | RAM in MiB. |
| `balloon` | true | Guest returns freed memory (free page reporting), and its caches when the Mac runs short (balloon inflates). Linux: `virtio_balloon`, in stock kernels; Windows: virtio-win Balloon driver. |
| `tpm` | false | TPM 2.0 (needs `swtpm`). Windows 11 wants it. |
| `nested` | false | Guest runs its own VMs. M3+, macOS 15+. Change needs a full shutdown; resume keeps the saved CPU config. No snapshots. Not Windows Hyper-V (no WSL 2, Docker Desktop): macOS gives guests EL2 without VHE, Hyper-V needs VHE; Windows boots without it. |
| `camera` | false | Mac camera as a USB webcam. Live only while the window is open and a guest app uses it. [GUIDE](GUIDE.md#camera). |
| `virtio_transport` | `auto` | `auto` (MMIO for kernel boot, PCI for UEFI), `mmio`, `pci`. PCI adds four hot plug slots. |

## `[boot]`

UEFI (installers, Windows, most cases):

```toml
[boot]
type = "uefi"
vars = "dev.state/efivars.fd"    # NVRAM, created on first boot
# firmware = "/path/QEMU_EFI.fd"  # default: search Homebrew/distro paths
# secure_boot = true              # New VM: "Secure Boot"; vroom new --secure-boot
```

`secure_boot = true` enables signature checks in the ARM firmware below. Its
variable store remains writable by the guest kernel; an enabled Secure Boot
flag does not establish protected authenticated variables. See the
[security limitation](SECURITY.md#known-gaps). The restricted
[x64 Linux profile](../crates/vmm/X64_LINUX.md) does not support firmware or TPM.

| | |
|---|---|
| Firmware | Debian's EDK2 (`AAVMF_CODE.secboot.fd`), in vroom.app's `Resources/firmware`; a build's in `target/firmware` (`scripts/make-firmware.sh`); Linux: `/usr/share/AAVMF` |
| Keys | PK: Debian's. KEK: Microsoft KEK CA 2011 and KEK 2K CA 2023. db: Windows Production PCA 2011, Windows UEFI CA 2023 (Windows' newer boot manager), Microsoft UEFI CA 2011 and 2023 (shim-booted Linux), Option ROM UEFI CA 2023 |
| NVRAM | made from `AAVMF_VARS.ms.fd`. NVRAM with no PK (made without Secure Boot): recreated at next cold boot, old one kept as `<vars>.bak`. Windows boots on via `\EFI\BOOT\BOOTAA64.EFI` |
| Off again | keys kept; Homebrew firmware doesn't check them |
| Windows | vroom's setup (unattended install, `vroom-setup.cmd`) sets `PreventDeviceEncryption` to suppress automatic device encryption. This default is not evidence that BitLocker protection is enabled. |

BitLocker acceptance requires a fully encrypted OS volume, protection on, a
TPM protector and successful cold-boot unlock, together with protected firmware
variables. Account for the setup suppression above when preparing that test;
the [read-only guest check](../scripts/guest-readiness.ps1) cannot establish
variable-store protection or cold-boot unlock by itself. The AWS pilot has not
yet run Windows or validated BitLocker.

Direct kernel boot (fast, no firmware):

```toml
[boot]
type = "kernel"
kernel = "Image"
initrd = "initrd"                # optional
cmdline = "console=ttyAMA0 root=/dev/vda1"
```

## `[[disk]]` (repeat for more disks, up to 16)

| Key | Default | What it does |
|---|---|---|
| `path` | required | Image file. |
| `format` | `auto` | `auto`, `raw`, `qcow2`. |
| `bus` | `virtio` | `virtio` (fast, needs a driver), `nvme` (driver in every OS; Windows setup needs it). |
| `readonly` | false | Guest can't write. Windows doesn't see it on NVMe ([GUIDE](GUIDE.md#read-only-disks)). |
| `cdrom` | false | Show as a CD (installer ISOs). Needs `readonly = true`. |
| `sync` | `fsync` | Guest flush = `full` (power-loss safe, slow on macOS), `fsync` (survives host crash), `none` (scratch). |
| `backing` | none | qcow2 image's own backing file (`vroom disk create --backing`). Required for a writable disk that has one: the VMM can rewrite the header, so only this file is believed. Relative to the VM file. |

## `[[net]]` (repeat for more adapters, up to 8)

```toml
[[net]]
mode = "nat"                     # nat | bridged | host-only
port_forwards = [{ host_port = 2222, guest_port = 22 }]
```

| Key | Modes | What it does |
|---|---|---|
| `mode` | all | `nat`: out only, in via forwards. `bridged`: on your LAN. `host-only`: private network of VMs and host. |
| `port_forwards` | nat | `{ host_port, guest_port, host_addr = "127.0.0.1", proto = "tcp" }`. `host_addr = "0.0.0.0"`: open to other machines. |
| `allow_host_loopback` | nat | Guest reaches host 127.0.0.1 services. Off by default: those often have no auth. |
| `host_ports` | nat | Only these host 127.0.0.1 ports, at the gateway (`10.0.2.2`). E.g. `host_ports = [8000]`: a local model server, nothing else. |
| `block_private` | nat | Internet only, not your LAN. |
| `mirror` | nat | `"10.99.0.0/24"`: guest traffic there comes back through its NIC, from that address: its own services seen as from a remote peer (fuzzing). |
| `interface` | bridged | Host interface, e.g. `en0`. |
| `network` | host-only | Name in `/etc/vroom/netd.toml`. Default `lab`: 192.168.77.0/24, Mac `.1`, DHCP for guests. VMs on one network see each other. |
| `mac` | generated | `02:12:34:56:78:9a`. Generated ones start with `identity.mac_prefix`. |
| `connected` | `true` | `false`: cable out, no link, no frames. |

- Bridged, host-only: need `vroom-netd` ([GUIDE](GUIDE.md#the-root-helper-vroom-netd)).
- Host-only minimum: `mode = "host-only"`; `vroom net` shows the guest's address.
- Live in the app: `connected`, `mode`, `interface`, `network`, `block_private`.
- Next start: port forwards, `allow_host_loopback`, `host_ports`, `mirror`, `mac`.
- Adapters: [NETWORKING.md](NETWORKING.md#changing-adapters-while-the-vm-runs).

## `[display]`

| Key | Default | What it does |
|---|---|---|
| `gpu` | `none` | `none` (serial only), `virtio2d`, `virtio3d` ([3D](GUIDE.md#3d-acceleration-linux-guests)). |
| `opengl` | true | With `virtio3d`: OpenGL too. `false`: Vulkan only, smaller attack surface. |
| `opengl_host` | `"apple"` | `"apple"` (guest GL 4.1) or `"angle"` (ANGLE on Metal, guest GL 2.1, stricter checks). |
| `video_decode` | false | Linux guests: V4L2 decoder on VideoToolbox. [GUIDE](GUIDE.md#video-decoding). |
| `video_encode` | false | Linux guests: V4L2 H.264/HEVC encoder on VideoToolbox. [GUIDE](GUIDE.md#video-encoding). |
| `width`, `height` | 1280, 800 | Start resolution; then the guest follows the window. |
| `hidpi` | true | Real Retina pixels, guest UI at 2x (needs the agent). [GUIDE](GUIDE.md#retina-screens). |
| `monitors` | 1 | 1 to 4, a window each. Needs a `gpu`. [GUIDE](GUIDE.md#several-monitors). |

## `[[share]]` (host folders, up to 8)

```toml
[[share]]
path = "/Users/me/shared"
tag = "host"                     # in the guest: mount -t virtiofs host /mnt
readonly = true                  # default; false lets the guest change files
```

- `tag`: 1-36 of `[A-Za-z0-9._-]`, unique.
- Windows: drive letters from Z: after `vroom-setup.cmd` ([GUIDE](GUIDE.md#windows-guests)).

## `[[usb]]` (host USB devices, up to 8)

```toml
[[usb]]
id = "1209:000a"                 # vendor:product, as `vroom usb list` shows it
serial = "A1B2C3"                # optional: only this one of several alike
name = "dev board"               # optional label for the app
mode = "device"                  # a USB disk: "files", "disk" or "device"
readonly = false                 # "files" and "disk": the guest can't change it
```

| Key | Default | What it does |
|---|---|---|
| `id` | required | `vendor:product`, 4 hex digits each. |
| `serial` | none | Pick one of several alike. Default: the first with that `id`. |
| `name` | none | Shown in the app. The guest never sees it. |
| `mode` | `"device"` | USB disks (macOS): `"files"` (volumes as a share), `"disk"` (whole disk), `"device"` (the USB device). [GUIDE](GUIDE.md#usb-disks). |
| `readonly` | false | With `"files"` and `"disk"`: the guest may read, not change. |

- `"files"`, `"disk"`: need the app (`vroom run` skips them). Share tags `vroom-usb*` reserved.
- Taken at start, given back at stop; replug works live. Entries added while running: next start (the window's USB button connects now).
- Devices a macOS driver holds: via vroom-netd ([GUIDE](GUIDE.md#devices-macos-uses)).
- Needs `brew install libusb`.

## `[audio]`

| Key | Default | What it does |
|---|---|---|
| `output` | false | Guest sound to host speakers. |
| `input` | false | Host microphone. Live only while the window is open, the VM runs and the guest records. Never headless. |
| `device` | `auto` | `hda` (Intel HD Audio, inbox everywhere), `virtio` (Linux). `auto`: `hda` for Windows. |

Sound card exists if `output` or `input` is on. Mic permission: [GUIDE](GUIDE.md#sound).

## `[clipboard]`

| Key | Default | What it does |
|---|---|---|
| `host_to_guest` | true | Host clipboard to guest while its window has focus. |
| `guest_to_host` | true | Guest clipboard to host. |
| `max_bytes` | 1048576 | Size cap per copy. Text only. |

## `[serial]`

`type` = `"stdio"` (default: the terminal running `vroom run`), `"file"` + `path = ...`, `"none"`.

## `[security]`

| Key | Default | What it does |
|---|---|---|
| `sandbox` | true | Lock down the VMM (Seatbelt on macOS, seccomp + Landlock on Linux) before the guest runs. Off: debugging only. |

## `[identity]`

What the guest reads about its "hardware". Missing keys = defaults below. New VMs get random serials: no two VMs match.

| Key | Default | Where the guest sees it |
|---|---|---|
| `vendor` | `vroom` | SMBIOS: BIOS vendor, system/chassis/CPU/memory maker |
| `product` | `vroom VM` | SMBIOS system product |
| `family` | `Virtual Machine` | SMBIOS system family |
| `sku` | the VM's name | SMBIOS system SKU |
| `version` | `1.0` | SMBIOS BIOS/system/chassis version |
| `bios_date` | `10/14/2026` | SMBIOS BIOS date |
| `serial` | from the VM's UUID | SMBIOS system and chassis serial |
| `chassis_type` | 1 (other) | SMBIOS chassis type, 1-36: 3 desktop, 10 notebook, 35 mini PC |
| `cpu_name` | `CPU` | SMBIOS processor version: the CPU name Windows and `dmidecode` show |
| `announce_vm` | true | SMBIOS "virtual machine" BIOS flag and the ACPI hypervisor vendor field |
| `acpi_oem_id` | `VROOM` | every ACPI table header and the RSDP (up to 6 chars) |
| `acpi_table_id` | `VROOMVM` | every ACPI table header (up to 8) |
| `acpi_creator_id` | `VRM` | every ACPI table header (up to 4) |
| `hypervisor_id` | `VROOM` | ACPI FADT hypervisor vendor, if `announce_vm` |
| `dt_model` | `linux,dummy-virt` | device tree `model` and `compatible` (direct kernel boot) |
| `disk_model` | `vroom NVMe` | NVMe controller model |
| `disk_serial` | `VRM` | disk serial prefix (NVMe and virtio), a number is added |
| `disk_firmware` | `1.0` | NVMe firmware revision (up to 8) |
| `pci_bridge_id` | `1b36:0008` | PCI IDs of the host bridge |
| `pcie_port_id` | `1b36:000c` | PCI IDs of the hot plug root ports |
| `nvme_pci_id` | `1b36:0010` | PCI IDs of the NVMe controller; Identify reports the same vendor |
| `usb_pci_id` | `1b36:000d` | PCI IDs of the USB (xHCI) controller |
| `hda_pci_id` | `8086:2668:1af4:1100` | PCI IDs of the HD Audio sound card (QEMU's `intel-hda`: an Intel ICH6) |
| `hda_codec_id` | `1af4:0022:1af4:0022` | HD Audio codec vendor:device and subsystem (QEMU's `hda-duplex`); Windows shows it as `HDAUDIO\FUNC_01&VEN_..&DEV_..&SUBSYS_..` |
| `usb_vendor_id` | `0x1209` | USB keyboard and tablet vendor ID, also the virtio input vendor |
| `usb_manufacturer` | `vroom` | USB and virtio input device names |
| `mac_prefix` | `02` | first 1-3 bytes of generated NIC MACs, e.g. `00:1b:21` |
| `nat_guest` | `10.0.2.15` | guest address on NAT |
| `nat_gateway` | `10.0.2.2` | NAT gateway (DHCP server, default route) |
| `nat_dns` | `10.0.2.3` | NAT DNS relay; may be the gateway's address |
| `nat_gateway_mac` | `02:42:57:4b:00:01` | the gateway's MAC in the guest's ARP table |
| `gl_renderer` | the host's | OpenGL renderer name with `virtio3d`: `glxinfo` shows `virgl (<this>)`. The host's names its GPU and macOS version. Up to 63 chars |
| `agent_port` | `org.vroom.agent.0` | guest agent port name. The agent finds a renamed port by itself if it's the only one; else start it with `--port` |

- PCI IDs: `vendor:device` or `vendor:device:subvendor:subdevice`, 4 hex digits each (`lspci -nn`). No subsystem part = `vendor:0000`. Default `1b36` = QEMU's vendor ID.
- A real chip's IDs make Linux apply that chip's driver quirks; class and revision don't change.
- `hda_codec_id`: a real codec's IDs (e.g. Realtek `10ec:0887`) make Linux load that codec's driver, which expects the real chip's layout. Keep a generic codec vendor unless the guest is Windows.
- The 3 NAT addresses share one private /24 (10/8, 172.16/12, 192.168/16). Adapter N adds N to the third number: `192.168.1.100` → 2nd adapter `192.168.2.100`.
- The guest can't reach that /24 on your LAN: pick one your LAN doesn't use.

```toml
[identity]
vendor = "LENOVO"
product = "ThinkStation P360"
sku = "30FA"
chassis_type = 3
announce_vm = false
acpi_oem_id = "LENOVO"
nvme_pci_id = "144d:a808:144d:a801"
mac_prefix = "00:1b:21"
nat_guest = "192.168.1.100"
nat_gateway = "192.168.1.1"
nat_dns = "192.168.1.1"
```

- What it can't hide: [SECURITY.md](SECURITY.md#what-the-guest-can-tell).
- Changing these on an installed Windows guest = new hardware (drivers reinstall, activation may ask again).
- New `mac_prefix` or NAT address = a new network to the guest.

## State directory

`<name>.state/`, next to the config: NVRAM, snapshots, suspend image, disk layers, VM UUID. `vroom run --state-dir` moves it.
