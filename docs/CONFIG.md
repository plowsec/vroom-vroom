# VM config reference

One TOML file per VM. Unknown keys are errors, so a typo can't silently
turn off a security setting. Relative paths are relative to the file.
`vroom check vm.toml` validates a file and prints it back with every
default filled in.

The app edits these same files. You can edit them by hand too; the app
picks up the change.

## Top level

| Key | Default | What it does |
|---|---|---|
| `name` | required | VM name. 1-64 chars of `A-Z a-z 0-9 . _ -`. |
| `os` | guessed | `ubuntu`, `windows`, `debian`, `fedora`, `alpine`, `linux`, `other`. Only picks icons and defaults. |
| `cpus` | 2 | Virtual CPUs, 1-64. |
| `memory_mib` | 2048 | RAM in MiB. |
| `tpm` | false | TPM 2.0 (needs `swtpm`). Windows 11 wants it. |
| `nested` | false | Let the guest run its own VMs. M3 or newer, macOS 15+. No snapshots then. |
| `camera` | false | The Mac's camera, as a USB webcam in the guest (video calls). On only while the VM's window is open and an app in the guest uses it; the window shows when. See [GUIDE.md](GUIDE.md#camera). |
| `virtio_transport` | `auto` | `auto` (MMIO for kernel boot, PCI for UEFI), `mmio`, `pci`. On PCI the VM also gets four PCI Express hot plug slots (network adapters added while it runs go there). |

## `[boot]`

UEFI (installers, Windows, most cases):

```toml
[boot]
type = "uefi"
vars = "dev.state/efivars.fd"    # NVRAM, created on first boot
# firmware = "/path/QEMU_EFI.fd"  # default: search Homebrew/distro paths
```

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
| `bus` | `virtio` | `virtio` (fast, needs a driver) or `nvme` (every OS has a driver, Windows setup needs it). |
| `readonly` | false | Guest can't write. Linux and virtio disks in Windows see a read-only disk; Windows mounts a read-only NVMe disk read-write, and its writes fail ([GUIDE](GUIDE.md#read-only-disks)). |
| `cdrom` | false | Show as a CD (installer ISOs). Needs `readonly = true`. |
| `sync` | `fsync` | What a guest flush costs: `full` (power-loss safe, slow on macOS), `fsync` (survives a host crash), `none` (scratch disks). |

## `[[net]]` (repeat for more adapters, up to 8)

```toml
[[net]]
mode = "nat"                     # nat | bridged | host-only
port_forwards = [{ host_port = 2222, guest_port = 22 }]
```

| Key | Modes | What it does |
|---|---|---|
| `mode` | all | `nat`: guest gets out, nothing gets in except forwards. `bridged`: guest is on your LAN. `host-only`: private network between VMs and the host. |
| `port_forwards` | nat | `{ host_port, guest_port, host_addr = "127.0.0.1", proto = "tcp" }`. Set `host_addr = "0.0.0.0"` to open it to other machines. |
| `allow_host_loopback` | nat | Guest can reach services on the host's 127.0.0.1. Off: those often have no auth. |
| `block_private` | nat | Guest can only reach the internet, not your LAN. |
| `interface` | bridged | Host interface, e.g. `en0`. |
| `network` | host-only | Network name. VMs with the same name see each other. |
| `mac` | generated | `02:12:34:56:78:9a`. Generated ones start with `identity.mac_prefix`. |
| `connected` | all | `false`: the cable is out. The guest sees no link on this adapter and no frames go either way (VMware's "Connected" box). Default `true`. |

Bridged and host-only need the `vroom-netd` helper running as root; the
app sets it up when a VM needs it. See
[GUIDE.md](GUIDE.md#the-root-helper-vroom-netd) and
[NETWORKING.md](NETWORKING.md).

While the VM runs (in the app), `connected`, `mode`, `interface`,
`network` and `block_private` apply right away, to the same adapter with
the same MAC. Port forwards, `allow_host_loopback` (unless an adapter had
it at start), `mac`, and adding or removing an adapter apply at the next
start; a removed adapter is unplugged until then. A suspended VM resumes
with the adapters it was saved with, set up as the file says then. See
[NETWORKING.md](NETWORKING.md#changing-adapters-while-the-vm-runs).

## `[display]`

| Key | Default | What it does |
|---|---|---|
| `gpu` | `none` | `none` (serial console only), `virtio2d`, `virtio3d` (Vulkan and OpenGL drawn by the host GPU, Linux guests; see the [guide](GUIDE.md#3d-acceleration-linux-guests)). |
| `opengl` | true | With `virtio3d`: OpenGL too (virgl). `false` keeps Vulkan only: less host code facing the guest. |
| `opengl_host` | `"apple"` | What runs the guest's OpenGL: `"apple"` (macOS's OpenGL 4.1; the guest gets OpenGL 4.1) or `"angle"` (ANGLE on Metal; the guest gets OpenGL 2.1, with every call checked). See the [guide](GUIDE.md#3d-acceleration-linux-guests). |
| `video_decode` | false | Hardware video decoding for Linux guests: a V4L2 decoder (H.264, HEVC, VP9 on M1 and later, AV1 on M3 and later) backed by the Mac's VideoToolbox in a sandboxed helper. Needs the guest's virtio-media driver; browsers also need our VA-API driver. See the [guide](GUIDE.md#video-decoding). |
| `video_encode` | false | Hardware video encoding for Linux guests: a V4L2 encoder (NV12 in; H.264 or HEVC out) backed by the Mac's VideoToolbox in a sandboxed helper of its own. For video calls and recording. Needs the guest's virtio-media driver; Chrome and FFmpeg's VA-API encoders also need our VA-API driver. See the [guide](GUIDE.md#video-encoding). |
| `width`, `height` | 1280, 800 | Starting resolution. The guest follows the window size once its agent or driver runs. |
| `hidpi` | true | Full resolution on Retina (HiDPI) screens: the guest gets every pixel of the window and its agent sets its UI scale to match (2x on Retina), so text is sharp. Guests without an agent that can scale get half the pixels and the window scales the picture up, as with `false`. See [GUIDE.md](GUIDE.md#retina-screens). |
| `monitors` | 1 | Guest monitors, 1 to 4, each in a window of its own. Needs a `gpu`. Linux gets one GPU with that many outputs; Windows gets a GPU per monitor, the extra ones 2D. The app sets it in Settings → Display. See [GUIDE.md](GUIDE.md#several-monitors). |

## `[[share]]` (host folders, up to 8)

```toml
[[share]]
path = "/Users/me/shared"
tag = "host"                     # in the guest: mount -t virtiofs host /mnt
readonly = true                  # default; false lets the guest change files
```

`tag`: 1-36 of `[A-Za-z0-9._-]`, different for each share. Windows guests
see each share as a drive letter (Z:, Y:, ...) once `vroom-setup.cmd` ran;
see [GUIDE.md](GUIDE.md#windows-guests).

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
| `serial` | none | Pick one device by its serial number. Without it, the first device with that `id` is taken. |
| `name` | none | Shown in the app. The guest never sees it. |
| `mode` | `"device"` | How a USB disk goes to the guest (macOS): `"files"` shares its volumes as a folder, `"disk"` gives the guest the whole disk as a plain USB stick, `"device"` passes the USB device itself. See [GUIDE.md](GUIDE.md#usb-disks). The app writes it when you tick "Remember for this device" or "Always connect to this VM". |
| `readonly` | false | With `"files"` and `"disk"`: the guest may read, not change. |

`"files"` and `"disk"` are done by the app: the VM file's entries with
them connect when the app runs the VM (`vroom run` alone logs that it
skips them). Share tags starting with `vroom-usb` are taken: they are the
shares a USB disk's files come through.

The VM takes the device when it starts and gives it back when it stops.
Unplug it and plug it back in while the VM runs: the guest sees that too.
A device added to the file while the VM runs is picked up at the next
start; to connect one right away, use the USB button in the VM window
("Always connect to this VM" there writes this entry).

The guest sees the device's own descriptors. On macOS, a device that a
macOS driver holds (keyboards, mice, disks, security keys, most USB
serial chips) is taken through vroom-netd, the root helper: see
[GUIDE.md](GUIDE.md#devices-macos-uses). Needs libusb (`brew install libusb`).

## `[audio]`

| Key | Default | What it does |
|---|---|---|
| `output` | false | Guest sound out through the host speakers. |
| `input` | false | The host microphone, for the guest to record from (video calls). It only flows while the VM's window is open, the VM runs and the guest records; the window shows when. Headless runs (`vroom run`) never get it. |
| `device` | `auto` | The sound card: `hda` (Intel HD Audio: Windows and Linux have a driver built in), `virtio` (virtio-snd: Linux only). `auto` is `hda` for Windows guests (`os`, or guessed from the name), `virtio` otherwise. |

The card is there when `output` or `input` is on. With `output` off the
guest still sees speakers but they play nowhere; with `input` off the
guest has no microphone at all. See [GUIDE.md](GUIDE.md#sound) for the
macOS microphone permission.

## `[clipboard]`

| Key | Default | What it does |
|---|---|---|
| `host_to_guest` | true | Host clipboard goes to the guest while its window has focus. |
| `guest_to_host` | true | Guest clipboard comes back to the host. |
| `max_bytes` | 1048576 | Size cap per copy. Text only. |

## `[serial]`

`type = "stdio"` (default, the terminal running `vroom run`), `"file"` with
`path = ...`, or `"none"`.

## `[security]`

| Key | Default | What it does |
|---|---|---|
| `sandbox` | true | Lock the VMM process down (Seatbelt on macOS, seccomp + Landlock on Linux) before the guest runs. Turn off only to debug. |

## `[identity]`

Everything the guest can read about its "hardware". Missing keys keep the
defaults below. New VMs get random serials, so two VMs never match.

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

PCI IDs are `vendor:device` or `vendor:device:subvendor:subdevice`, 4 hex
digits each (`lspci -nn` shows them). Without the subsystem part it is
`vendor:0000`. The default `1b36` is QEMU's vendor ID. A real chip's IDs
make Linux apply that chip's driver quirks; class and revision don't change.
The same goes for `hda_codec_id`: a real codec's IDs (say Realtek
`10ec:0887`) make Linux load that codec's driver, which expects the real
chip's layout, so keep a codec vendor Linux treats as generic unless the
guest is Windows.

The three NAT addresses share one private /24 (10/8, 172.16/12,
192.168/16). Adapter N adds N to the third number, so `192.168.1.100`
gives the second adapter `192.168.2.100`. The guest can't reach that /24
on your LAN, so pick one your LAN doesn't use.

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

What it can't hide is in [SECURITY.md](SECURITY.md#what-the-guest-can-tell).
Changing these on an installed Windows guest looks like new hardware to
it (drivers re-install, activation may ask again). A new `mac_prefix` or
NAT address shows up as a new network.

## State directory

Next to the config: `<name>.state/`. Holds NVRAM, snapshots, the suspend
image, disk layers and the VM's UUID. `vroom run --state-dir` moves it.
