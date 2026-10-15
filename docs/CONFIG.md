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
| `virtio_transport` | `auto` | `auto` (MMIO for kernel boot, PCI for UEFI), `mmio`, `pci`. |

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
| `readonly` | false | Guest can't write. |
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
| `mac` | generated | `52:54:00:12:34:56`. |

Bridged and host-only need the `vroom-netd` helper running as root. See
[NETWORKING.md](NETWORKING.md).

## `[display]`

| Key | Default | What it does |
|---|---|---|
| `gpu` | `none` | `none` (serial console only), `virtio2d`, `virtio3d` (Vulkan through MoltenVK, Linux guests). |
| `width`, `height` | 1280, 800 | Starting resolution. The guest follows the window size once its agent or driver runs. |

## `[[share]]` (host folders, up to 8)

```toml
[[share]]
path = "/Users/me/shared"
tag = "host"                     # in the guest: mount -t virtiofs host /mnt
readonly = true                  # default; false lets the guest change files
```

Linux guests only for now.

## `[audio]`

| Key | Default | What it does |
|---|---|---|
| `output` | false | Guest sound out through the host speakers. There is no microphone path. Linux guests only. |

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
| `version` | `1.0` | SMBIOS BIOS/system/chassis version |
| `bios_date` | `10/14/2026` | SMBIOS BIOS date |
| `serial` | from the VM's UUID | SMBIOS system and chassis serial |
| `announce_vm` | true | SMBIOS "virtual machine" BIOS flag and the ACPI hypervisor vendor field |
| `acpi_oem_id` | `VROOM` | every ACPI table header and the RSDP (up to 6 chars) |
| `acpi_table_id` | `VROOMVM` | every ACPI table header (up to 8) |
| `acpi_creator_id` | `VRM` | every ACPI table header (up to 4) |
| `hypervisor_id` | `VROOM` | ACPI FADT hypervisor vendor, if `announce_vm` |
| `disk_model` | `vroom NVMe` | NVMe controller model |
| `disk_serial` | `VRM` | disk serial prefix (NVMe and virtio), a number is added |
| `usb_vendor_id` | `0x1209` | USB keyboard and tablet vendor ID |
| `usb_manufacturer` | `vroom` | USB and virtio input device names |
| `agent_port` | `org.vroom.agent.0` | guest agent port name; start the agent with `--port` if you change it |

```toml
[identity]
vendor = "LENOVO"
product = "ThinkStation P360"
announce_vm = false
acpi_oem_id = "LENOVO"
```

What it can't hide is in [SECURITY.md](SECURITY.md#what-the-guest-can-tell).
Changing these on an installed Windows guest looks like new hardware to
it (drivers re-install, activation may ask again).

## State directory

Next to the config: `<name>.state/`. Holds NVRAM, snapshots, the suspend
image, disk layers and the VM's UUID. `vroom run --state-dir` moves it.
