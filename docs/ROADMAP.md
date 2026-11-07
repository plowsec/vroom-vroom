# Roadmap

Left before vroom replaces VMware Fusion, Parallels or VirtualBox day to day, roughly in order. Done items move to GUIDE / ARCHITECTURE.

## 3D: catching up with VMware and Parallels

| | Linux guests | Windows guests |
|---|---|---|
| vroom | OpenGL 4.1 (virgl); Vulkan 1.3 windowed and off screen (Venus); OpenGL 4.4 per app (Zink) | Direct3D 11; OpenGL 4.1 (virgl) |
| VMware Fusion (Apple Silicon) | OpenGL 4.3 | Direct3D 11, OpenGL 4.3 |

Findings (Oct 2026, macOS 15.6, MoltenVK 1.4.2, guest Mesa 25.2):

- macOS OpenGL stops at 4.1. Only way past: Zink (GL on Vulkan) over Venus.
- Zink with only `nullDescriptor` gave GL 2.1. MoltenVK lacks transform feedback (GL 3.0), depth clip control (3.2), packed 10-bit vertex formats (3.3), RGB32 texel buffers (4.0). GL 4.5 needs cull distances: Metal lacks them.
- KosmicKrisp (LunarG, Vulkan on Metal, in Mesa since late 2025, Vulkan 1.4 conformant) has all of it but needs Metal 4 = macOS 26. Not on macOS 15. MoltenVK main still says `nullDescriptor = false`.
- So: gaps filled in virglrenderer's Venus decoder (`third_party/virglrenderer/0007-venus-what-zink-needs.patch`):
  - null descriptors → small zeroed objects of our own
  - depth clip off → depth clamp (what Metal does anyway)
  - Zink only (it names itself at start): transform feedback written by the vertex shaders (0016, ARCHITECTURE.md); missing formats read through the nearest one
  - result: Zink reports GL 4.4 core + compat, ES 3.1. GL 4.3 compute test: correct, 330× llvmpipe
- Vulkan windows:
  - guest Venus offers `VK_KHR_swapchain` only if the host imports sync fds into semaphores; MoltenVK can't. Guest always waited for the fd itself: host just marks the semaphore signaled (empty submission).
  - window images get host-visible memory (must be shareable)
  - guest compositor (Xorg glamor, Weston, GNOME Shell) shows the frame via GL/virgl; macOS GL can't share memory with Vulkan: vrend copies the frame into a texture when the compositor uses it (`0008-venus-swapchain.patch`), on the GPU via IOSurface for big frames (0030, ARCHITECTURE.md)
  - same fix gives Vulkan 1.3 (lacked `synchronization2`)
- Proton, DXVK: can't run; need geometry shaders (neither MoltenVK nor KosmicKrisp).

Linux guests, next:

