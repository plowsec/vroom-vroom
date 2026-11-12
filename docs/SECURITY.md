# Security model

## Threat model

Guest = hostile (compromised OS or malware in it). Goals, in order:

1. Guest can't get code execution on the host.
2. If it does (VMM bug), it lands in a process that can do almost nothing.
3. Guest can't read host data it wasn't given (files, clipboard, network).

Out of scope: malicious host; CPU side channels (Spectre-class: hypervisor's and OS's job); guest DoS against its own VM.

## Layer 1: small, careful device code

- One Rust process per VM.
- `forbid(unsafe_code)`: `devices`, `virtio`, `arch`, `net`, `config`, `snapshot`, `proto`, `video`.
- Unsafe only in `hv`, `memory`, `sandbox`, `gpu3d`, `ipc`, `usb` (libusb, loaded by the USB helper only; IOKit listing) and a few FFI shims (VideoToolbox in the video helper among them), each with `// SAFETY:` notes.
- Guest memory only via bounds-checked `GuestMemory`; no raw pointers into guest RAM in device code.
- Every guest value validated: virtqueue rings (loops, sizes, indirect tables), NVMe PRP lists, xHCI rings and contexts, GPU resources (sizes, counts, rects, backing entries), qcow2 headers and L1/L2 tables, network packets, agent messages (size caps, UTF-8).
- Hard caps on guest-driven allocation: GPU resource memory, pending fences, queue sizes, NAT connections and buffers (~80 MiB max), fragment memory, disk requests in flight.
- Strict config: unknown keys are errors. `vroom check` (and every start) flags risky valid choices: sandbox off, writable or home-wide shares, LAN-reachable port forwards, host loopback open, bridged networking.
- Logging capped at 50 lines/s (errors excepted): a guest can't flood the terminal or fill a log disk.
- Guest serial and `vroom guest exec` output on a terminal: OSC 52 (clipboard), DCS, SOS, PM, APC, ENQ dropped; CSI, other OSC pass (`cli/src/term.rs`).
- qcow2 backing files: only the image's own dir or allow-listed ones (exact dirs, not below); no protocol prefixes; chain depth capped; format from the backing-format extension, never sniffed (missing: refused, `qemu-img rebase -u -F`).
- Writable VM disk chains (VMM, prune, app compact): backing = a plain file in `<state>/disks` (qcow2) or exactly the configured image; a writable image with a backing file of its own is refused (the VMM could have rewritten its header).
- VMDK import (read-only): headers, grain tables, descriptors range-checked and capped like qcow2; extent files plain names next to their descriptor (no symlinks out); parents must be `.vmdk`, CIDs must match, chains capped, loops refused.

## Layer 2: sandbox

VMM drops privileges after building the VM, before the guest runs.

| | |
|---|---|
| macOS (Seatbelt, `sandbox_init`) | deny by default. Allowed: VM's disk images and state dir; outbound sockets only if NAT is used; accept only on configured port-forward sockets; pasteboard only if clipboard sync is on. No exec, no new listeners, no user files (tested: `/etc/passwd`, `~/.ssh`, `exec`, `bind` all fail) |
| Linux (Landlock + seccomp) | Landlock: same files and dirs, on every thread, or the VM doesn't start (no Landlock = error, not a warning). seccomp: only syscalls the VMM uses (no `bind`, `execve`, `ptrace`...); signals only to own threads (`tgkill` with our pid); new sockets IPv4/IPv6 only (no Unix, netlink, packet). Anything else kills the process. Bad arguments get EPERM: tty ioctls (`TIOCSTI`, `TIOCLINUX`, job control, console/VT), `F_SETOWN`/`F_SETSIG`/`FIOASYNC`, `clone` without `CLONE_THREAD` or with `CLONE_NEW*` (no fork), `socketpair` non-Unix, `sendto` with an address, risky `prctl`s. `clone3`: ENOSYS (glibc falls back to `clone`) |

Network reach of the VMM itself:

- macOS: TCP and UDP only (no Unix sockets: no ssh-agent, no docker.sock); host loopback denied unless `allow_host_loopback` (port 53 stays open if the host resolver is local). Loopback = 127.0.0.1, ::1 and the Mac's own addresses. Tested: `cargo test -p vroom-sandbox` (a bare `(remote ip)` allow left loopback open on macOS 27).
- Linux: seccomp can't see addresses; loopback refused by the NAT code only.

Linux kernel needed (else `--no-sandbox`):

