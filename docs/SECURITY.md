# Security model

## Threat model

The guest is hostile: a compromised OS, or malware running in it. Goals, in
order:

1. A guest can't get code execution on the host.
2. If it does (a VMM bug), it lands in a process that can do almost nothing.
3. A guest can't read host data it wasn't given (files, clipboard, network).

Out of scope: a malicious host, side channels the CPU itself leaks
(Spectre-class issues are the hypervisor's and the OS's job), denial of
service by a guest against its own VM.

## Layer 1: small, careful device code

- One process per VM, written in Rust. `devices`, `virtio`, `arch`, `net`,
  `config`, `snapshot` and `proto` are `forbid(unsafe_code)`; unsafe code
  is confined to `hv`, `memory`, `sandbox`, `gpu3d`, `ipc`, `usb` (libusb,
  loaded by the USB helper only, and IOKit device listing) and a few FFI
  shims, with `// SAFETY:` notes.
- Guest memory goes through bounds-checked `GuestMemory`; there are no raw
  pointers into guest RAM in device code.
- Every guest-controlled value is validated: virtqueue rings (loops, sizes,
  indirect tables), NVMe PRP lists, xHCI rings and contexts, GPU resources
  (sizes, counts, rects, backing entries), qcow2 headers and L1/L2 tables,
  network packets, agent messages (size caps, UTF-8).
- Hard limits on everything a guest can make us allocate: GPU resource
  memory, pending fences, queue sizes, NAT connections and their buffers
  (about 80 MiB at most), fragment memory, disk requests in flight.
- Config parsing is strict: unknown keys are errors. `vroom check` (and
  every start) also names risky but valid choices: sandbox off, writable or
  home-wide shares, port forwards reachable from the LAN, host loopback
  open to the guest, bridged networking.
- Logging is capped (50 lines a second, errors excepted): a guest that
  keeps tripping warnings can't flood the terminal or fill a log disk.
- qcow2 backing files: only from the image's own directory or allow-listed
  ones, no protocol prefixes, chain depth capped.
- Imported VMware disks (VMDK, read-only): headers, grain tables and
  descriptors are range-checked and capped like qcow2's; extent files must
  be plain names next to their descriptor (no symlinks out); parents must
  be `.vmdk` files, CIDs must match, chains are capped and loops refused.

## Layer 2: sandbox

After the VM is built and before the guest runs, the VMM drops its
privileges:

**macOS (Seatbelt, `sandbox_init`)**: deny by default. Allowed: the VM's own
disk images and state directory, outbound sockets only if NAT is used,
accept only on the configured port-forward sockets, the pasteboard only if
clipboard sync is on. No exec, no new listeners, no reading the user's
files (tested: `/etc/passwd`, `~/.ssh`, `exec` and `bind` all fail).

**Linux (Landlock + seccomp)**: Landlock limits the filesystem to the same
files and directories; seccomp allows only the syscalls the VMM uses (no
`bind`, `execve`, `ptrace`...). Signals go only to our own threads
(`tgkill` with our pid); new sockets only for IPv4/IPv6 (no Unix,
netlink or packet sockets). Anything else kills the process.

Network reach of the VMM process itself: on macOS the profile allows IP
only (no Unix sockets, so no ssh-agent or docker.sock) and denies the host's
loopback unless `allow_host_loopback` (port 53 stays open if the host's
resolver is local). On Linux, seccomp can't see addresses: loopback is
refused by the NAT code only.

`--no-sandbox` exists for debugging and prints a warning.

Check it on your machine: `VROOM_SANDBOX_SELFTEST=1 vroom run vm.toml`
makes the VMM, once locked down, try to read `/etc/passwd` and
`~/.ssh`, exec a program, open a listener, reach host loopback
(`VROOM_SELFTEST_LOOPBACK_PORT=<a port you listen on>`), connect to
`$SSH_AUTH_SOCK` and reach the internet, and print what happened. On
Linux the Unix-socket probe ends the process (seccomp kills it): that is
the expected result.

