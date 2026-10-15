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

## State directory

Next to the config: `<name>.state/`. Holds NVRAM, snapshots, the suspend
image, disk layers and the VM's UUID. `vroom run --state-dir` moves it.