| Landlock ABI | Kernel | When |
|---|---|---|
| 2 (`Refer`) | 5.19 | process single-threaded at lock-down (swtpm child) |
| 8 (`TSYNC`) | 7.0 | other threads already run: VMM (vCPUs), GPU process, USB helper (libusb's event thread) |

Known gaps (Linux):

- `sendmsg` can carry an address (inside the struct, seccomp can't read it): an existing Unix datagram fd may still reach a pathname socket.
- `chmod`/`fchmodat` allowed (layer freeze, snapshot dirs); Landlock doesn't cover chmod: any path the user owns can be chmod'ed.
- `ioctl`, `prctl`: deny-lists, not allow-lists (in-process Vulkan drivers).

`--no-sandbox`: debugging only, prints a warning.

State dir (`<vm>.state`) = hostile input for everything outside the sandbox (next VMM start, app, CLI). `vroom_state::hostile`:

- Reads: `O_NOFOLLOW|O_NONBLOCK`, regular file, one link, size cap (`uuid`, disk pointers, `reidentify`, `viewer`, `control`, snapshot blobs).
- Writes: new temp file (`O_EXCL|O_NOFOLLOW`) + rename (`uuid`, pointers, `viewer`, `control`). NVRAM opened `O_NOFOLLOW`, regular file.
- Dirs (`disks`, `tpm`, `shaders`, snapshot dirs): real dirs (`symlink_metadata`), not links. `tpm/`: non-plain swtpm files removed before swtpm starts.
- Pointer layer names: only `disk<i>-<secs>-<nanos>.qcow2` (plain file) or the base image's name.
- Sockets: `$XDG_RUNTIME_DIR/vroom` (Linux, if set) or `$TMPDIR/vroom`; must be ours, 0700. App/CLI connect only to the path derived from the state dir, never one a file names.

Self-test:

```bash
VROOM_SANDBOX_SELFTEST=1 vroom run vm.toml
VROOM_SELFTEST_LOOPBACK_PORT=<a port you listen on>   # for the loopback probe
```

Once locked down, the VMM tries: read `/etc/passwd` and `~/.ssh`, exec, open a listener, reach host loopback, connect to `$SSH_AUTH_SOCK`, reach the internet; prints results. Linux: the Unix-socket probe ends the process (seccomp kill) = expected.

## Clipboard

- Text only, size capped, sync only while the VM window has focus. Background guest can't read host copies or push to your clipboard. Headless: explicit push/pull.
- `vroom-gui` is not sandboxed, so it treats the VMM as guest-exposed:
  - applies the VM config's clipboard policy itself (never sends host clipboard if `host_to_guest` off; never sets it if `guest_to_host` off; focus and size limits)
  - caps and cleans status text
  - checks every mode and rect against the shared framebuffer
  - Linux: maps only a framebuffer memfd sealed against shrinking (a shrinking VMM would crash the GUI)
- The app (same binary, no args) reads VM state dirs the sandboxed VMM can write: parses only snapshot manifests (JSON, size checked), checks lock and suspend marker.
- Pictures it shows: its own data dir, taken from the shared framebuffer; never image files a VMM could plant.
- Snapshot names from VMM status messages must be valid names before becoming file names.

## Drag and drop

Mac → guest:

- Files read by the app (the drag target), never the VMM (its sandbox opens no user file).
- App writes them into a pipe handed to the VMM; VMM checks each frame (this drop's id, file entries and data only, size-capped), passes it to the agent.
- While hovering: guest gets names only. Dragging across a window leaks no content.
- Agent rechecks every path (relative, no `..`, no names Windows reads another way, under an announced name), writes no more bytes than announced.
- Files land in the logged-in user's staging folder (Windows: temp; Linux: cache), then where the drop lands.
- Agent's drag window (Windows, X11) only starts a drag of those files.

Guest → Mac (guest is the one to distrust):

- Guest only answers a question: asked once per mouse-button press, when the mouse leaves the guest screen with the button held; VMM takes one answer per question.
- A name list alone starts a Mac drag but copies nothing. Data flows only when something on the Mac takes the drop and asks for an item, and only that item.
- Names and sizes checked twice, VMM and app (vroom-proto `TreeCheck`): `drop_name_ok` names (no `/`, `..`, control, bidi or invisible characters); asked-for item first; every folder entry announced before it; no more bytes than announced; ≤200,000 entries. Frames of an abandoned item carry an old stream number: dropped. A rule break loses the drag.
- App (not VMM) writes the files where the Mac drop says: new files/folders only (`O_EXCL`, `O_NOFOLLOW`, `mkdir`), never through a link or over existing (taken name gets a number), removes partial output on failure, stops at <2 GB free.
- Every file and folder written gets `com.apple.quarantine` (`0081;<hex time>;vroom;`): Gatekeeper checks a guest-made app or script. Same for `vroom guest pull`. Can't set it: that file fails.
- Agent reads only paths the desktop handed its drop target; skips links inside folders.

## Coherence

The guest decides which windows appear on the Mac, where, with what title. Treated as the guest's word:

- Window list checked 3× (agent protocol in VMM, viewer protocol in app, app itself): ≤64 windows, unique ids, titles ≤256 bytes UTF-8, sizes ≤65536 px. App clips every window to the guest screen, shows ≤24. Titles lose control and direction-changing characters, cut to 80 chars. Flooding costs one queued list in VMM, one in app.
- App icons: raw RGBA, ≤64x64, exact byte count, checked by VMM and app decoders and again before becoming an image. No image format decoded on the Mac. Only icons of last-list windows passed on; flood = one waiting icon per window (≤128) in VMM and app. Shown in our bar next to the title; never in the strip, Dock, menu bar.
- Every Coherence window has a strip drawn by the app above the guest picture: VM name + window title. Guest can't draw over it, can't open a window without it. macOS window title starts with the VM name (Mission Control, Window menu).
- Guest screen placed inside the Mac screen's free area: can't cover menu bar, Dock, Coherence bar; normal window level (never above other apps by level).
- New guest windows open behind other apps' windows while vroom isn't frontmost; guest window order applied only while it is: no popping over the user's work (still possible while the user works in one of its windows).
- Clicking a Mac window activates its guest window; menus and untitled windows never activated. Guest asked to act only on last-list windows (VMM and agent check), only: front, minimize, polite close (`SC_CLOSE`, app may refuse), move.
- Clipboard unchanged: sync only while a VM window (or the bar) has the keyboard.
- Bar app field sends a name: one line of plain text ≤256 bytes, checked by viewer protocol, VMM, agent. Nothing on the Mac is started. Typed keys stay out of the guest until Enter.
- In the guest, the name only picks an installed app:
  - Windows: Start menu `.lnk` (ProgramData's or the user's) or a `shell:AppsFolder` app (Store, by AppUserModelID). Opened with `ShellExecute` (shortcut or `shell:AppsFolder\<id>`) from the user's session helper.
  - Linux: menu desktop entry (XDG data dirs; hidden ones, NoDisplay, Hidden, OnlyShowIn another desktop, excluded). Run with `gio launch` or its Exec line split into words (field codes out), never via a shell, as the session user. Agent running as root refuses.
  - The name is only compared with app names; never a command line, argument, path or file. Boundary: host starts what the user could start from the guest menu, nothing else, no files, no options.
  - Windows fallback, only if no app matches and the name is plain (letters, digits, spaces, a few marks: no path, no command line): typed into Start search as the user would; may pick something else (web search). Host has the guest keyboard anyway.
- `Apps` list (suggestions) is the guest's word too: ≤512, each name a sendable line, given once; icons raw RGBA ≤32x32, exact bytes, checked by VMM and app decoders and again. Passed only in Coherence, shown only as suggestions: a lying guest can make the field suggest a name, which goes back to the guest's own matching. Guest reads its icons/logos itself (PNG ≤4 MB, ≤1024x1024; Store manifests without DTDs); no image format decoded on the Mac.
- "Coherence can't work here" (Wayland): one plain line, cleaned again before display.
- Guest screen follows the bar to another Mac screen only where the user put the bar, never on the guest's say. Windows the user drags off the guest screen stay on it in the guest.
- Guest window events (Windows hooks, X11 events) only wake the helper, ≤ every 10 ms: a window-moving app costs helper CPU and a list per look, coalesced.
- X11: helper acts like a taskbar (client messages to the WM), only on last-list windows. May hide Ubuntu's dock (user setting, restored) and GNOME's top bar (enables the tools' own Shell extension, which does nothing else; disables after).

Left: a guest app can draw anything below the strip, including a fake Mac dialog. The strip says whose window it is: read it before typing a password.

## Network

- NAT: userspace proxy; guest never gets raw access to the host network stack.
- Host loopback unreachable unless `allow_host_loopback`. Same for the host's own addresses (LAN, VPN, `bridgeN`): reach `*`-bound services. Checked per new flow (UDP connect probe, nothing sent; unknown = refused). `block_private`: guest off your LAN.
- Bridged, host-only: via `vroom-netd`: allow-listed interfaces, source-MAC anti-spoofing ([the root helper](#the-root-helper-vroom-netd)).

### Network changes while the VM runs

Cable and network change live ([NETWORKING.md](NETWORKING.md#changing-adapters-while-the-vm-runs)). The sandbox stays as set at start; nothing here widens it.

- **Who asks**: the VM window only (`ToVmm::NetConnect`, `ToVmm::NetSet`, `ToVmm::NetAdd`, `ToVmm::NetRemove`; viewer messages 40, 41, 95, 96). Guest can't send them. VMM checks each like guest input: adapter index, mode, names (VM file rules), forwards (count, addresses), MAC (unicast, not another adapter's), bridged/host-only change comes with its socket.
- **Add/remove adapters** (PCI VM): same virtio-net device, same backends as at start, in a PCIe hot plug slot (below), ≤8 adapters. Remove only works on hot-added ones; a start-time adapter just has its cable pulled.
- **Bridged, host-only**: sandboxed VMM can't open Unix sockets, can't reach vroom-netd after start; stays so.
  - App asks netd for the port as the user (netd checks and per-user limits apply), hands the VMM the frame socket over the window connection (SCM_RIGHTS), as for USB helpers.
  - VMM gets what it gets at start: one datagram socket of Ethernet frames for one adapter, behind netd's interface allow-list and source-MAC check.
  - Opening sockets itself would need Unix sockets in its sandbox (ssh-agent, docker.sock); a handed-over socket reaches only netd's port.
  - MAC in the request: the one the VMM reports for the adapter, made locally administered. netd refuses other MACs from anyone; macOS vmnet picks its own anyway.
- **NAT**: the VMM's own, only if its sandbox already allows outbound connections (a NAT adapter at start). VM started with only bridged/host-only: no network in its sandbox, none later; NAT waits for next start. `allow_host_loopback` turned on later works only if an adapter had it at start (sandbox refuses loopback otherwise; NAT code doesn't try).
- **Port forwards** need listening sockets, never allowed by the sandbox: new ones wait for next start. Start-time NAT kept (forwards included) while the adapter is on another network: listeners stay open, a forwarded connection waits until the adapter is back on NAT.
- **MAC translation**: vmnet port passing another MAC than the guest's (adapter started on NAT): VMM rewrites in frames: Ethernet, ARP, DHCP client address, IPv6 ND options (`vroom-net/src/mac.rs`). Every offset checked; unparseable frames pass unchanged. netd still drops frames whose source isn't the port's MAC: no sending as another machine.
- Cable out: VMM drops frames both ways on that adapter.

### PCI hot plug slots

PCI VMs: 4 PCIe root ports ([ARCHITECTURE.md](ARCHITECTURE.md#pci-hot-plug)). Guest reach:

- Each port's config space: bridge header (bus numbers, memory windows: plain registers, masked as on real hardware), PCIe capability (Slot Control/Status, Link Status), MSI. In `devices` (`forbid(unsafe_code)`), fixed size, every write masked.
- Config cycles for other buses: routed to the port whose secondary bus the guest set, device 0 only, only while the slot has power; anything else reads all ones. Two ports given the same bus: the first one's device.
- Memory windows not enforced: a device behind a port answers at the BAR the guest gave it (as on the root bus). Guest can only point its own devices at its own address space.
- Slot state machine: guest can't plug anything in (host only, from the VM window); can't keep a device it was asked to return (pulled after 15 s). Powering a slot off unasked only drops its link. Removed devices dropped after every lock is released.
- Slot events: one MSI per rising edge, or a level INTx: no more interrupts than commands written.
- Snapshot restores a device into a slot only if the VM was built with it there (manifest `net.hotplug`, checked: slot numbers in range, each once).

## Shared folders

- Off unless configured; read-only unless `readonly = false`.
- Host side (`vroom-fs`): every guest path walked one component at a time with `openat(O_NOFOLLOW)` from the share root fd. A symlink anywhere ends the walk: neither guest-planted nor host-swapped-mid-walk links lead outside.
- Refused names: `/`, NUL, `.`, `..`. Only regular files and directories opened (no FIFOs, devices, sockets). Guest can't create links or device nodes.
- Capped: node, handle, directory listing counts; path length 4096 bytes; total memory of all node paths.
- Sandbox grants only the share's subtree. Read-only shares: read only. Writable: no set-id bits, owner changes, ACLs, file flags, extended attributes.
- Files the guest writes to a share carry no quarantine (sandbox denies xattr writes): treat them as untrusted downloads.
- FUSE server fuzzed, as both clients, on both kinds of share.
- Windows guests (virtio-win virtiofs.exe), same server:
  - Windows-special names (`name:stream`, `CON`, trailing dots) are plain names on Mac/Linux hosts: stay inside the share. Same for Mac `file/..namedfork/rsrc` (a file is never walked through).
  - Only extra name accepted: `.` = this directory (client uses it for the root). `..` still refused.
  - Guest needs WinFsp: setup installs one pinned version, checks SHA-256 first.

## Sound

- Off unless `[audio] output` or `input`.
- PCM over a datagram socket to `vroom audio-out`, which plays it: CoreAudio and its plug-ins never run in the VMM.
- Helper Seatbelt profile: system libraries, CoreAudio services, audio user client only. No user files, network, exec.
- VMM never blocks on it (drops audio instead).
- Guest input parsing: virtio-snd chains as virtqueues; HD Audio registers of any width/offset, CORB/RIRB rings (pointers masked to ring size, one pass per doorbell), buffer descriptor lists (index masked; empty or unreachable entries stop the stream with an error), DMA amounts capped per tick. Both fuzzed.

### Microphone

Off by default.

- `[audio] input = false` (default): sound card has no input (no capture stream, no mic pin); helper started without `--input`, refuses to record; its profile lacks `device-microphone`. All fixed before the guest runs; a guest owning the VMM can't change them.
- `input = true`: helper may record, profile allows the mic. VMM itself has no mic access; only the helper talks to CoreAudio (or ALSA).
- VMM asks for audio only while: a VM window is open (viewer connected; never headless), the VM runs (pause stops it), the guest records. Window shows "Microphone in use" whenever the VMM asked.
- Gap: with `input = true`, gate and indicator are enforced by the VMM. A guest that broke into the VMM could ask the helper for the mic any time the VM runs, without telling the window. macOS's orange menu-bar dot can't be faked. Hence opt-in.
- macOS grants mic permission to the app that started vroom (Terminal, iTerm, VS Code...): a yes covers everything started from it. [GUIDE.md](GUIDE.md#microphone).
- Helper replies parsed like guest input (sizes, whole frames, generation number drops audio from earlier requests); ≤¼ s buffered.

### Camera

- Off unless `camera = true`: USB camera in the guest + `vroom camera-helper` with its own Seatbelt profile: camera, system camera services, frame buffers (IOSurface), system library reads. No user files, writes, network, exec.
- VMM never touches the camera. Asks for frames only while a window is open and the guest reads them (camera off 2 s after the guest stops); window shows "Camera in use" whenever it asked.
- Same gap as the mic: a VMM-escaped guest could ask for frames while the VM runs without telling the window; macOS's green camera light can't be faked.
- Helper frames checked like guest input (size, whole frame).

## Video decoding

Off unless `[display] video_decode = true`. A video stream is about the most hostile input there is (big, old C decoders): decoding never happens in the VMM.

```text
guest V4L2 decoder --virtio-media--> VMM device (Rust)  --socket--> vroom video-helper --XPC--> Apple's decoder service
                                     checks every ioctl,             parses the stream,          (macOS's own sandbox)
                                     copies bitstreams out,          then VideoToolbox;
                                     grants picture buffers          writes pictures into
                                                                     the granted buffers
```

VMM device (`virtio/src/devices/media`, no unsafe):

- Speaks V4L2 only. Every ioctl size-checked; queue types, buffer indices, memory types, control values checked.
- Limits: ≤8 decoders; per decoder 16 bitstream buffers (≤8 MiB each), 24 picture buffers (≤4096x2304).
- Buffers in one 1 GiB shared window the guest maps; guest can map only our buffers' offsets.
- Bitstream copied out of the window before going anywhere (no change under the helper). ≤16 MiB or 32 buffers per decoder waiting at the helper. Pending events capped.

Helper (`vroom video-helper`, one per VM, started before the VMM sandbox):

- Own Seatbelt profile: system libraries (read), VideoToolbox decoder XPC service (the only lookup allowed), IOSurface. No files, network, exec, GPU, writes.
- Holds its socket + the buffer window (shm object passed at start); writes pictures straight into guest buffers there. Window holds only decoder buffers, never guest RAM.
- Parses the stream before VideoToolbox (`crates/video`, fuzzed): H.264/HEVC NAL units (≤4096 per buffer), parameter sets (≤4 KiB each, ids within standard ranges), access units (≤256 per buffer), slice headers for display order; VP9 superframe indexes and frame headers up to frame size; AV1 OBU and sequence headers (≤1 KiB).
- VideoToolbox gets: parameter sets + length-prefixed access units; VP9 frames whose header we read; AV1 temporal units without delimiters/padding + a sequence header we parsed.

Each side treats the other as hostile:

- Helper accepts from the VMM only known message kinds within size (8 MiB bitstream; grant ≤ one 4K P010 picture), 8 sessions, 64 queued buffers and 64 grants each. Writes only into a grant inside the window and big enough.
- VMM accepts from the helper only pictures in the oldest granted buffer (helper must name it), current epoch (changes when guest buffers do), exact size for the stream format; formats must be valid size/depth. >256 unread messages or a write blocked 2 s: helper counted gone; guest decoders fail `EIO`, rest of VM runs. VMM never reads a picture.
- Stream-compromised helper: can write anywhere in the window (that VM's other decoders' buffers, which the guest sees anyway). Can't reach VMM memory, guest RAM, other devices.

Snapshots:

- Keep each decoder's V4L2 side + ≤16 KiB helper resume state (parameter sets, VP9 key frame header, AV1 sequence header). VMM doesn't read it; after restore hands it to the new helper, which parses it like any stream.
- Snapshot = a file: on restore every field rechecked (sizes; buffer offsets inside the window, non-overlapping; queues naming real buffers). Inconsistent decoder state fails.

Reach of a stream-triggered bug:

- helper: VideoToolbox client side, its decoder service, IOSurfaces; that VM's other decoders' streams and buffers. Nothing else of the user's.
- Apple's decoder service (`com.apple.coremedia.videodecoder`, in front of the hardware decoder's kernel driver): same reach as a video opened in Safari or QuickTime. Apple's code, shared by every app: the real cost of enabling this.

Memory a guest can cause: window (≤1 GiB, only touched); helper per decoder ≤16 MiB bitstream + 24 pictures as VideoToolbox buffers (8 waiting for a guest buffer, ≤16 held for display order): ~350 MB for a 4K decoder.

Guest side: Google's virtio-media driver, out of tree. `scripts/guest/install-virtio-media.sh` downloads one pinned commit, checks SHA-256 before building. Our VA-API driver (`scripts/guest/vaapi`) runs in guest players, talks only to that V4L2 decoder (and the guest GPU for dma-bufs): nothing new host side.

## Video encoding

Off unless `[display] video_encode = true`. Raw frames are just pixels: nothing parsed. Guest controls encoder settings and buffer placement; both checked.

```text
guest V4L2 encoder --virtio-media--> VMM device (Rust)   --socket--> vroom video-helper --encode --XPC--> Apple's encoder service
                                     checks every ioctl,              reads granted frames,                (macOS's own sandbox)
                                     every control value,             VideoToolbox; writes
                                     grants coded buffers             coded frames into grants
```

Device: second virtio-media device, own buffer window (decoder's untouched), same code.

- Every ioctl size-checked. ≤8 encoders; per encoder 16 raw + 16 coded buffers (≤8 MiB each), ≤4 raw frames at the helper; ≤64 controls per request.
- Refused: formats it doesn't make. Sizes even, 16–4096 (≤4096x2304). Frame rates 1 per 1000 s to 240/s. Bitrates 1 kbit/s–400 Mbit/s. Profiles, levels, menus: listed values only. Stream-changing settings need queues stopped.
- Settings go to the helper as one fixed-size message both sides check identically (`EncConfig::valid`).

Encoder helper (`vroom video-helper --encode`), separate process:

- Profile: the decoder's, minus Apple's decoder service, plus its encoder service (`com.apple.coremedia.videoencoder`) and the user's trust agent (`com.apple.trustd.agent`, Security's certificate service, reachable by every app; VideoToolbox finds no hardware encoder without it). No files, network, exec, GPU, writes.
- Reads a raw frame only where the VMM said (inside the window, session layout size); writes a coded frame only into a grant inside the window that fits.
- Accepts only known messages within size, a consistent config, 8 sessions × 64 frames × 64 grants.
- Coded frames come from VideoToolbox but are parsed as hostile (length-prefixed NAL → Annex B, every length checked, `video/src/nal.rs`, fuzzed).

VMM:

- Takes from the helper only coded frames in the oldest granted buffer, current epoch, ≤ that buffer's size.
- Returns a raw buffer to the guest only once the helper says it read it.
- Never reads a frame or coded frame. Guest changing a frame during the copy spoils that frame's pixels, nothing else.

Snapshot keeps each encoder's settings, controls, buffers; rechecked on restore like the decoder's (inconsistent state fails that encoder in the guest).

Bug reach: helper: VideoToolbox client side, that VM's encoder buffers. Apple's encoder service (in front of the hardware encoder driver): same reach as any app encoding video (FaceTime call, screen recording). Memory: window (≤1 GiB, touched only); helper per encoder ≤8 coded frames held (≤8 MiB each) + VideoToolbox's pixel buffer pool.

Guest side: same virtio-media driver; our VA-API driver's encoder half talks to that V4L2 encoder only.

## USB devices

Off unless `[[usb]]` is listed or a device is connected from the VM window.

- VMM never touches the host USB stack: each device gets `vroom usb-host`, opening that one device with libusb.
- VM-file helpers: started by the VMM before lockdown. Devices connected while running: helper started by the app (or vroom-netd, below), socket passed over the viewer connection. VMM can start nothing; gains only one more socket to an equally limited helper.
- Protocol: submit, cancel, reset; back: attached/detached, transfer results.

Helper treats the VMM as hostile:

- Opens only the device on its command line (vendor, product, serial if given), chosen by VMM config before the sandbox. Protocol can't name another device, list devices, or change the address.
- Requests checked: ≤1 MiB per transfer, ≤64 in flight; endpoints only of the guest-selected configuration and alternate settings, with the descriptor-given type. Isochronous: ≤128 packets, none larger than the endpoint's per-interval max.
- Configuration, interface, halt changes via libusb's own calls; SET_ADDRESS refused; hubs never opened.
- Malformed message ends the connection.

VMM checks the helper:

- Answers must match a sent transfer (size, direction; isochronous: packet count and each size), never write past guest buffers.
- ≤8 transfers in flight per endpoint, 32 per device.
- Dead or misbehaving helper = unplugged device. vCPUs never wait on a helper.

Helper sandbox:

- macOS (Seatbelt): system libraries and libusb, IOKit registry, USB device and interface user clients only. No files, network, exec, other drivers. Seatbelt can't tell USB devices apart: a compromised helper could open other USB devices a normal user may open (none held by a macOS driver: not keyboards, mice, disks).
- Linux: no_new_privs + Landlock (`/dev/bus/usb`, sysfs only; not enforced on every thread = helper exits). seccomp deny-list (EPERM): exec, ptrace, modules, kexec, mount, namespaces, `bpf`, `perf_event_open`, io_uring, set*id, new sockets, keyrings, signals to other processes, plus the VMM's argument checks; x86_64: x32 syscalls killed. Can open any USB device node your user may (udev rules decide).

Guest gets full control of a given device (any request, firmware updates included): pass only devices you'd plug into that machine.

Test devices (`VROOM_USB_FAKE`: serial port, sound card, USB stick) run inside the helper in place of libusb, behind the same checks. The stick parses guest SCSI there: fixed-size blocks, every offset checked against its RAM disk.

### USB disks: their files, or the disk

macOS 15.3+ refuses USB storage to programs without Full Disk Access, root included. Two ways that don't pass the USB device and need no root helper (GUIDE, "USB disks"):

"Share its files":

- App (unsandboxed, the user's) opens `/Volumes/NAME`, issues a sandbox extension for that one path: a kernel-signed token of our class (`org.vroom.usb-files.read` or `.read-write`). Passes dir fd + token over the viewer socket.
- VMM profile allows file access under extensions of those classes, nowhere else new: no token from the app, no reach.
- Token arriving during VMM setup (as for a VM file's disk): kept, consumed once the sandbox is on (one consumed before would be lost with the new sandbox).
- Seatbelt checks paths: a dir fd alone isn't enough; the token lets the VMM in, only below that dir.
- Served by the shared-folder code (component walks with `openat(O_NOFOLLOW)`, no symlinks, no `..`); read-only if asked or if macOS has the volume read-only.
- Disconnect: VMM drops the share (fds close at once), releases the token.
- Gaps: a guest-owned VMM could keep the token: read access to that volume while the VMM lives. Token names the path; whether the kernel also binds it to the volume (a later mount under the same name): not checked. Extension calls are private API (Safari, WebKit use them): a macOS update could change them; the share would fail, not open up.
- Guest side: two virtio-fs devices on every Mac-started VM (`vroom-usb0`, `vroom-usb1`), empty until shared. Agent mounts on VMM request (tag of that form; volume name checked like a file name: no `/`, `..`, leading dot or dash). Linux via `systemd-run` (agent service has no mount privilege), arguments passed as is, no shell.

"Use as a disk":

- App finds the disk in IOKit below the chosen USB device (vendor, product, serial); takes it only if macOS calls it removable or ejectable, its storage device doesn't say internal, and nothing of it is mounted where macOS lives.
- Unmounts (`diskutil unmountDisk`, refuses while files are open), checks the same disk is still there, opens `/dev/rdiskN`: directly if the user owns the node (disk images), else via `/usr/libexec/authopen` (admin password, fd passed back over a socket). Fd must be a character device with that disk's device number.
- VMM gets the fd over the viewer socket; checks it's a character device (tests: plain file); reads size with disk ioctls. Profile allows ioctls on `/dev/rdisk*` only, only on given fds (it can open no disk).
- Served as a plain USB stick: test stick's Bulk-Only/SCSI code in a VMM thread with the fd as store, behind the helper's request checks (endpoints, sizes, in-flight). Every block number and length checked against disk size; whole-block I/O only (fuzzer target `usb_proto` drives it as a hostile VMM would).
- Disconnect: VMM flushes (disk cache too), closes the fd, then tells the app, which has macOS remount.
- Disk image whose partitions don't return: app opens `/dev/rdiskN` for writing and closes it (macOS rereads the partition table). Writes nothing; only while nothing of that disk is mounted; opens no node it can't open as the user (a USB stick's is root's).
- Guest gets all of that disk, read and overwrite (format, plant a boot sector): what was asked. No other disks: VMM holds one fd. vroom-netd not involved: no root process opens disks.

macOS privacy: reading a removable volume (and probably opening a removable disk) = "Removable Volumes" permission, asked once for vroom (`Info.plist` says why). Denied: the app says where to allow it.

xHCI always has 8 USB 2 + USB 3 pairs for these (fixed: guest and saved VMs see the same controller). Viewer protocol caps names and list; VMM checks every device id handed in. A guest-owned VMM can't plug a device: it gets only sockets the app gives it.

## The root helper (vroom-netd)

Root needed for bridged/host-only networking and for taking USB devices from macOS drivers. One process: `vroom-netd`, a launchd daemon.

Setup (app, on request; macOS password prompt via `osascript ... with administrator privileges`):

- Root-owned copies of the binary and libusb in `/usr/local/libexec/vroom`; config `/etc/vroom/netd.toml`; launchd file.
- Refuses to install under a `/usr/local` or `/usr/local/libexec` anyone but root can change.
- The app itself never runs as root.

Does only:

| | |
|---|---|
| network | vmnet (macOS) or tap (Linux) port for one VM adapter, returned as a datagram socket of Ethernet frames. Only `bridge_interfaces` interfaces, only listed host-only networks; locally administered unicast MACs only, every frame's source MAC checked; 32 ports per user |
| USB | USB helper (`vroom-netd usb-host`, same code as `vroom usb-host`) started as root for one device, returned as a stream socket |

Who can ask:

- Socket `/var/run/vroom/netd.sock`: root-owned, 0660, group `group` (`staff`: the Mac's users).
- Caller identity from the kernel (`getpeereid`; Linux `SO_PEERCRED`), not from the request.
- USB: members of `[usb] group` only (default `admin`: users who could sudo the same), off unless `[usb] enabled`.

Requests:

- One JSON line, strict (unknown fields refused), bounded length, 5 s to send.
- USB: VM name (for the log), `vendor:product`, optional serial (printable ASCII). No paths, bus locations, arguments, environment.
- Device must be plugged in, not a hub, not already served to a VM.
- Limits: 10 USB requests/min per user; 8 USB helpers per user, 16 total; 64 connections at once.

Root USB helper:

- Run via `/usr/bin/sandbox-exec` with its own Seatbelt profile: like the user's helper, but libusb only from the root-owned copy (`/opt/homebrew` is the user's: nothing user-writable loads into a root process); empty environment.
- Before each start the daemon checks its binary, that libusb, and every parent directory are root-owned and not writable by others.
- Can do what the user's helper does (USB device user clients, IOKit registry; no files, network, exec) plus what root adds: open and take any USB device. As asked: the user named the device; the helper serves only it.
- Linux: same as the user's helper, still root (keeps reopening the device on replug); the seccomp deny-list is what stops root's extras (modules, mount, set*id, raw sockets).
- On a device a macOS driver holds (or one left taken by an earlier helper): takes the whole device (libusb "capture": macOS re-enumerates, keeps its drivers off; mounted disk unmounted). Keeps it through guest resets (macOS never mounts the disk in between). Gives it back at the end (one more re-enumeration, drivers return). A device the guest can't use at all: given back at once.

Full Disk Access (needed to pass a USB disk as the device on macOS 15; sharing files / using as a disk needs neither FDA nor vroom-netd):

- Covers the daemon and the helpers it starts: macOS then lets them read the user's protected files (Mail, Messages, other apps' data), which root alone can't.
- Neither does: daemon reads only its config; helper profile allows no files.
- Still a real widening: code execution in vroom-netd would get it.
- Without it everything else works; only USB disks fail (returned to macOS).

Compromised VMM gains nothing:

- VMM asks for VM-file USB devices at start, before lockdown (user wrote the file); afterwards its sandbox can't reach the daemon socket.
- Devices connected while running come from the app. Network was already its business.
- A compromised `admin` user account could take USB devices (keyboards included) without the password; it could also wait for the next sudo.
- Every request logged with the user to `/var/log/vroom-netd.log`.

Socket passing: macOS may deliver a socket already disconnected if the sender closes its copy while in flight (~1 in 10 here). Daemon keeps its copy until the client hangs up; app keeps its copy of a helper socket a few seconds after handing it to the VMM.

Frame sockets: macOS gives Unix datagram sockets 4 KiB (2 frames). Both ends ask for 2 MiB; VMM side full anyway: daemon drops the frame (like a full NIC queue). Before: whole port given up after the first failed send (bridged adapter could die seconds into a boot, guest driver not reading yet, or in a burst). Only a gone VMM ends a port. Needs the daemon set up again; the VMM's bigger buffer helps with an older daemon too.

## Snapshots

- Contain guest RAM (keys, passwords).
- Written 0600 in 0700 dirs.
- Every blob SHA-256 checked on load; restore parses with the same validation as live input.
- Hashes are in the manifest the VMM writes: a save is the guest's word. Resume/restore (`restore_hardware`):

  | From | What |
  |---|---|
  | VM file | boot files, disk images, net attachments, share folders + writability, USB devices, identity, clipboard, serial, sandbox |
  | save, only if the file has it too | 3D, OpenGL (Apple driver), video decode/encode, nested, TPM, sound, mic, camera |
  | save | CPUs, memory, transport, balloon, monitors, sound card |

  - Mismatch (disk path/setup, disk count, boot kind, Secure Boot, identity, opt-in off in the file): refused, `--fresh` boots.
  - Share tag the file dropped: device stays, empty. USB device the file dropped: left out.
  - Adapter the file dropped: kept unplugged, no backend, host-only for the sandbox.
- Frozen layer named by a snapshot: a plain file in `<state>/disks` (qcow2) or exactly the disk's image (format from the VM file; never sniffed).

## vTPM

- `tpm = true`: TPM 2.0. libtpms runs in a separate `swtpm` process, not the VMM. macOS: own Seatbelt profile (its install, system libraries, its state dir; no network). Linux: no_new_privs + Landlock set up before exec (its binary and install prefix, `/usr`, `/lib*`, Nix/Guix stores read+exec; loader cache, OpenSSL config read; state dir read-write; nothing else) + swtpm's own `--seccomp action=kill`. No Landlock ABI 2 = `tpm = true` fails.
- VMM only frames commands for it, size-limited.
- State: `<state>/tpm` (0700). Capture requires successful volatile export and complete permanent/volatile state; export/read errors abort the snapshot. Incomplete state archives are rejected before stopping the current TPM or replacing its files. See the [TPM state contract](../crates/vmm/TPM_STATE.md) for recovery behavior and compatibility limits. Valid-input restore replacement remains nontransactional; this is not a whole-VM atomicity or cross-version portability guarantee.

## What 3D costs

`gpu = "virtio3d"`: guest command streams parsed by C/C++ code that reaches the Apple GPU driver:

| Stream | Parsed by |
|---|---|
| Venus (Vulkan) | virglrenderer's Venus decoder |
| virgl (OpenGL) | vrend, virglrenderer's GL decoder: the longer bug history |
| Neptune (Windows Direct3D 11, UTM's protocol) | virglrenderer's Neptune decoder + DXMT (guest D3D shaders → Metal via its own LLVM-based converter) |

- Lots of young C/C++ in guest reach: none of it runs in the VMM.
- Windows guests reach vrend too (3D driver makes virgl contexts for its own copies; our Windows OpenGL driver uses them like Linux Mesa). Our Windows Vulkan driver (and Zink on it) makes Venus contexts like Linux Mesa. Nothing new on the host: any guest code could already ask for either.

### The GPU process

- `vroom gpu-helper`, started by the VMM, one per VM, holds OpenGL; Vulkan and Direct3D contexts get a process each below it ([below](#a-process-per-vulkan-and-direct3d-context)).
- virglrenderer, MoltenVK, DXMT, the GL driver load there only. VMM never loads them; its sandbox has no GPU rules (no AGX user clients, no Metal compiler service, no shm but its own guest RAM).

Bug reach from the GPU process:

- the GPU: AGX and IOSurface user clients, Metal compiler service, framework reads (what Metal needs, nothing else)
- that VM's guest RAM (shm fds from the VMM; the guest has it all anyway) and buffers it made
- its two sockets to the VMM, and one to the render server

Can't reach: disks, network, shared folders, clipboard, the hypervisor (no vCPUs, no VM; a VM belongs to the process that made it), the viewer, other VMs, user files, other processes (no exec, no signals, no other process's shm). Sandbox goes up after libraries load, before the first guest command.

VMM trusts nothing it can't check:

- Replies framed with size limits.
- Handed-over buffers mapped at the guest-requested size, never more.
- Frames back via a VMM-made shared buffer, copied out at the size the VMM asked; or (macOS, app-started window) as IOSurfaces passed to the app unopened ([below](#the-screen-as-iosurfaces-macos)).
- GPU process gets fence socket and guest RAM over its call socket at start, not by inheritance.
- GPU process dies: VMM sees the socket close; 3D stops for that VM (guest gets GPU errors, nothing else breaks) until restart.
- `VROOM_GPU_IN_PROCESS=1`: renderer back in the VMM with GPU rules in its sandbox (debugging; macOS only has the GPU process).

vrend's GL target (`opengl_host`):

| | |
|---|---|
| `"apple"` (default) | macOS OpenGL 4.1. vrend's checks are the only ones before Apple's driver; no robust buffer access: an out-of-bounds shader read returns whatever GPU memory of the GPU process follows (its own guest's data) or crashes it. Gives GL 4.1, which most Linux apps need |
| `"angle"` | ANGLE on Metal: checks every call like browser WebGL before Metal; a vrend mistake meets a second check. Contexts request robust buffer access (OOB reads = zeros). Guest gets GL 2.1 only |

`opengl = false`: vrend and both GL drivers left out entirely.

Library loading: virglrenderer by full path from the GPU process's own dir (next to `vroom`, or `Contents/Frameworks` for `vroom.app/Contents/MacOS/vroom`, symlinks resolved); only if absent: Homebrew prefix, then dyld search path.

Our Venus patches (all in the GPU process):

- Null descriptors filled (never reach MoltenVK, which would follow a null pointer); empty submissions for sync fd imports; for Zink, some format/feature substitutions.
- Without semaphore emulation, a guest could already crash the GPU process via `vkWaitSemaphoreResourceMESA` (no MoltenVK function for it).
- Vulkan window on macOS GL: vrend maps the frame's shm read-only, copies rows; guest row size, offset, height checked against the shm size first.
- Big BGRA frames (patch 0030): Metal does the copy. Same read-only mapping (whole pages) becomes a Metal buffer (read only); blit with the checked offset, stride, size into an IOSurface the GPU process makes and keeps (not global, never sent); GL copies it into the texture. Metal was already there, sandbox rules unchanged, no new CPU-side code reached, only one more GPU copy with the same bound checks. Nothing new loaded.
- Fence thread per Vulkan/Direct3D context in the GPU process, woken via a pipe by its worker; reads the same shared fence counters the 1 ms poll read before: writing to the pipe gains a worker nothing.

GL side:

- Texture clears (`VIRGL_CCMD_CLEAR_TEXTURE`) now reach the host on macOS GL and ANGLE (no GL call for it): box checked against the texture level, texel uploaded via normal upload from a ≤4 MiB scratch buffer (one band of rows): a huge box costs time, not memory.
- macOS GL: guest told the real sample count (4), not 1 (patch 0014). Apps (Chrome, antialiased WebGL, games) now make multisampled textures and resolve with blits. Possible before too (vrend checked and created them the same way); now common. Snapshots don't keep their contents.
- Transform feedback could crash the GPU process at app end (macOS `glEndTransformFeedback` with the wrong program bound; patch 0015). Guest lost 3D until VM restart, nothing more: crash stays in the GPU process.

Zink transform feedback (patch 0016), in the Venus worker:

- SPIR-V rewriter (`vkr_xfb.c`) reads guest vertex shaders. Every read checked against module size and id bound. Unexpected input (captured structs, 64-bit outputs, strides >2048, offsets past the stride, SPIR-V <1.5): module left as is. 300,000 mutated Zink shaders under AddressSanitizer + UBSan: no finding. Output goes to MoltenVK's compiler, which read the guest's SPIR-V anyway.
- Shader stores via device addresses. Addresses and vertex count come only from the host, pushed before each draw: host copy of the bound range (checked against buffer size; only xfb buffers bound to memory) and remaining room. Guest push constants into our 48 bytes cut off. Rewritten shader checks each vertex slot against the count: writes stay in bound ranges.
- Zink devices now have `bufferDeviceAddress` on (any Venus Vulkan app could already ask): a guest shader can write anywhere in its worker's GPU memory, which holds only that app's objects.
- Command buffers keep a little state; layouts and query pools found again by id: guest destroying them mid-recording gets nothing written, not a dangling pointer.

### The screen as IOSurfaces (macOS)

App-started VMM: guest GL screen can reach the app as IOSurfaces instead of pixels ([ARCHITECTURE.md](ARCHITECTURE.md#the-screen-straight-from-the-gpu-macos)). Adds:

- Mach ports between our processes. An IOSurface crosses only as a mach port or global id; global ids unused (no surface made global, none looked up by id); Unix sockets can't carry ports. App gives the VMM, VMM gives the GPU process, a send right to its own port at spawn (registered port, `mach_ports_register`, taken by the child before starting anything and cleared so its children don't get it). Nothing in the bootstrap namespace; no sandbox gets a `mach-lookup` rule.
- Crossing: one message per surface (3 per monitor, made on size change) with its port + 5 numbers; one small socket message per frame.
- VMM accepts port messages only from its GPU process (kernel gives sender pid), checks numbers (monitor, slot, generation, size), forwards the port as is. Never maps or looks up a surface; its sandbox has no IOSurface access.
- App accepts only from the VMM it started; checks each surface: announced size, one plane, BGRA, rows within its memory. Guest can't name a surface, only change pixels the GPU process puts in it (as before).
- Compromised GPU process sending other ports or junk: malformed thrown away (rights returned); VMM forwards only well-formed messages from that pid; app refuses anything not a surface of the announced size.
- Compromised VMM can send the app any right-size surface: it decides what the screen shows anyway. App keeps surfaces of the latest size only (3).
- App reads a surface on the CPU only for thumbnails and pictures, locked read-only, within size. Window server shows the rest.
- A process sending on someone else's port can only be one we started during the brief port-registration window (another helper spawned then); its messages fail the pid check.

### A process per Vulkan and Direct3D context

Each guest Vulkan (Venus) and Direct3D (Neptune) context (roughly one per GPU-using app) gets its own process:

```text
 GPU process (sandboxed)          render server (no sandbox)
 virgl GL contexts, proxy  ---->  vroom gpu-server: starts workers
   |  one socket per context           |
   v                                   v  (posix_spawn, fixed arguments)
 vroom gpu-worker, one per context, each in its own sandbox:
 the GPU, its own shared memory; Direct3D ones DXMT's shader cache
```

Worker:

- Loads virglrenderer and its one driver (MoltenVK or DXMT) from vroom's own dir (`Contents/Frameworks` in vroom.app), never a search path.
- Sandboxes itself before reading any guest input: GPU rules above, shm under its own pid only, Direct3D: the VM's shader cache.
- Environment cleared except logging and driver debug switches; never a library-naming variable.
- A guest app breaking its driver lands with only its own GPU objects: no other app's textures/buffers, no GL state in the GPU process, no guest RAM (workers never map it: Vulkan/D3D buffers are shm the worker makes and hands to the GPU process).
- Talks only to the GPU process, over its socket.

Render server:

- No sandbox: on macOS a process can't narrow an inherited sandbox, so whatever starts sandboxed workers must have none.
- Kept small: reads 4 fixed-size requests from the GPU process (start, reset, create, destroy context); refuses unrequested backends and >256 workers; starts only `vroom gpu-worker` with arguments it builds (guest's context name cut to 31 of letters, digits, `._-`); never loads virglrenderer or GPU code.
- Compromised GPU process can ask for more workers, each sandboxed alike: nothing new.
- Exits with the GPU process, kills its workers.

Worker crash (or kill): takes its context only. Server logs it; GPU process retires that context's fences (guest doesn't wait forever); other contexts continue.

- Linux guest: Vulkan app sees device lost, stops.
- Windows: app's D3D device stops drawing. Desktop compositor's context: screen frozen until Windows restarts graphics (Win+Ctrl+Shift+B; tested, desktop returns). Holding fences back doesn't help: Windows' GPU watchdog doesn't fire with this driver, desktop just hangs.

Windows D3D contexts aren't saved: VMM has the agent turn the 3D display off first ([ARCHITECTURE.md](ARCHITECTURE.md#direct3d-state-cant-be-saved)).

- VMM destroys contexts the stopped driver left (like a device reset): a host decision on the agent's word that the device is off; a lying guest loses only its own contexts.
- Patch 0017 writes "fatal" into the ring status the guest already shares.
- `vroom_d3d.dll` (wrapper around the guest D3D driver): guest only, no new host path.

Vulkan context snapshots (patch 0010):

- 3 new requests on each worker socket, 3 calls VMM ↔ GPU process.
- Worker writes state into its own shm object (under a name its sandbox already allows), hands it to the GPU process → VMM. VMM checks size (≤64 GiB, ≤ what the object holds), stores it unread.
- Restore: VMM puts the data in shm, sent down to a new worker, which maps it read-only.
- Data comes from the snapshot file (controlled by anything that can write the state dir): worker checks it like guest input: each record length vs what's left; contents vs the size of the target resource, memory or image region (regions recomputed from the image, never from the file); rings with `vkCreateRingMESA`'s layout checks; reply streams with `vkSetReplyCommandStreamMESA`'s checks.
- Kept commands are the guest's own bytes, replayed through the same Venus decoder and handlers (reply flag cleared). Workers still never map guest RAM.
- VMM checks what it keeps like guest requests: blob sizes and kinds, guest RAM ranges, each mapping with `RESOURCE_MAP_BLOB`'s checks.
- Command following adds code on every guest command, bounded: past 4M commands or 1 GiB kept, the context isn't followed.

OpenGL (virgl) contexts stay together in the GPU process: vrend shares one GL context group (shared objects, one GL thread); splitting = a GL driver per app + copying every shared texture. A GL bug reaches every GL app of that VM, and guest RAM.

Cost: new Vulkan/D3D context +~50 ms (process start, driver load, Metal device). Windows desktop with Edge: 15–20 contexts, 10–60 MB each: +200–400 MB vs one process.

Loading and shared memory:

- libepoxy loads ANGLE only from virglrenderer's dir; Apple's OpenGL only by its system path, after the GPU process made a CGL context current; never a search path.
- DXMT (LGPL, separate dylib) loaded from the same dir before the sandbox; virglrenderer patched to look only there and never start its own render server.
- Shm: GPU process and each worker may create, map, unlink shm only under names with their own pid and virglrenderer's prefixes; open no other (not even read: another app's or worker's shm unreachable; `VROOM_SANDBOX_SELFTEST=1` checks it).
- Metal shader cache (shared with the user's other apps): read-only, so a compromised process can't plant GPU code for them.
- DXMT cache (Metal versions of guest shaders): per VM, in its state dir: no cross-guest planting.

Host-visible window (8 GiB of guest address space):

- Backed by reserved memory where no buffer is mapped (like real card memory). Pages the guest writes become real host memory: guest can grow the VMM by ≤8 GiB beyond its RAM.
- Swapping a page's backing (buffer unmapped, filler mapped) leaves it unbacked for a few µs. A guest access there we can't emulate (exclusive load, used by Windows' Vulkan driver on its ring) is retried until it succeeds, ≤100 ms per address; then the VM stops as before. Costs only that guest's vCPU time.

GL snapshots hold guest GPU contents (textures read back: whatever was on screen) next to RAM, in the state dir. Restore: GL state goes back through virglrenderer's checks like any guest command; resource sizes, backing ranges, counts get guest-command limits. A tampered snapshot can do what the guest could, no more.

Use 3D only for guests that need it: the GPU process confines the 3D code, but an escape from it still holds the host GPU, and GPU drivers are kernel code.

## The viewer socket

- App-started VMs accept a new window on a Unix socket (survive app crashes).
- Socket in the user's temp dir (`$TMPDIR/vroom/`, 0700), itself 0600, bound before the sandbox.
- Whoever connects gets the guest screen and sends keys and commands: any process of the user can, matching what user processes can already do to the VM's files.
- One viewer at a time; guest can't reach it.
- Hung VM: app reads the pid from the VMM's state lock, signals it only if that process is a `vroom` executable (the VMM can write that file).

## Several monitors

With `[display] monitors`:

- Shared framebuffer: a slot per monitor (8K each, ≤4); every read/write checked against its slot: nothing crosses monitors. App maps what the file holds, never >4 slots.
- Viewer messages for other monitors (mode, damage, window size, pointer) name a monitor; outside 1..3 refused; sizes both or neither.
- virtio-gpu: ≤4 scanouts; every scanout id checked; every rect clipped to its scanout's view; snapshot scanouts checked like commands.
- Agent gets the layout (sizes, places, bounded) and, on Windows, the pointer on monitors the USB tablet doesn't reach. Host → guest only. Session helper injects the pointer with SendInput: can't reach elevated windows or the secure desktop (UAC), like any user app.
- Windows' extra GPUs: 32 MiB plain memory in BAR 0 (reserved; only guest-written pages become memory), cleared by the 2D driver as a framebuffer. Windows' 3D GPU: 8 MiB, for the basic display driver while 3D is off. Device never reads it. Memory follows the BAR only inside the PCI windows: a BAR mid-sizing (all ones in its low half) can sit on RAM; once the memory went there and RAM was unmapped when it moved on.
- Windows: agent service picks a display driver per GPU from the driver store (2D on GPUs without 3D; 3D on the first when the host has 3D), like Device Manager; chooses only among installed drivers.

## Guest agent (Windows and Linux)

Windows (VirtIO serial port admits only Administrators and SYSTEM), so split:

- LocalSystem service: owns the port; clock sync, clean shutdown.
- Unprivileged console-session helper: clipboard, resolution, UI scale. Talks to the service over a named pipe: SYSTEM/Administrators/interactive users only; remote clients refused; only the active console session served.
- Helper only talks to a pipe server in session 0: a user process squatting the pipe name gets nothing.
- Service accepts from the helper only: clipboard text (size-capped), dropped-file answers, window and app lists while the host window is in Coherence (checked like the rest).

Linux, same split:

- Hardened systemd service: `CAP_SYS_TIME` only, read-only file system, Unix sockets only.
- Session helper started with the desktop.
- Meet on `/run/vroom-agent.sock`; service serves only the user logind reports active on seat0; helper only talks to a root-owned peer.

### Guest tools updates

Host sends code the guest runs as root/SYSTEM. Intended: the guest asked for host tools by installing the agent, and the host controls the guest anyway. Must hold: only the host can do it; the VMM never runs anything the guest sends.

- Only the privileged side takes an update, only from the host port (root; Windows: Administrators, SYSTEM). The session helper link never carries one: service drops `UpdateBegin`/`UpdateData` from a helper, never relays them (test: `updates_come_only_from_the_host`). Agent run by hand (not as service) refuses unless root.
- Manifest first: each file's path, size, SHA-256. Paths checked like dropped files (relative, no `..`, no drive letters, nothing Windows reads another way); ≤512 files, ≤256 MB total, no duplicates.
- Files written into a new root-only folder (Linux: `/var/lib/vroom-agent/update`, 0700, root-owned, never through a link; Windows: under `Program Files\Vroom`, not user-writable); each checked against size and hash as it arrives; first mismatch drops everything. Only a complete checked set is installed.
- Hashes catch damage and half-sent updates; not a signature, and needn't be: only the host can write the port. Signing protects against nothing the host couldn't do another way.
- Linux: hardened service can't replace `/usr` files or restart itself, and stays so. Writes a `ready` file; a systemd path unit starts `vroom-agent-update.service`, running the update's `install-agent.sh --update` as root.
- Windows: service starts the update's `vroom-agent.exe --apply-update`: stops the service, adds drivers whose version changed (`pnputil`; display driver only into the driver store), swaps files (in-use files renamed aside, restored on any failure), restarts the service.
- VMM passes an update from the app only as announced: one `UpdateBegin`, then exactly its bytes in `UpdateData` frames of that update, nothing else (`forward_update`). Never reads contents, never runs anything. Guest's only answer: `UpdateDone` (one short line, shown as text).
- App takes tools from its bundle (`Contents/Resources/guest-tools`) or `VROOM_GUEST_TOOLS`. Agent version read from the agent binary there; guest's version from its hello, only compared and shown, cleaned like every status line.
- Tools stick (guests without an agent): the app's own image, plugged read-only; installing from it is the user's step in the guest.

## Fuzzing

`fuzz/`: cargo-fuzz targets for what a guest (or downloaded image) controls. Run: `cargo +nightly fuzz run -O <target>`.

| Area | Targets |
|---|---|
| protocols | agent; viewer (both directions); USB passthrough both ways with the helper's checks; video helper protocol both ways, encoder messages included |
| network | NAT parsers, MAC translation |
| virtio | virtqueue chains over arbitrary guest memory; virtio-gpu 2D commands; virtio-fs (FUSE) requests; virtio-snd control and PCM (playback, capture); virtio-pci transport and PCI config space |
| devices | HD Audio (registers, CORB/RIRB, BDLs, stream DMA, MSI), xHCI (also a passthrough device answering from the input), NVMe, TPM TIS, fw_cfg (DMA included), flash: arbitrary register writes + guest RAM |
| images | qcow2 parsing; qcow2 differential (random writes, discards, zero writes, reopens, compactions checked byte for byte against a model); VMDK (sparse headers, grain tables, compressed grains, descriptors) |
| USB | descriptors |
| video (`video`) | stream parsing (NAL units, parameter sets, display order, VP9 superframes and frame headers, AV1 OBUs and sequence headers), encoder output to Annex B. 21M runs clean, +4.5M with the encoder parts |
| media (`media`) | virtio-media device, decoder and encoder: ioctls from guest memory, then hostile helper replies, then a snapshot of the result restored. 4.7M runs clean; also junk-command tests |

Found and fixed: qcow2 crash on short v3 headers; qcow2 multi-GB allocations from crafted table sizes; USB short configuration descriptor read out of bounds (panic, not memory corruption).

## Host resources

- A guest can fill its disk layers, hence the host disk: VMM pauses the VM at <2 GiB free where its state lives, rather than failing guest writes half-way.
- State dir locked by its VMM. Every disk image locked: exclusive while a VM writes it, shared as a read-only layer.
- A second VM can't write an image in use, or build on a base another still writes (silently corrupts the overlay).
- macOS: these locks also keep `qemu-img` out of a running VM's images, even with `-U`.

## Known gaps

- Secure Boot (`secure_boot = true`, Debian's EDK2): ARM firmware checks boot signatures, but its variable store is not protected from guest-kernel writes. An enabled flag or successful installation does not establish authenticated-variable enforcement. Installer checks bypassed by an existing setup path do not satisfy Secure Boot/TPM acceptance.
- Windows agent split is new; older installs run the all-in-one agent elevated (re-run `vroom-setup.cmd`).
- x86_64: the restricted one-vCPU, 512 MiB [direct-Linux VMM profile](../crates/vmm/X64_LINUX.md) passed boot and lifecycle acceptance on AWS nested KVM. Firmware/security features, Windows, general CLI/RPC, snapshots and migration remain outside that acceptance. The profile rejects firmware and TPM configuration.
- Fuzzing doesn't cover Venus, virgl, Neptune (3D) command streams: virglrenderer C (and DXMT C++ for Neptune), in the GPU process.

## What the guest can tell

From `[identity]` ([CONFIG.md](CONFIG.md#identity)):

- SMBIOS: vendor, product, family, SKU (VM name unless set), version, date, serials, chassis type, CPU name, "VM" BIOS flag
- ACPI table header IDs, FADT hypervisor vendor
- device tree model (direct kernel boot)
- disks: NVMe model, serial, firmware revision; virtio disk serials
- PCI IDs of host bridge, NVMe, USB, HD Audio controllers; HD Audio codec IDs. Defaults are QEMU's (vendor 0x1B36; HD Audio: QEMU's Intel ICH6 `intel-hda` and `hda-duplex` codec)
- USB keyboard/tablet vendor ID and names; virtio input names and vendor
- generated MAC prefix; NAT guest, gateway, DNS addresses, gateway MAC. Defaults (10.0.2.15, .2, .3) = classic QEMU/VirtualBox layout
- agent port name

`announce_vm = false` drops the SMBIOS and ACPI "this is a VM" markers.

Visible whatever the config:

- virtio devices: PCI vendor 0x1AF4, subsystem 0x1AF4:1100, revision 1 (Linux and Windows drivers match on these; virtio-win INFs list `SUBSYS_11001AF4&REV_01`)
- `video_decode`: virtio-media device (id 48) "Video decoder", formats (codecs this Mac decodes), sizes, levels = VideoToolbox's. Our VA-API driver names itself "vroom"
- `video_encode`: another, "Video encoder", codecs this Mac encodes; streams are VideoToolbox's
- fw_cfg: "QEMU" signature, DMA signature, file names (`etc/acpi/tables`, `etc/smbios/...`, `etc/ramfb`); EDK2 looks for them
- board: QEMU virt memory map, PL011/PL031 PrimeCell IDs, CFI flash (Intel 0x89/0x18), GICv3, ACPI device names (`COM0`, `PCI0`, `VR00`...) and _HIDs; stock EDK2 and OS drivers expect this
- PSCI 1.0 / SMCCC 1.1 answers (no vendor named). KVM hosts: kernel PSCI also answers the KVM vendor-hypervisor UID call
- firmware: EDK2 reports its own vendor in UEFI tables
- display size follows the window (says something about your screen); `[display] hidpi` also reveals pixel density (the scale the agent is asked for)
- display: ramfb, then virtio-gpu without EDID. 3D: Vulkan shows the host GPU (MoltenVK on Apple). Windows 3D: "Red Hat VirtIO GPU 3D controller", Direct3D on Metal underneath. GL renderer name follows `gl_renderer`, but limits (GL 4.1 with `apple`; GL 2.1, ES 3.0 with `angle`; texture sizes, formats) still give the host driver away
- CPU ID registers from the host CPU (an Apple core on a Mac); timing differs from bare metal
- vTPM is swtpm: manufacturer IBM; TIS DID/VID register agrees (1014:0001)
- NAT's TCP/IP stack (smoltcp) and the absolute-pointer tablet behave like no physical router or mouse
- guest agent if installed (its protocol says "vroom")
- shared folders: virtio-fs itself, host disk size and free space; Windows: `VroomShare_<tag>` services

Defeats casual checks, not software that tries hard.