## Clipboard

Text only, size capped, and sync happens only while the VM window has focus.
A background guest can't read what you copy on the host, and can't push to
your clipboard behind your back. Headless runs need explicit push/pull.

The GUI (`vroom-gui`) is not sandboxed, so it treats the VMM like the
guest-exposed process it is: it applies the clipboard policy from the VM's
config itself (never sends the host clipboard if `host_to_guest` is off,
never sets it if `guest_to_host` is off, focus and size limits too),
caps and cleans status text, checks every mode and rectangle against the
shared framebuffer, and on Linux only maps a framebuffer memfd that is
sealed against shrinking (a VMM shrinking it would crash the GUI).

The app (same binary, no arguments) also reads VM state dirs, which the
sandboxed VMM can write: it only parses snapshot manifests (JSON, size
checked) and checks for the lock and the suspend marker there. Pictures
it shows come from its own data dir, taken from the shared framebuffer,
never from image files a VMM could have planted. Snapshot names from VMM
status messages must be valid names before they become file names.

## Networking

NAT is a userspace proxy: the guest never gets raw access to the host
network stack. Host loopback is unreachable unless `allow_host_loopback`;
`block_private` keeps the guest off your LAN. Bridged and host-only go
through `vroom-netd`, which enforces allow-listed interfaces and source
MAC anti-spoofing.

## Shared folders

Off unless configured, read-only unless `readonly = false`. The host side
(`vroom-fs`) walks every guest path one component at a time with
`openat(O_NOFOLLOW)` from an fd on the share root: a symlink anywhere ends
the walk, so neither a guest-planted link nor one swapped in on the host
mid-walk leads outside. Names with `/`, NUL, `.` or `..` are refused; only
regular files and directories are opened (no FIFOs, devices, sockets);
the guest can't create links or device nodes. Node, handle and directory
listing counts are capped, and so are path lengths (4096 bytes) and the
memory all node paths take together. The sandbox only grants the share's
own subtree (read-only shares: read access only; writable ones: no set-id
bits, owner changes, ACLs, file flags or extended attributes). The FUSE
server is fuzzed, as both clients and on both kinds of share.

Windows guests (virtio-win's virtiofs.exe) get the same server. Names
that mean something special to Windows (`name:stream`, `CON`, trailing
dots) are plain names on a Mac or Linux host: they stay inside the share.
So does a Mac's `file/..namedfork/rsrc`, since a file is never walked
through. The only extra name accepted is `.` for "this directory", which
the Windows client uses for the root; `..` is still refused. The guest
side needs WinFsp: setup installs one pinned version and checks its
SHA-256 first.

## Sound

Off unless `[audio] output` or `input` is on. PCM goes over a datagram
socket to a helper process (`vroom audio-out`) that plays it, so
CoreAudio and the plug-ins it loads never run in the VMM. The helper
runs under its own Seatbelt profile: system libraries, the CoreAudio
services and the audio user client only; no user files, no network, no
exec. The VMM never blocks on it (it drops audio instead).

Both sound cards parse guest input with the same care as the rest:
virtio-snd chains as virtqueues; HD Audio registers of any width and
offset, CORB/RIRB rings (pointers masked to the ring size, one pass per
doorbell), buffer descriptor lists (index masked, empty or unreachable
entries stop the stream with an error), DMA amounts capped per tick.
Both are fuzzed.

### Microphone

There is a microphone path now, off by default. How it works:

- `[audio] input = false` (the default): the guest's sound card has no
  input at all (no capture stream, no mic pin). The helper is started
  without `--input`, so it refuses to record whatever the VMM asks, and
  its Seatbelt profile has no `device-microphone`. Both are fixed before
  the guest runs; a guest that took over the VMM can't change them.
- `input = true`: the helper may record and its profile allows the
  microphone. The VMM itself still has no microphone access: only the
  helper talks to CoreAudio (or ALSA). The VMM asks for audio only
  while a VM window is open (a viewer is connected; headless runs never),
  the VM runs (pausing stops it) and the guest records. The window shows
  "Microphone in use" whenever the VMM says it asked.
- What that gate does not cover: with `input = true`, the gate and the
  indicator are enforced by the VMM process. A guest that broke out into
  the VMM could ask the helper for the microphone at any time while the
  VM runs, and not tell the window. macOS's own indicator (the orange
  dot in the menu bar) can't be faked by it. That's why it's opt-in.