- **Transform feedback, the rest** (Zink): non-indexed point/line/triangle lists captured. Indexed draws, strips, fans capture nothing (output order ≠ vertex order). Indexed lists: non-indexed draw reading the index buffer in the vertex shader. Strips: each vertex written up to 3×. Vulkan apps get no `VK_EXT_transform_feedback` (rewrite expects Zink's SPIR-V only).
- **KosmicKrisp on macOS 26**: load instead of MoltenVK when Metal 4 is there; Zink gets most emulated features for real. Check speed vs MoltenVK.
- **Zink speed**: app window (Xorg, 1280x760, 6 glmark2 scenes): Zink ~860 vs virgl 1930 (since screen-from-GPU); light scenes 1240 vs 2800 fps; terrain 228 vs 389. Terrain is GPU bound (CPUs mostly idle): cost is in MoltenVK's shaders from Zink's, or MoltenVK per-draw work. Each Zink frame also waits in the guest for the GPU before showing: no overlap. virgl stays default; Zink per app.
- **Vulkan snapshots, the rest** (they work: GUIDE.md, ARCHITECTURE.md):
  - GPU part as raw bytes, not base64 in JSON (1 GiB Vulkan memory → 1.45 GB blob, most of a 9 s snapshot)
  - save a Vulkan window's memory still shown after its app is gone (refuses today)
  - query results
  - GNOME Wayland not tried (Xorg was)
- **Early Vulkan fence**: ~once a minute `vkWaitForFences` returns before a buffer copy in the same submission is visible; 20 ms later it is. Venus + MoltenVK, not snapshots. Not investigated.
- **Screen as IOSurfaces, the rest** (works for the first monitor's GL screen in a window the app started; GUIDE.md):
  - Windows' Direct3D screen: a blob the VMM reads with the CPU (GPU process could blit it with Metal, as vrend does for Vulkan windows)
  - other monitors still get pixels
  - window reattached to a running VM gets pixels (mach port only at VMM start; later needs XPC or a launchd name)
  - layer steps aside while the mouse is over the toolbar (tooltips); a keyboard-focus tooltip would hide under it
  - tooltips, menus, live resize, full screen: check with a real mouse

Windows guests:

- **Direct3D 12**: most games since 2020. Also needed by Microsoft's "OpenCL, OpenGL and Vulkan Compatibility Pack" (we have our own GL 4.4 / Vulkan 1.3: [GUIDE.md](GUIDE.md#opengl-and-vulkan-in-windows-guests)).
- **OpenGL and Vulkan without the copy**: frames go back to the guest and through GDI (640x480 spinning triangle: 415 fps Zink, 750 virgl, vCPUs at two cores, mostly copying). Needs: guest Vulkan presenting into a Direct3D 11 texture the compositor takes (Mesa win32 WSI has such a path for drivers sharing memory with D3D12); host: that Neptune texture and the Venus image as one IOSurface. Both live in render workers: IOSurface passed worker → GPU process → worker as mach ports (as the screen goes to the app). Work in guest driver, Neptune, Venus.
- **Zink OpenGL app crashed at a save** (1 in 8, Oct 18): write to address 0 in `vroom_gl_arm64.dll` (+0x733838) after its Vulkan device was lost, from the app's frame.
- **Vulkan/OpenGL apps through suspend**: 3D display goes off for the save → they lose the GPU (Vulkan: lost device). Linux keeps theirs (Venus snapshots). Windows needs: Venus driver reconnecting after the display returns (new context, same objects and mappings) + host restoring the context into it.
- **Zink on real apps**: only our tests (spinning cube, triangle, compute, D3D11 at the same time). Not tried: Blender 4.x, games, Qt apps. Zink warns MoltenVK lacks logic ops and custom border colors.
- **Suspend with 3D, the rest** (crashes, refusals, hangs fixed: ARCHITECTURE.md "Direct3D state can't be saved"):
  - 32-bit apps keep the stock D3D driver (no x86 `vroom_d3d`)
  - LogonUI loaded the stock one with ours registered (seen once)
  - root cause in UTM's drivers, theirs to fix: kernel driver unmaps a live process's rings, stops before Windows destroyed every context. Fixed there → our wrapper and context drop can go.
- **32-bit apps**: Microsoft's OpenGL 1.1, no Vulkan (no x86 builds of our drivers).

## Hardware video decoding and encoding

Done, Linux guests ([decode](GUIDE.md#video-decoding), [encode](GUIDE.md#video-encoding)):

| | V4L2 (GStreamer, FFmpeg) | VA-API (our driver) |
|---|---|---|
| decode (`video_decode = true`) | H.264, HEVC, VP9, AV1 | H.264, HEVC, VP9: Firefox, Chrome |
| encode (`video_encode = true`) | H.264, HEVC | Chrome calls, FFmpeg |

VMware Fusion and Parallels offer none of this to Linux guests on Apple Silicon.

### Which guest side, and why (October 2026)

| Option | Status |
|---|---|
| **virtio-video** (ids 30, 31) | spec draft 2019–2023, never merged; Linux driver never mainline; authors moved to virtio-media. Dead |
| **virtio-media** (id 48, Google) | **used.** V4L2 over virtio: guest sees a plain V4L2 decoder (or camera). Android, ChromeOS ship it. Device id mainline since early 2026; driver not (posted 2025, v3, out of tree in Linux 7.3). Builds out of tree on Ubuntu 24.04's 6.8 with a two-line fix. Works as is with V4L2 stateful decoder users: GStreamer (`v4l2h264dec`, `v4l2h265dec`, `v4l2vp9dec`), FFmpeg (`h264_v4l2m2m`, ...), so mpv and anything on FFmpeg. Stateful suits VideoToolbox: both take the plain bitstream |
| **VA-API** (browsers) | **guest driver done** (`scripts/guest/vaapi`), on top of the V4L2 decoder. Alternative was virglrenderer decoding VA-API on the host (no VA-API on macOS: needs a VideoToolbox backend in the GPU process). Either way VA-API gives parsed parameters, VideoToolbox wants bitstream: parameter sets written back out. Guest driver keeps decoding behind the existing device and helper (no new host code, nothing in the GPU process); works without 3D for players that copy pictures (Firefox needs 3D for dma-bufs). No maintained VA-API driver over V4L2 stateful exists (`libva-v4l2-request` is stateless) |
| **Firefox V4L2 path** (Raspberry Pi, via FFmpeg) | takes only DRM PRIME dma-bufs: only the Pi's FFmpeg gives them, needs dma-buf export from the V4L2 driver. Not usable on Ubuntu (checked in Firefox 158 source) |
| **Vulkan Video via Venus** | Mesa Venus doesn't expose it; MoltenVK doesn't implement it. Waits on both |

### Left to do

- **AV1 in the VA-API driver**: VA-API passes tile data and header fields, not the header. Writing sequence/frame headers back: long, mechanical. Some fields are coded against earlier frames (global motion, segmentation, loop filter deltas): keep them per reference slot. Firefox calls AV1 not power efficient here: YouTube sends VP9 for now.
- **Chrome on `opengl_host = "angle"`**: GPU process won't start (ANGLE ES 3.0 has no multisample textures → one sample, so no ES 3.0 by spec). Works on `apple`.
- **Encoding, the rest**:
  - VP8/VP9 calls (Google Meet often) stay software: no VideoToolbox VP8/VP9 encoder. Steering Chrome to H.264 needs a page or an extension.
  - Chrome needs `--enable-features=AcceleratedVideoEncoder` (install script adds it). Drop once Chrome enables it on Linux.
  - Chrome frames reach the driver in memory: Chrome converts each camera frame to NV12, driver copies into the encoder buffer. Encoding from Chrome's GPU buffers (dma-bufs, screen sharing) needs dma-buf import; driver imports only its own.
  - HEVC via VA-API: no crop passed → whole rows of 16 (1920x1088). Fix: read crop from FFmpeg's packed SPS. `hevc_v4l2m2m` unaffected.
  - Windows guests: Media Foundation encoder on the same helper.
- **Guest drivers**: ship virtio-media (DKMS) until upstream, follow upstreaming; package the VA-API driver.
- **Fewer copies, the rest**: VideoToolbox buffer → guest buffer is one copy now (was: socket + another copy). Firefox/Chrome: VA-API driver copies into its surface, again into exported GPU buffers (Chrome GPU process: 0.13 CPU at 1080p30). Fix: VideoToolbox IOSurface to virglrenderer as the texture (needs video helper ↔ GPU process sharing). Chrome on X11: driver converts each picture to BGRA on guest CPU (~1 ms per 1080p picture); a Mac GPU shader would do it free and keep 10-bit in 10 bits.
- Chrome on GNOME Wayland drops ~1 in 25 1080p30 pictures with hardware decode; Xorg none. Display path, to look at.
- Firefox in test setup (sway on virgl, headless) drops ~40% of frames whatever decodes (software too, even 360p): display path. Check with a real window.
- Snapshot restored while Firefox ran: its 3D context fails (virgl `SUBMIT_3D` errors, either decode). A 3D snapshot issue.
- FFmpeg 6.1 `v4l2m2m` decoders (not ours): mid-stream size change hangs them ("waiting for user to release AVBufferRefs"), drop pictures, repeat timestamps. GStreamer 1.24: no P010 from V4L2, no V4L2 AV1 decoder.

### Windows guests (research only)

- Edge, Chrome, most apps decode via Direct3D 11 video (DXVA), part of the graphics driver's user-mode side. Ours is viogpu3d's, forwarding D3D11 to DXMT on the Mac (Neptune); DXMT has no video decoder as far as we know.
- Work: video decoder calls in guest driver and Neptune; host side decoding DXVA's parsed picture parameters with VideoToolbox. Same problem our VA-API driver solves (H.264/HEVC parameter sets, HEVC slice headers); DXVA's parameters are close to VA-API's: share the code.
- Media Foundation apps (Movies & TV) also use DXVA via D3D11. Our own MF decoder sending bitstream to the video helper would serve them without the graphics driver, not Edge/Chrome. No Windows virtio-media driver: new kernel driver + decoder.

## Before it's ready for people other than us

Testing:

- Other Macs (M1–M4, 8 GB base models), other macOS versions. Days-long runs: sleep/wake with VMs running, full disks.
- Only a person at the Mac can try: bridged on real vmnet; drag out of the guest to Finder, Mail, Desktop with a real mouse; Coherence beside a window tiler; several monitors on several Mac screens.
- Benchmarks vs VMware Fusion and Parallels on the same Mac, losses published with the wins.

USB:

- Disks via vroom-netd: give installed vroom-netd Full Disk Access, try a real stick in a guest. macOS 15 refuses USB storage to programs without FDA, root included; tried without it only: macOS releases the stick, takes it back, guest can't read it.
- Other real devices (serial adapters, keyboards, webcams, sound cards) untried.
- Helper should tell the VMM why it gave up on a device, for the app to show (today: log only).
- Developer ID signature would keep vroom-netd's permission across updates.
- Disks as files / as a disk with a real stick from the app (tried: disk images, a volume's folder only): macOS "Removable Volumes" prompt for vroom.app; `authopen` on a real `/dev/rdiskN` (its password prompt; whether it also needs FDA: unknown).
- Card reader with several disks: device passthrough only. Linux hosts: neither way yet (device passthrough works).
- "Use as a disk" given back with a real stick: macOS should mount the guest's partitions (seen with disk images only). App says macOS sees no partitions: `lsof /dev/rdiskN` for what holds it; `vroom usb reset` via vroom-netd re-enumerates without a replug.
- `com.apple.vm.device-access` entitlement: real passthrough of devices macOS drivers hold (USB disks above all: macOS 15.3+ refuses them to programs without FDA, root included). Needs paid Developer ID + Apple's approval; then no root helper for USB. Until then: share as files or use as a disk (GUIDE, "USB disks").

Distribution:

- Signed, notarized vroom.app (`scripts/app.sh` signs ad hoc; needs Developer ID + hardened runtime), disk image, updates, crash reports.

Features and gaps:

- Drag and drop on Wayland: no live drag either way (outside programs can't join a Wayland drag); files land on the Desktop.
- Several monitors: one UI scale for all (per-screen scale needs per-monitor scales); Direct3D on Windows' first monitor only (others copied to 2D GPUs); Coherence on the first only (others off meanwhile).
- Network:
  - new port forwards on a running VM need listening sockets the sandbox forbids (app could open and hand them over)
  - adapter added while running = ordinary root-bus device next start: Windows sees a new device (new "Ethernet N", settings not carried). VM file could pin its slot.
  - no hot plug for disks yet (slots take any virtio device; a save would record them like adapters), nor for virtio-mmio VMs (Linux direct kernel boot)
  - netd fixes (frame buffers; a busy VMM drops frames, not the port) need netd set up again
- Rosetta for Linux guests (x86_64 programs): Apple offers it only via Virtualization.framework; we'd have to look like it.
- Fuzzing the 3D command streams (later).
- Windows: desktop's own D3D process dies → screen frozen until Win+Ctrl+Shift+B. Guest driver should report a lost device for that context (Windows rebuilds it).
- **Windows 3D: black display after some resumes** (Oct 18, M3 Max): 2–4 resumes in 12 show nothing for ~1 min; no crash, nothing left over.
  - Fixed causes: a process crashing in the D3D driver during the save (DWM above all; display stayed off in 3 of its 4 crashes); first suspend after boot saved 1.5 s after the 3D display went off (4 of 4; at 3 s and 6 s: 0 of 5).
  - Left: bad resume brings up 2 D3D contexts instead of 4; display off/on brings the same 2 back; Windows recovers by itself after 60–70 s. Every third resume in one run of 12: hints at something cycling in Windows.
  - VMM watchdog (15 s after Windows says its 3D display is on with nothing shown: device off 3 s, on; ≤3 times) helped once in these runs. Monitor replug (tried first before) never helped: removed.
  - Next: which processes own the 2 contexts (names only say "neptune-win32"); Windows display logs (Microsoft-Windows-Dxgkrnl, DWM) during a bad resume.
- "Undecodable MMIO access at 0x8200020000" (Oct 18, 3 in ~30 saves): Windows' basic display driver drawing into the 3D GPU's registers when the 3D display went off. Fixed: memory in BAR 0 (ARCHITECTURE.md). Those at 0x8200010000 (Oct 16, 17, also during saves): most likely the same, other PCI layout.
  - 100 ms of retries for such accesses (`hv/src/hvf/vcpu.rs`) stay, for the host-visible window.
  - VMM can't emulate a load/store pair to device registers: a guest doing one stops.
- Guest tools, the rest (update button, ready-made VMs work):
  - Real CD drive: tools stick is a USB disk, so Windows shows AutoPlay ("open folder"), ignores `autorun.inf`. A USB CD-ROM (MMC commands on the existing Bulk-Only stick) would run `vroom-setup` from AutoPlay and could carry the Windows ISO: no install stick, no 4 GB limit (wimlib).
  - Ready-made Windows: OpenSSH comes from Windows Update (download in the guest); 3D driver package not on the install stick yet; app could remove `install.img` from the VM file once the new agent says hello.
  - GDM login screen ignores the window's UI scale on a HiDPI Mac (session helper starts with the session): small text until login.
  - GNOME Shell extension could serve the clipboard from the compositor, for Wayland desktops without Xwayland (helper uses X11's clipboard; GNOME passes it to Wayland apps).
  - Windows mounts a read-only NVMe disk read-write (refused writes fail fast now: GUIDE.md "Read-only disks"). Ignores the namespace write-protect flag and namespace write protection; what its driver reports to the disk class driver (MODE SENSE write-protect bit) is out of our reach. Tools/install stick as USB stick or USB CD-ROM: Windows sees those read-only.
  - Signed tools: updates checked by hash against the host's manifest; a signature matters only if something other than the host could write the agent's port.

## Later

Power users first; these wait.

- **Coherence, next** (GUIDE.md): works for Windows and Linux X11. Wayland needs a helper inside the compositor (KWin script, or the GNOME Shell extension the tools install, which could report windows too).
