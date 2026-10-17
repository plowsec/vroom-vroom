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
  `config`, `snapshot`, `proto` and `video` are `forbid(unsafe_code)`;
  unsafe code is confined to `hv`, `memory`, `sandbox`, `gpu3d`, `ipc`,
  `usb` (libusb, loaded by the USB helper only, and IOKit device listing)
  and a few FFI shims (VideoToolbox in the video helper among them), with
  `// SAFETY:` notes.
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

## Drag and drop

Files dragged onto a VM window are read by the app, which the user
dragged them to, never by the VMM: its sandbox opens no file of the
user's. The app writes them into a pipe it hands the VMM; the VMM checks
each frame (this drop's id, file entries and data only, size-capped) and
passes it to the guest agent. While files only hover, the guest gets
their names and nothing else, so a guest can't read what the user merely
dragged across its window.

In the guest the agent checks every path again (relative, no `..`, no
names Windows reads another way, under a name the drag announced) and
writes no more bytes than each file announced. Files land in a staging
folder of the logged-in user (their temp folder on Windows, their cache
folder on Linux), then go where the drop lands. The drag window the agent
puts under the pointer (Windows, X11) only starts a drag of those files.

Files dragged out of the guest go the other way, and there the guest is
the one to distrust. It can only answer a question: the app asks once
per press of the mouse button, when the mouse leaves the guest screen
with the button held, and the VMM takes one answer per question; a list
of names alone starts a drag on the Mac but copies nothing. Data only
flows when something on the Mac takes the drop and asks for an item, and
only that item. The guest picks the names and the sizes, so both are
checked twice, in the VMM and in the app (vroom-proto's `TreeCheck`):
names `drop_name_ok` takes (no `/`, `..`, control characters), the item
asked for first, every entry inside a folder announced before it, no
more bytes than a file announced, at most 200,000 entries; frames of an
item the app gave up on carry an old stream number and are dropped. A
guest that breaks a rule loses the drag. The app, not the VMM, writes
the files, where the Mac drop says: it only creates new files and
folders (`O_EXCL`, `O_NOFOLLOW`, `mkdir`), never writes through a link
or over something there (a taken name gets a number), removes what it
wrote if the copy fails, and stops when the disk would have less than
2 GB free. In the guest the agent reads only the paths the desktop's
drag handed its drop target, and leaves out links inside folders.

## Coherence

In Coherence the guest decides what windows appear on the Mac desktop,
where, and with what title. That is the guest's word, so:

- The list is checked three times (agent protocol in the VMM, viewer
  protocol in the app, then the app itself): at most 64 windows, ids
  unique, titles of at most 256 bytes of UTF-8, sizes within 65536
  pixels. The app clips every window to the guest's screen and shows at
  most 24. Titles lose control and direction-changing characters and are
  cut to 80 characters. A guest that floods lists costs one queued list
  in the VMM and one in the app.
- App icons are the guest's pictures: raw RGBA, at most 64x64, exactly
  that many bytes, checked by the VMM's and the app's decoders and once
  more before they become an image. No image format is decoded on the
  Mac. Only icons of windows in the last list are passed on; a guest
  that floods them costs one waiting icon per window (at most 128) in the
  VMM and in the app. Icons show in our bar next to the title, never in
  the strip, the Dock or the menu bar.
- Every Coherence window carries a strip the app draws above the guest's
  picture, with the VM's name and the window's title; the guest can't draw
  over it, and has no way to open a window without it. Its macOS window
  title starts with the VM's name too (Mission Control, the Window menu).
- The guest's screen is placed inside the Mac screen's free area: guest
  windows can't cover the menu bar, the Dock or the Coherence bar, and
  they sit at the normal window level (never above other apps' windows by
  level).
- New guest windows open behind other apps' windows while vroom isn't the
  app in front, and the guest's window order is only applied while it is:
  a guest can't pop windows over what the user is working on. It still
  can while the user works in one of its windows.
- Clicking a Mac window activates its guest window; menus and other
  windows without a title are never activated. The guest is asked to
  act only on windows of its last list (the VMM and the agent both
  check), and only to bring them to the front, minimize, close them
  politely (`SC_CLOSE`, the app may refuse) or move them.
- The clipboard rules don't change: sync only while one of the VM's
  windows (or the bar) has the keyboard.
- The bar's app field sends a name to the guest (one line of plain
  text, at most 256 bytes, checked by the viewer protocol, the VMM and
  the agent). Nothing on the Mac is started. Keys typed in the field
  stay out of the guest until Enter.
- In the guest, that name only picks one of the apps installed there:
  Windows: a Start menu shortcut (`.lnk` in ProgramData's or the user's
  Start menu folder) or an app of `shell:AppsFolder` (Store apps, by
  their AppUserModelID); Linux: a desktop entry of the menu (the XDG
  data dirs; entries the menu hides, NoDisplay, Hidden, OnlyShowIn
  another desktop, are left out). The name is only compared with the
  apps' names. It never becomes a command line, an argument, a path or
  a file to open. That is the boundary: the host can start what the
  user could start from the guest's menu, and nothing else, with no
  files and no options. Linux runs the entry with `gio launch` or its
  own Exec line split into words (field codes out), never through a
  shell, as the session's user; an agent running as root refuses.
  Windows opens the shortcut, or `shell:AppsFolder\<id>`, with
  `ShellExecute` from the user's session helper. Only when no app has
  the name, and the name is plain (letters, digits, spaces and a few
  marks: no path, no command line), does Windows fall back to typing it
  into Start's search, as the user would; that may pick something else
  (a web search). The host has the guest's keyboard anyway.
- The list of apps the guest sends for the field to suggest (`Apps`) is
  the guest's word too: at most 512, each name a line the field could
  send and given once, icons raw RGBA of at most 32x32 with exactly that
  many bytes, checked by the VMM's and the app's decoders and once more
  before they become an image. Passed on only in Coherence, and only
  shown as suggestions: a guest that lies about its apps can make the
  field suggest a name, and that name goes back to the guest's own
  matching above. The guest reads its icons and logos itself (PNG files
  of at most 4 MB and 1024x1024; Store app manifests without DTDs); no
  image format is decoded on the Mac.
- "Coherence can't work here" from the guest (Wayland) is one line of
  plain text, cleaned again before the app shows it.
- The guest's screen follows the bar to another Mac screen, never on the
  guest's say: only where the user put the bar. Windows the user drags
  off the guest's screen stay on it in the guest.
- In the guest, window events (Windows hooks, X11 events) only wake the
  helper, at most every 10 ms: a guest app that moves windows all the
  time costs the helper's CPU and a list per look, coalesced as above.
- X11: the helper acts on windows as a taskbar does (client messages to
  the window manager) and only on windows of its last list. It may hide
  Ubuntu's dock (a user setting, put back after) and GNOME's top bar
  (it enables the guest tools' own GNOME Shell extension, which does
  nothing else, and disables it after).

What's left: a guest app can still draw anything below the strip,
including something that looks like a Mac dialog. The strip says whose
window it is; read it before typing a password into a window.



NAT is a userspace proxy: the guest never gets raw access to the host
network stack. Host loopback is unreachable unless `allow_host_loopback`;
`block_private` keeps the guest off your LAN. Bridged and host-only go
through `vroom-netd`, which enforces allow-listed interfaces and source
MAC anti-spoofing (see [the root helper](#the-root-helper-vroom-netd)).

### Network changes while the VM runs

An adapter's cable and network can change while the VM runs (see
[NETWORKING.md](NETWORKING.md#changing-adapters-while-the-vm-runs)). The
sandbox stays as it was set at start; nothing here widens it.

- **Who asks**: the VM window (`ToVmm::NetConnect`, `ToVmm::NetSet`,
  viewer messages 40 and 41). The guest has no way to send them. The
  VMM checks each like guest input: adapter index, mode, names (the VM
  file's rules), forwards (count, addresses), MAC (unicast), and that a
  bridged or host-only change comes with its socket.
- **Bridged and host-only**: the sandboxed VMM can't open Unix sockets,
  so it can't reach vroom-netd after start, and that stays so. The app
  asks netd for the port, as the user (netd's checks and per-user
  limits apply as for any request), and hands the VMM the frame socket
  over the window's connection (SCM_RIGHTS), as for USB helpers. What
  the VMM gets is what it gets at start: one datagram socket of Ethernet
  frames for one adapter, behind netd's interface allow-list and
  source-MAC check. Opening sockets itself would have needed Unix
  sockets in its sandbox (ssh-agent, docker.sock); a socket handed over
  reaches only netd's port. The MAC in the app's request is the one the
  VMM reports for the adapter, made locally administered: netd refuses
  other MACs whoever asks, and on macOS vmnet picks its own anyway.
- **NAT**: the VMM's own, and only if its sandbox already allows
  connections out (a NAT adapter at start). A VM that started with only
  bridged or host-only adapters has no network in its sandbox and gets
  none later: NAT waits for its next start. `allow_host_loopback` turned
  on later works only if an adapter had it at start (the sandbox refuses
  the host's loopback otherwise; the NAT code doesn't even try).
- **Port forwards** need listening sockets, which the sandbox never
  allows: new ones wait for the next start. A NAT the adapter started
  with is kept, forwards and all, while the adapter is on another
  network: its listening sockets stay open, so a connection to a
  forwarded port then waits until the adapter is back on NAT.
- **MAC translation**: when a vmnet port passes another MAC than the
  guest's (the adapter started on NAT), the VMM rewrites one to the
  other in frames: Ethernet, ARP, the DHCP client address, IPv6 neighbor
  discovery options (`vroom-net/src/mac.rs`). Every offset is checked;
  frames it can't parse pass unchanged. netd still drops frames whose
  source isn't the port's MAC, so a guest can't use it to send as
  another machine.
- A cable pulled out drops frames both ways in the VMM: nothing reaches
  the guest or leaves it on that adapter.

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

### Camera

Off unless `camera = true`; then there is a USB camera in the guest and a
helper process (`vroom camera-helper`) with a Seatbelt profile of its
own: the camera, the system's camera services and frame buffers
(IOSurface), reading the system's libraries; no files of the user's, no
writes, no network, no exec. The VMM never touches the camera. It asks
the helper for frames only while a window is open and the guest reads
them (two seconds after the guest stops, the camera goes off), and the
window shows "Camera in use" whenever it asked. The same limit as the
microphone applies: a guest that broke out into the VMM could ask for the
camera while the VM runs without telling the window; macOS's green
camera light can't be faked by it. Frames from the helper are checked
like guest input (size, a whole frame) before they reach the guest.

## Video decoding

Off unless `[display] video_decode = true`. A guest's video stream is
about the most hostile input there is: decoders are big, old C code. So
the decoding never happens in the VMM:

```text
guest V4L2 decoder --virtio-media--> VMM device (Rust)  --socket--> vroom video-helper --XPC--> Apple's decoder service
                                     checks every ioctl,             parses the stream,          (macOS's own sandbox)
                                     copies bitstreams out,          then VideoToolbox;
                                     grants picture buffers          writes pictures into
                                                                     the granted buffers
```

The VMM's device (`virtio/src/devices/media`, no unsafe code) speaks
V4L2 to the guest and nothing else: every ioctl is size-checked, queue
types, buffer indices, memory types and control values are checked, at
most 8 decoders, 16 bitstream buffers (8 MiB each at most) and 24 picture
buffers each (4096x2304 at most). The buffers live in one shared memory
window of 1 GiB the guest maps; the guest can only map our buffers'
offsets in it. A bitstream is copied out of the window before it goes
anywhere (the guest can't change it under the helper), and no more than
16 MiB or 32 buffers per decoder wait at the helper. Pending events are
capped.

The helper (`vroom video-helper`, one per VM, started before the VMM's
sandbox goes up) runs under its own Seatbelt profile: system libraries
(read), VideoToolbox's decoder service (an XPC service, the only
service it may look up) and frame buffers (IOSurface). No files, no
network, no exec, no GPU, no writes. Besides its socket it holds the
buffer window: the VMM passes it the window's shared memory object at
start, and it writes each picture straight into a guest buffer there.
That is all the guest memory it gets: the window holds only decoder
buffers, never guest RAM.

Before VideoToolbox sees anything the helper takes the stream apart
itself (`crates/video`, fuzzed): H.264 / HEVC NAL units (4096 per buffer
at most), parameter sets (4 KiB each, ids within what the standards
allow), access units (256 per buffer), slice headers for the display
order; VP9 superframe indexes and frame headers up to the frame size;
AV1 OBU headers and sequence headers (1 KiB at most). VideoToolbox gets
parameter sets and length-prefixed access units, VP9 frames whose header
we read, AV1 temporal units without delimiters or padding, and a
sequence header we parsed.

Each side treats the other as hostile. The helper takes from the VMM
only messages of known kinds within their size (8 MiB of bitstream, a
grant for at most one 4K P010 picture), for 8 sessions, 64 queued buffers
and 64 grants each. It writes a picture only into a grant that lies in
the window and is big enough for it. The VMM takes from the helper only
pictures in the oldest buffer it granted (the helper must name it), under
the current epoch (one that changes whenever the guest's buffers do), of
exactly the size the stream's format makes; a format must be a valid
size and depth; more than 256 unread messages, or a write that blocks for
2 seconds, and the helper counts as gone. Then the guest's decoders fail
with `EIO` and the rest of the VM runs on. The VMM never reads a
picture.

A helper taken over by a stream can write anywhere in the window: into
that VM's other decoders' buffers, which the guest sees anyway. It can't
reach the VMM's memory, guest RAM or the guest's other devices.

Snapshots keep each decoder's V4L2 side and up to 16 KiB of resume state
from the helper (the stream's parameter sets, VP9 key frame header or AV1
sequence header). The VMM doesn't read that state; after a restore it
hands it to the new helper, which parses it like any stream. A snapshot
is a file: on restore every saved field is checked again (sizes, buffer
offsets inside the window and not overlapping, queues naming real
buffers), and a decoder whose state doesn't hold together fails.

What a bug reachable from a stream gets:

- in the helper: VideoToolbox's client side, its decoder service and
  IOSurfaces; the streams and buffers of that VM's other decoders.
  Nothing else of the user's.
- in Apple's decoder service (`com.apple.coremedia.videodecoder`), which
  the hardware decoder's kernel driver sits behind: the same reach a video
  file opened in Safari or QuickTime has. That code is Apple's and shared
  with every app on the Mac; this is the real cost of turning it on.

Memory a guest can make us use: the window (up to 1 GiB, only what its
buffers touch), and in the helper per decoder up to 16 MiB of bitstream
and 24 pictures held as VideoToolbox's buffers (8 waiting for a guest
buffer, up to 16 held for display order): about 350 MB for a 4K decoder.

The guest side runs Google's virtio-media driver, out of tree.
`scripts/guest/install-virtio-media.sh` downloads one pinned commit and
checks its SHA-256 before building it. Our VA-API driver
(`scripts/guest/vaapi`) runs inside the guest's players and only talks to
that V4L2 decoder (and the guest's GPU for dma-bufs): nothing new on the
host side.

## USB devices

Off unless the VM file lists `[[usb]]` devices or you connect one from
the VM window. The VM process never touches the host's USB stack: each
device gets a helper process (`vroom usb-host`) that opens that one
device with libusb. The VMM starts the VM file's helpers before it
locks itself down; for a device connected while the VM runs, the app
starts the helper (or vroom-netd does, below) and passes its socket to
the VMM over the viewer connection. The VMM can't start anything itself
and gains nothing new: one more socket to a helper that is as limited
as the others. The two talk with a small protocol: submit a transfer,
cancel one, reset the device; back come attached/detached and transfer
results.

The helper treats the VMM as hostile, like the VMM treats the guest:

- It only opens the device its command line names (vendor and product,
  and serial number if given), chosen by the VMM's config before the
  sandbox; the protocol has no way to name another device, list devices
  or change the address.
- Every request is checked: sizes (1 MiB per transfer), at most 64
  transfers in flight, endpoints only those of the configuration and
  alternate settings the guest selected, with the type the descriptors
  give. Isochronous transfers: at most 128 packets, none larger than
  what the endpoint moves per interval (its descriptors say).
  Configuration, interface and halt changes go through libusb's own
  calls; SET_ADDRESS is refused. Hubs are never opened.
- Malformed messages end the connection.

The VMM checks the helper the same way: answers must match a transfer it
sent (size, direction, for isochronous the packet count and each
packet's size), and never write past the guest's buffers. The VMM keeps
at most 8 transfers in flight per endpoint and 32 per device. A dead
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

The test devices (`VROOM_USB_FAKE`: serial port, sound card, USB stick)
run inside the helper in place of libusb, behind the same checks. The
stick parses the guest's SCSI commands there: fixed-size blocks, every
offset checked against its RAM disk.

### USB disks: their files, or the disk

macOS 15.3 and later refuse USB storage to any program without Full Disk
Access, root included. So the app offers two ways that don't pass the
USB device, and need no root helper (GUIDE, "USB disks"):

"Share its files". The app (unsandboxed, the user's) opens the volume's
directory (`/Volumes/NAME`) and issues a sandbox extension for that one
path: a token the kernel signs, of a class of ours
(`org.vroom.usb-files.read`, or `.read-write`). It passes the directory
fd and the token to the VMM over the viewer socket. The VMM's profile
allows file access under an extension of those classes and nowhere else
new: without a token from the app it reaches nothing more. (A token
that comes while the VMM is still setting up, as for a VM file's disk,
is kept and consumed once the sandbox is on: one consumed before it
would be lost with the new sandbox.) Seatbelt
checks paths, so a directory fd alone wouldn't be enough; the token is
what lets the VMM in, and only below that directory. The files are
served by the same code as shared folders (component walks with
`openat(O_NOFOLLOW)`, no symlink followed, no `..`), read-only when the
user asked or when macOS has the volume read-only. On disconnect the
VMM drops the share (its fds close at once) and releases the token. A
VMM taken over by the guest could keep the token instead: it then keeps
read access to that volume, as long as the VMM lives. The token names
the path; whether the kernel also binds it to that volume (a new one
mounted later under the same name) we haven't checked. The extension
calls are not public API (Safari and WebKit use them): a macOS update
could change them, and the share would then fail, not open up.

The guest sees these on two virtio-fs devices every VM started on a Mac
has (`vroom-usb0`, `vroom-usb1`), empty until something is shared. The
agent mounts them when the VMM says so (a tag of that form and a volume
name checked like a file name: no `/`, `..`, leading dot or dash); on
Linux through `systemd-run` (the agent's own service has no mount
privilege), with the arguments passed as they are, no shell.

"Use as a disk". The app finds the disk in the IOKit registry below the
chosen USB device (vendor, product, serial), and takes it only if macOS
calls it removable or ejectable, its storage device doesn't say it is
internal, and nothing of it is mounted where macOS lives. It unmounts it
(`diskutil unmountDisk`, which refuses while files are open), checks
the same disk is still there, and opens `/dev/rdiskN`: directly when the
user owns the node (disk images), else with `/usr/libexec/authopen`,
macOS's tool for this, which asks for an admin password and passes the
fd back over a socket. The fd must be a character device with that
disk's device number. The VMM gets the fd over the viewer socket. It
checks it is a character device (or, for tests, a plain file) and reads
its size with the disk ioctls; its profile allows ioctls on
`/dev/rdisk*` only, and only on fds it was given (it can open no disk).
It serves the disk as a plain USB stick: the test stick's Bulk-Only and
SCSI code, now in a thread of the VMM with the fd as its store, behind
the same request checks as a helper (endpoints, sizes, transfers in
flight). Every block number and length is checked against the disk's
size before the read or write; reads and writes are whole blocks (the
fuzzer's `usb_proto` target drives it as a hostile VMM would). On
disconnect the VMM flushes (the disk's cache too) and closes the fd
before it tells the app, which then has macOS mount the disk again. A
disk image whose partitions don't come back is read once more by the
app: it opens `/dev/rdiskN` for writing and closes it (macOS reads the
partition table again then). It writes nothing, does it only while
nothing of that disk is mounted, and opens no disk node it can't open
as the user (a USB stick's is root's).

What the guest gets: all of that disk, to read and overwrite (format it,
plant a boot sector). That is what was asked for. It can't reach other
disks: the VMM holds the one fd. vroom-netd has no part in it: no root
process opens disks.

macOS's privacy checks: reading a removable volume, and probably opening
a removable disk, is a "Removable Volumes" permission macOS asks the
user for once, on behalf of vroom (the app's `Info.plist` says why). If
the user said no, the app says where to allow it.

The VM's xHCI always has 8 USB 2 + USB 3 port pairs for these devices
(fixed, so the guest and saved VMs see the same controller); the
viewer protocol caps names and the list, and the VMM checks every
device id it is handed. A VMM taken over by a guest can't plug in a
device: it gets only the sockets the app gives it.

## The root helper (vroom-netd)

Bridged and host-only networking, and taking a USB device from a macOS
driver, need root. vroom has one process for that, `vroom-netd`, a
launchd daemon. The app sets it up when asked (macOS's own password
prompt, `osascript ... with administrator privileges`): a root-owned
copy of the binary and of libusb in `/usr/local/libexec/vroom`, its
config in `/etc/vroom/netd.toml`, its launchd file. The setup refuses to
go under a `/usr/local` or `/usr/local/libexec` someone other than root
can change. The app itself never runs as root.

What it does, and only that:

- Network: on request, a vmnet (macOS) or tap (Linux) port for one VM
  adapter, handed back as a datagram socket of Ethernet frames. Only
  interfaces `bridge_interfaces` allows, only host-only networks it
  lists; locally administered unicast MACs only, every frame's source
  MAC checked; 32 ports per user.
- USB: on request, the USB helper (`vroom-netd usb-host`, the same code
  as `vroom usb-host`) started as root for one device, handed back as a
  stream socket.

Who can ask: its socket (`/var/run/vroom/netd.sock`) is root-owned,
mode 0660, group `group` (`staff`: the Mac's users). Each connection's
user comes from the kernel (`getpeereid`, `SO_PEERCRED` on Linux), not
from what it says. USB requests are for members of `[usb] group` only
(`admin` by default: users who could do the same with sudo), and off
unless `[usb] enabled`.

What a request can say: one JSON line, strictly parsed (unknown fields
refused, bounded length, 5 seconds to send it). For USB: a VM name for
the log, a `vendor:product` id and maybe a serial number (printable
ASCII). No paths, no bus locations, no arguments, no environment. The
device must be plugged in and not a hub, and not already served to a
VM. At most 10 USB requests a minute per user, 8 USB helpers per user,
16 in all, 64 connections at once.

How the root USB helper is run: `/usr/bin/sandbox-exec` with its own
Seatbelt profile, as the user's helper but libusb only from the
root-owned copy (`/opt/homebrew` is the user's: nothing a user can
write is loaded into a root process), with an empty environment. The
daemon checks before each start that its binary and that libusb, and
every directory above them, belong to root and no one else can write
them. So the helper can do what the user's helper does (the USB device
user clients, the IOKit registry; no files, no network, no exec), plus
what root adds there: open and take any USB device. That is what was
asked for: the user named the device, and the helper serves only that
one. On Linux the root helper sets no_new_privs and Landlock like the
user's.

What the root USB helper does to the device: as soon as it opens one a
macOS driver holds (or one an earlier helper left taken), it takes the
whole device (libusb's "capture": macOS re-enumerates it and keeps its
drivers off; a mounted disk is unmounted). It keeps it through the
guest's resets, so macOS never mounts the disk in between, and gives
it back when it ends (one more re-enumeration, macOS's drivers come
back). A device the guest can't use at all is given back at once.

macOS 15 opens USB storage only to a program with Full Disk Access,
root or not. To pass a USB disk as the device the user gives vroom-netd
that permission (GUIDE); sharing its files or using it as a disk (above)
needs neither the permission nor vroom-netd. That covers the daemon and the helpers it starts: macOS then
lets them read the user's protected files too (Mail, Messages, other
apps' data), which root alone can't. Neither does: the daemon reads
only its config, and the helpers' Seatbelt profile allows no files at
all. It is a real widening still: code execution in vroom-netd would
get it. Without the permission everything else works; only USB disks
fail, and they go back to macOS.

What a compromised VMM can do with it: nothing new. The VMM asks for
the VM file's USB devices when it starts, before it locks itself down
(the user wrote that file); afterwards its sandbox has no way to reach
the daemon's socket. Devices connected while the VM runs come from the
app. The network side was already its business. A compromised user
account in `admin` could take USB devices (keyboards included) without
the password; it could also wait for the next sudo. The daemon logs every request with the user to
`/var/log/vroom-netd.log`.

Passing sockets: macOS can deliver a socket already disconnected when
the sender closes its own copy while the socket is still in flight
(about 1 in 10 times here). The daemon keeps its copy until the
client hangs up; the app keeps its copy of a helper's socket for a few
seconds after handing it to the VMM.

Frame sockets: macOS gives a Unix datagram socket 4 KiB of buffer, two
frames. Both ends now ask for 2 MiB, and when the VMM's side is full
anyway the daemon drops the frame, as a full NIC queue would. Before,
it gave the whole port up after the first failed send, so a bridged
adapter could die a few seconds into a boot (the guest's driver isn't
reading yet) or during a burst. Only a VMM that is gone ends a port.
That part needs the daemon set up again to take effect; the VMM's
bigger buffer helps with an older daemon too.

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

With `gpu = "virtio3d"`, guest command streams are parsed by C code and
reach the Apple GPU driver: Venus (Vulkan) commands by virglrenderer's
Venus decoder, virgl (OpenGL) commands by its GL decoder (vrend), which
has the longer history of bugs, and on Windows, Direct3D 11 calls
(Neptune, UTM's protocol) by virglrenderer's Neptune decoder and DXMT,
which turns the guest's Direct3D shaders into Metal ones with its own
LLVM-based converter. That is a large amount of young C and C++ code in
reach of the guest. So none of it runs in the VMM. Windows guests reach
vrend too: the 3D driver makes virgl contexts for its own copies, and
our OpenGL driver for Windows apps uses them like Linux's Mesa does
(nothing new on the host: any guest code could already ask for one).

### The GPU process

As browsers do, the renderer has a process of its own: `vroom gpu-helper`,
started by the VMM, one per VM, with OpenGL in it; Vulkan and Direct3D
contexts get a process each below it (next section). virglrenderer,
MoltenVK, DXMT and the GL driver are loaded there and only there; the
VMM never loads them and its
sandbox has no GPU rules at all (no AGX user clients, no Metal compiler
service, no shared memory but its own guest RAM).

What a bug in the 3D code can reach from the GPU process:

- the GPU (the AGX and IOSurface user clients, the Metal compiler
  service, framework reads: what Metal needs, nothing else);
- that VM's guest RAM, which it maps (shared memory the VMM hands over
  as fds; the guest has it all anyway) and the buffers it made itself;
- the two sockets to the VMM, and the one to the render server.

What it can't: disks, the network, shared folders, the clipboard, the
hypervisor (it has no vCPUs and no VM; a VM belongs to the process that
made it), the viewer, other VMs, any file of the user's, other processes
(no exec, no signals, no other process's shared memory). The sandbox goes
up after the libraries are loaded and before the first guest command.

What the VMM trusts from it: nothing it can't check. Replies are framed
with size limits; buffers it hands over are mapped at the size the guest
asked for, never more; frames come back through a shared buffer the VMM
made, copied out at the size the VMM asked for. If the GPU process dies,
the VMM sees the socket close: 3D stops for that VM (the guest gets GPU
errors, nothing else breaks) until the VM is started again.
`VROOM_GPU_IN_PROCESS=1` puts the renderer back in the VMM, with the GPU
rules in its sandbox (for debugging; macOS only has the GPU process).

Where vrend's GL calls go depends on `opengl_host`:

- `"apple"` (the default): macOS's own OpenGL 4.1. vrend's checks are the
  only ones before Apple's driver, and macOS's GL has no robust buffer
  access, so a shader reading out of bounds reads whatever GPU memory of
  the GPU process follows (its own guest's data) or crashes it. In
  exchange the guest gets OpenGL 4.1, which most Linux apps need.
- `"angle"`: ANGLE on Metal, which checks every call the way a browser
  checks WebGL before anything reaches Metal, so a vrend mistake meets a
  second check. Its contexts ask for robust buffer access (out-of-bounds
  reads give zeros). The guest gets OpenGL 2.1 only.

`opengl = false` leaves vrend and both GL drivers out entirely.

The GPU process loads virglrenderer by full path from its own directory
(next to `vroom`, or `Contents/Frameworks` when it is
`vroom.app/Contents/MacOS/vroom`, symlinks resolved), and only if it isn't
there from Homebrew's prefix, then dyld's search path.

Our Venus patches add a little code in reach of the guest, all in the GPU
process: filling null descriptors (they now never reach MoltenVK, which
would follow a null pointer), empty submissions for sync fd imports, and
for Zink a few format and feature substitutions. Without the semaphore
emulation, a guest could already crash the GPU process by calling
`vkWaitSemaphoreResourceMESA`, which MoltenVK has no function for. To show
a Vulkan window on macOS's OpenGL, vrend maps the frame's shared memory
read only and copies rows out of it; the row size, offset and height the
guest gives are checked against the memory's size first. Nothing new is
loaded and the sandbox rules don't change. Each Vulkan or Direct3D
context also has a thread in the GPU process that its worker wakes
through a pipe when fences finish; it reads the same shared fence
counters the millisecond poll read before, so a worker gains nothing new
by writing to that pipe.

On the GL side, texture clears (`VIRGL_CCMD_CLEAR_TEXTURE`) now reach the
host on macOS's OpenGL and ANGLE too, which lack the GL call: vrend checks
the box against the texture's level first, then uploads the texel through
its normal upload path from a scratch buffer of at most 4 MiB (one band of
rows), so a huge box costs time, not memory.

On macOS's OpenGL the guest is now told the GPU's real sample count (4)
instead of 1 (patch 0014), so ordinary apps (Chrome, antialiased WebGL,
games) make multisampled textures and resolve them with blits. A guest
could ask for those before too: vrend checked and created them the same
way. Now they are common, not rare. Snapshots don't keep their contents.

Any guest GL app using transform feedback could crash the GPU process
when it ended (macOS's `glEndTransformFeedback` with the wrong program
bound; patch 0015). The guest lost 3D until the VM restarted, nothing
more: the crash stays in the GPU process.

Transform feedback for Zink (patch 0016) adds, in the Venus worker:

- A SPIR-V rewriter (`vkr_xfb.c`) reading the guest's vertex shaders.
  Every read is checked against the module's size and id bound; anything
  it doesn't expect (structs captured, 64-bit outputs, strides over 2048,
  offsets past the stride, SPIR-V before 1.5) leaves the module as it was.
  300,000 mutated Zink shaders ran through it under AddressSanitizer and
  UBSan without a finding. Its output goes to MoltenVK's compiler, which
  read the guest's own SPIR-V before anyway.
- Shader stores through device addresses. The addresses and how many
  vertices may be written come only from the host, pushed right before
  each draw: the host's copy of the bound range (checked against the
  buffer's size, buffers made for transform feedback and bound to memory
  only) and the room left in it. The guest's own push constants into our
  48 bytes are cut off. The rewritten shader checks each vertex's slot
  against that count, so it writes inside the bound ranges. Zink devices
  now have `bufferDeviceAddress` on, as a Vulkan app on Venus could
  already ask for: a guest shader can write anywhere in its worker's GPU
  memory, which only holds that app's own objects.
- Command buffers keep a little state; layouts and query pools are found
  again by id, so a guest destroying them mid-recording gets nothing
  written rather than a dangling pointer.

### A process per Vulkan and Direct3D context

Each Vulkan (Venus) and Direct3D (Neptune) context of the guest, roughly
one per app using the GPU, runs in a process of its own:

```text
 GPU process (sandboxed)          render server (no sandbox)
 virgl GL contexts, proxy  ---->  vroom gpu-server: starts workers
   |  one socket per context           |
   v                                   v  (posix_spawn, fixed arguments)
 vroom gpu-worker, one per context, each in its own sandbox:
 the GPU, its own shared memory; Direct3D ones DXMT's shader cache
```

A worker loads virglrenderer and the one driver it needs (MoltenVK or
DXMT) from vroom's own directory (`Contents/Frameworks` in vroom.app),
never a search path, then sandboxes
itself (the GPU rules above, shared memory under its own process id
only, and for Direct3D the VM's shader cache) before it reads anything
from the guest. Its environment is cleared but for logging and the
drivers' debug switches, never a variable that names a library. A guest
app that breaks its driver lands in a process with only its own GPU
objects: it can't read another app's textures or buffers, nor the GL
state in the GPU process, nor guest RAM (workers never map it: Vulkan
and Direct3D buffers are shared memory the worker makes and hands to the
GPU process). Workers talk to the GPU process only, over their socket.

The render server has no sandbox, because on macOS a process can't
narrow the sandbox it inherits: whatever starts sandboxed workers must
have none. It is kept small: it reads four fixed-size requests from the
GPU process (start, reset, create and destroy a context), refuses
backends the GPU process didn't ask for and more than 256 workers,
starts only `vroom gpu-worker` with arguments it builds itself (the
guest's name for the context is cut to 31 letters, digits and `._-`),
and never loads virglrenderer or any GPU code. A GPU process taken over
can ask it for more workers, each sandboxed like the others: nothing it
didn't have. The server exits when the GPU process does, and kills its
workers.

A worker that crashes (or is killed) takes its context with it, nothing
else: the server logs it, the GPU process retires that context's fences
so the guest doesn't wait forever, and the other contexts go on. In a
Linux guest the Vulkan app sees its device lost and stops. In Windows,
an app's Direct3D device stops drawing; if the context was the desktop
compositor's, the screen stops updating until Windows restarts its
graphics (Win+Ctrl+Shift+B; tested: the desktop comes back). Holding the
fences back instead doesn't help: Windows' GPU watchdog doesn't fire
with this driver, the desktop just hangs.

Snapshots of Vulkan contexts (patch 0010) add three requests on each
worker's socket and three calls between the VMM and the GPU process. The
worker writes its context's state into a shared memory object of its own
(under one of the names its sandbox already allows) and hands it to the
GPU process, which hands it to the VMM. The VMM checks its size (at most
64 GiB, no more than the object holds) and stores it without reading it.
On restore the VMM puts that data in shared memory, and it goes down the
same way to a new worker, which maps it read only. The data comes from
the snapshot file, which anything that can write the VM's state
directory controls, so the worker checks it like guest input: each
record's length against what is left; contents against the size of the
resource, memory or image region they go to (regions are worked out again
from the image, never taken from the file); rings with the same layout
checks as `vkCreateRingMESA`; reply streams with the same checks as
`vkSetReplyCommandStreamMESA`. The kept commands are the guest's own
bytes, replayed through the same Venus decoder and handlers as when the
guest sent them (with the reply flag cleared). Workers still never map
guest RAM. The VMM checks what it keeps itself like the guest's own
requests: blob sizes and kinds, guest RAM ranges, and each mapping with
the same checks as `RESOURCE_MAP_BLOB`. Following commands adds code on
every command a guest sends, with a bound: past 4M commands or 1 GiB
kept, a context isn't followed any more.

OpenGL (virgl) contexts stay together in the GPU process: vrend shares
one GL context group across them (shared objects, one GL thread), so
splitting them would mean a GL driver per guest app and copying every
shared texture. A GL bug reaches every GL app of that VM, and guest RAM.

Costs: a new Vulkan or Direct3D context takes about 50 ms more (start
the process, load the driver, make a Metal device). A Windows desktop
with Edge open has 15 to 20 contexts, 10 to 60 MB each: 200 to 400 MB
more than with one process.

libepoxy is built to load ANGLE only from virglrenderer's own directory,
and Apple's OpenGL only by its system path, when the GPU process made a
CGL context current first; never a search path. DXMT (LGPL, a separate
dylib) is loaded from the same directory before the sandbox goes up, and
virglrenderer is patched to look for it only there, and to never start a
render server of its own. Shared memory: the sandbox lets the GPU
process and each worker create, map and unlink shared memory only under
names carrying its own process id and virglrenderer's prefixes, and
open no other (not even read: another app's or another worker's shared
memory is out of reach; `VROOM_SANDBOX_SELFTEST=1` checks it). Metal's
shader cache, which the user's other apps share, is read only, so a
process taken over can't plant GPU code for them. DXMT's cache (Metal
versions of guest shaders) is per VM, in its state dir: one guest can't
plant shaders for another.

With 3D, the GPU's host-visible window (8 GiB of guest address space) is
backed by reserved memory where no buffer is mapped, as a real card's
memory would be. Pages the guest writes there become real host memory: a
guest can grow the VMM by up to 8 GiB that way, on top of its RAM.

Snapshots with GL in them hold the guest's GPU contents (textures read
back, so whatever was on screen) next to its RAM, in the state dir. On
restore, the GL state goes back through virglrenderer's checks like any
command the guest sends, and resource sizes, backing ranges and counts get
the same limits as guest commands: a tampered snapshot can do what the
guest could, no more.

Use 3D only for guests that need it: the GPU process confines the 3D
code, but a guest that breaks out of it still holds the host GPU, and
GPU drivers are kernel code.

## The viewer socket

A VM started from the app accepts a new window on a Unix socket, so it
survives the app crashing. The socket is in the user's own temp dir
(`$TMPDIR/vroom/`, 0700), itself 0600, bound before the sandbox goes up.
Whoever connects gets the guest's screen and sends it keys and commands:
any process running as the user can, which matches what the user's
processes can already do to the VM's files. One viewer at a time; the
guest has no way to reach it. To stop a VM that hangs, the app reads the
pid the VMM wrote into its state lock and signals it only if that process
is a `vroom` executable (the VMM can write that file).

## Several monitors

What changes with `[display] monitors`:

- The shared framebuffer gets a slot per monitor (8K each, 4 at most);
  every write and read is checked against its slot, so nothing written
  for one monitor reaches another. The app maps what the file holds,
  never more than 4 slots.
- The viewer messages for the other monitors (mode, damage, window
  size, pointer) name a monitor; numbers outside 1..3 are refused, sizes
  come both or neither.
- virtio-gpu takes up to 4 scanouts; every scanout id is checked, every
  rect is clipped to its scanout's view, and a snapshot's scanouts are
  checked like commands.
- The guest agent gets the layout (sizes and places, bounded) and, on
  Windows, the pointer on monitors the USB tablet doesn't reach. Both go
  host to guest only; the agent's session helper injects that pointer
  with SendInput, so it can't reach elevated windows or the secure
  desktop (UAC), as with any app of the user.
- Windows' extra GPUs carry 32 MiB of plain memory in BAR 0 (reserved,
  only what the guest writes is ever memory): its 2D driver clears it
  as a framebuffer. The device never reads it.
- On Windows the agent's service picks a display driver per GPU from the
  driver store (the 2D one on GPUs without 3D, the 3D one on the first
  when the host has 3D), as Device Manager does. It only chooses between
  drivers that are already installed.

## Guest agent (Windows)

The VirtIO serial port only admits Administrators and SYSTEM, so the agent
is split. A LocalSystem service owns the port and does clock sync and
clean shutdown. An unprivileged helper in the console session does
clipboard, resolution and UI scale, and talks to the service over a named pipe:
SYSTEM/Administrators/interactive users only, remote clients refused,
only clients from the active console session served. The helper only
talks to a pipe server in session 0, so a user process squatting the pipe
name gets nothing. The service accepts nothing from the helper but
clipboard text (size-capped), answers about dropped files, and window
and app lists while the host's window is in Coherence (checked like the
rest).

The Linux agent has the same split: a hardened systemd service
(`CAP_SYS_TIME` only, read-only file system, Unix sockets only) and a
session helper started with the desktop. They meet on
`/run/vroom-agent.sock`; the service serves only the user logind reports
active on seat0, and the helper only talks to a root-owned peer.

## Fuzzing

`fuzz/` has cargo-fuzz targets for what a guest (or a downloaded image)
controls: agent protocol, the viewer protocol (both directions), network frames (the NAT's parsers and the MAC translation), virtqueue chains over arbitrary
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
come from the input), and the video helper's stream parsing (NAL units,
parameter sets, display order, VP9 superframes and frame headers, AV1
OBUs and sequence headers) and its protocol both ways (`video`, 21M runs
clean; the virtio-media device has a junk-command test of its own). Run with
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
- Fuzzing doesn't cover Venus, virgl or Neptune (3D) command streams:
  that is virglrenderer's C code (and DXMT's C++ for Neptune), which runs
  in the GPU process.

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
- With `video_decode`: a virtio-media device (id 48) named "Video
  decoder", whose formats (the codecs this Mac decodes), sizes and levels
  are VideoToolbox's. Our VA-API driver names itself "vroom".
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
  the host GPU (MoltenVK on Apple). Windows guests with 3D see a "Red Hat
  VirtIO GPU 3D controller" whose Direct3D runs on Metal underneath. OpenGL's renderer name follows
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