- macOS asks for the microphone permission on behalf of the app that
  started vroom (Terminal, iTerm, VS Code...): a yes covers everything
  started from that app, not just vroom. See [GUIDE.md](GUIDE.md#microphone).
- What the helper sends back is parsed by the VMM like guest input
  (sizes, whole frames, a generation number so audio from an earlier
  request is dropped), and at most a quarter second is buffered.

## USB devices

Off unless the VM file lists `[[usb]]` devices. The VM process never
touches the host's USB stack: each device gets a helper process
(`vroom usb-host`), started before the VMM locks itself down, that opens
that one device with libusb. The two talk over a socketpair with a small
protocol: submit a transfer, cancel one, reset the device; back come
attached/detached and transfer results.

The helper treats the VMM as hostile, like the VMM treats the guest:

- It only opens the device its command line names (vendor and product,
  and serial number if given), chosen by the VMM's config before the
  sandbox; the protocol has no way to name another device, list devices
  or change the address.
- Every request is checked: sizes (1 MiB per transfer), at most 64
  transfers in flight, endpoints only those of the configuration and
  alternate settings the guest selected, with the type the descriptors
  give. Configuration, interface and halt changes go through libusb's own
  calls; SET_ADDRESS is refused. Hubs are never opened.
- Malformed messages end the connection.

The VMM checks the helper the same way: answers must match a transfer it
sent (size, direction), and never write past the guest's buffers. A dead
or misbehaving helper looks like an unplugged device. The vCPUs never
wait on a helper.

The helper's own sandbox (macOS, Seatbelt): system libraries and libusb,
the IOKit registry, and the USB device and interface user clients only;
no files, no network, no exec, no other drivers. Seatbelt can't tell USB
devices apart, so a compromised helper could open other USB devices macOS
lets a normal user open (none that a macOS driver holds: not keyboards,
mice or disks). On Linux the helper sets no_new_privs and Landlock
(`/dev/bus/usb` and sysfs only); it has no seccomp filter yet, and can
open whatever USB device nodes your user may open (udev rules decide).

The guest gets full control of a device it is given (it can send it any
request, including firmware updates). Give it only devices you'd plug
into that machine.

## Snapshots

Snapshots contain guest RAM (keys, passwords). They're written 0600 in 0700
directories, every blob is SHA-256 checked on load, and restore parses them
with the same validation as live input.

## vTPM

`tpm = true` attaches a TPM 2.0. The TPM itself (libtpms) runs in a
separate `swtpm` process, not in the VMM: on macOS under its own Seatbelt
profile (its install, system libraries, its state directory, nothing
else; no network). The VMM only frames commands for it, with size limits.
State is in `<state>/tpm` (0700); snapshots include it, so reverting a VM
reverts its TPM too.

## What 3D costs

`gpu = "virtio3d"` loads virglrenderer and MoltenVK into the VMM, and
with `opengl` (the default) a GL driver too. Guest command streams are
then parsed by C code and reach the Apple GPU driver: Venus (Vulkan)
commands by virglrenderer's Venus decoder, virgl (OpenGL) commands by its
GL decoder (vrend), which has the longer history of bugs. Where vrend's
GL calls go depends on `opengl_host`:

- `"apple"` (the default): macOS's own OpenGL 4.1. vrend's checks are the
  only ones before Apple's driver, and macOS's GL has no robust buffer
  access, so a shader reading out of bounds reads whatever GPU memory of
  the VMM follows (its own guest's data; each VM is its own process) or
  crashes the VMM. In exchange the guest gets OpenGL 4.1, which most
  Linux apps need.
- `"angle"`: ANGLE on Metal, which checks every call the way a browser
  checks WebGL before anything reaches Metal, so a vrend mistake meets a
  second check. Its contexts ask for robust buffer access (out-of-bounds
  reads give zeros). The guest gets OpenGL 2.1 only.

`opengl = false` leaves vrend and both GL drivers out entirely.

The sandbox keeps working but has to allow the GPU: three IOKit user
clients (`AGXDeviceUserClient`, `AGXSharedUserClient`,
`IOSurfaceRootUserClient`), the Metal compiler service, framework reads
and the Metal shader cache. OpenGL needed nothing more: ANGLE and Apple's
OpenGL (on Apple Silicon it runs on Metal too) are loaded, with a first
context, before the sandbox goes up. libepoxy is built to load ANGLE only
from virglrenderer's own directory, and Apple's OpenGL only by its system
path, when the VMM made a CGL context current first; never a search path.

Snapshots with GL in them hold the guest's GPU contents (textures read
back, so whatever was on screen) next to its RAM, in the state dir. On
restore, the GL state goes back through virglrenderer's checks like any
command the guest sends, and resource sizes, backing ranges and counts get
the same limits as guest commands: a tampered snapshot can do what the
guest could, no more.

Use 3D only for guests that need it. A future step is moving the renderer
into its own process (possible for most of Venus and for virgl, whose
frames already come back by copy; host-visible memory needs shared
mappings).

## Guest agent (Windows)

The VirtIO serial port only admits Administrators and SYSTEM, so the agent
is split. A LocalSystem service owns the port and does clock sync and
clean shutdown. An unprivileged helper in the console session does
clipboard, resolution and UI scale, and talks to the service over a named pipe:
SYSTEM/Administrators/interactive users only, remote clients refused,
only clients from the active console session served. The helper only
talks to a pipe server in session 0, so a user process squatting the pipe
name gets nothing. The service accepts nothing from the helper but
clipboard text (size-capped).

The Linux agent has the same split: a hardened systemd service
(`CAP_SYS_TIME` only, read-only file system, Unix sockets only) and a
session helper started with the desktop. They meet on
`/run/vroom-agent.sock`; the service serves only the user logind reports
active on seat0, and the helper only talks to a root-owned peer.

## Fuzzing

`fuzz/` has cargo-fuzz targets for what a guest (or a downloaded image)
controls: agent protocol, the viewer protocol (both directions), network frames, virtqueue chains over arbitrary
guest memory, virtio-gpu 2D commands, virtio-fs (FUSE) requests, virtio-snd control
and PCM transfers (playback and capture), the HD Audio controller (registers,
CORB/RIRB, buffer descriptor lists, stream DMA, MSI), the virtio-pci transport and PCI
config space, xHCI, NVMe, TPM TIS, fw_cfg (DMA included) and flash, each
driven by arbitrary register writes plus guest RAM; and qcow2 images,
both parsing and a differential target (random writes, discards, zero
writes, reopens and compactions checked byte for byte against a model),
and VMDK (sparse headers, grain tables, compressed grains, descriptors),
the USB passthrough protocol both ways with the helper's checks, and USB
descriptors (the xHCI target also has a passthrough device whose answers
come from the input). Run with
`cargo +nightly fuzz run -O <target>`. First runs found two qcow2 bugs
(a crash on short v3 headers, multi-GB allocations from crafted table
sizes); both fixed. The USB target found a short configuration descriptor
read out of bounds (a panic, not memory corruption); fixed.

## Host resources

A guest can fill its disk layers, and so the host disk. The VMM pauses the
VM when less than 2 GiB is left where its state lives, rather than letting
guest writes fail half-way. A state dir is locked by the VMM using it,
and every disk image is locked too: exclusive while a VM writes it, shared
while it is a read-only layer. A second VM can't write an image another
uses, or build on a base image another is still writing (that silently
corrupts the overlay). On macOS these locks also keep `qemu-img` out of a
running VM's images, even with `-U`.

## Known gaps

- No UEFI Secure Boot: Homebrew's aarch64 EDK2 isn't built with it, and
  without a protected (secure-world) variable store a guest kernel could
  rewrite the keys anyway. Windows 11 is installed with that check
  bypassed.
- The Windows agent split is new; installs from before it run the
  all-in-one agent elevated (re-run `vroom-setup.cmd` to upgrade).
- x86_64 isn't implemented, so none of this is tested there.
- Fuzzing doesn't cover Venus or virgl (3D) command streams: that is
  virglrenderer's C code, which runs in the VMM process when 3D is on.

## What the guest can tell

These come from the VM's `[identity]` (see [CONFIG.md](CONFIG.md#identity)):

- SMBIOS: vendor, product, family, SKU (the VM's name unless set),
  version, date, serials, chassis type, CPU name, the "VM" BIOS flag.
- ACPI table header IDs and the FADT hypervisor vendor.
- The device tree model (direct kernel boot).
- Disks: NVMe model, serial, firmware revision; virtio disk serials.
- PCI IDs of the host bridge, NVMe, USB and HD Audio controllers, and
  the HD Audio codec's IDs. The defaults are QEMU's (vendor 0x1B36; HD
  Audio: QEMU's Intel ICH6 `intel-hda` and `hda-duplex` codec).
- USB keyboard/tablet vendor ID and names, virtio input names and vendor.
- Generated MACs (prefix), and the NAT network: guest, gateway and DNS
  addresses and the gateway's MAC. The defaults (10.0.2.15, .2, .3) are
  the classic QEMU/VirtualBox layout.
- The agent's port name.

`announce_vm = false` drops the SMBIOS and ACPI "this is a VM" markers.

Still visible, whatever the config says:

- virtio devices: PCI vendor 0x1AF4, subsystem 0x1AF4:1100, revision 1.
  Linux and the Windows drivers match on these (virtio-win INFs list
  `SUBSYS_11001AF4&REV_01`).
- fw_cfg: the "QEMU" signature, its DMA signature and file names
  (`etc/acpi/tables`, `etc/smbios/...`, `etc/ramfb`). EDK2 looks for them.
- The board: QEMU virt memory map, PL011/PL031 PrimeCell IDs, CFI flash
  (Intel 0x89/0x18), GICv3, ACPI device names (`COM0`, `PCI0`, `VR00`...)
  and _HIDs. Stock EDK2 and the OS drivers expect this layout.
- PSCI 1.0 / SMCCC 1.1 answers. They name no vendor. On KVM hosts the
  kernel's PSCI also answers the KVM vendor-hypervisor UID call.
- The firmware itself: EDK2 reports its own vendor in UEFI tables.
- The display size follows the window, so it says something about your
  screen; with `[display] hidpi` the guest also learns its pixel density
  (the scale its agent is asked for).
- The display: ramfb, then virtio-gpu with no EDID. With 3D, Vulkan shows
  the host GPU (MoltenVK on Apple). OpenGL's renderer name follows
  `gl_renderer`, but its limits (GL 4.1 with `apple`, GL 2.1 and ES 3.0
  with `angle`, texture sizes, formats) still give the host driver away.
- CPU ID registers come from the host CPU (an Apple core on a Mac), and
  timing differs from bare metal.
- The vTPM is swtpm: its manufacturer is IBM, and the TIS DID/VID register
  says the same (1014:0001) so the two agree.
- NAT's TCP/IP stack (smoltcp) and the absolute-pointer tablet behave like
  no physical router or mouse.
- The guest agent, if installed (its protocol says "vroom").
- Shared folders, if any: virtio-fs itself, the host disk's size and free
  space, and on Windows the `VroomShare_<tag>` services.

So this makes casual checks fail. It doesn't make a VM look like real
hardware to software that tries hard.

