# Roadmap

What is left before vroom can replace VMware Fusion, Parallels or
VirtualBox for everyday use, roughly in the order we mean to do it. Items
move to the docs (GUIDE, ARCHITECTURE) once they work.

## In progress

- **Coherence, next steps** (see GUIDE.md): Wayland sessions would need
  a helper inside the compositor (a KWin script, or the GNOME Shell
  extension the guest tools already install, which could report windows
  too).

## 3D: catching up with VMware and Parallels

Where we are: Linux gets OpenGL 4.1 (virgl), Vulkan 1.3 in windows and
off screen (Venus), and OpenGL 4.4 for the apps you start on Zink (below);
Windows gets Direct3D 11 and OpenGL 4.1 (virgl). VMware Fusion gives Windows on Apple Silicon
Direct3D 11 and OpenGL 4.3, and Linux OpenGL 4.3.

What we found (October 2026, macOS 15.6, MoltenVK 1.4.2, guest Mesa 25.2):

- macOS's own OpenGL stops at 4.1. The only way past it is OpenGL on top
  of Vulkan in the guest (Mesa's Zink) over Venus. Zink needs more from
  Vulkan than `nullDescriptor`. With only that, it gave OpenGL 2.1:
  MoltenVK also has no transform feedback (needed from OpenGL 3.0), no
  depth clip control (3.2), no packed 10-bit vertex formats (3.3) and no
  RGB32 texel buffers (4.0). 4.5 needs cull distances, which Metal lacks.
- LunarG's KosmicKrisp (Vulkan on Metal, in Mesa since late 2025, Vulkan
  1.4 conformant) has all of the above, but it is built on Metal 4 and so
  needs macOS 26. It can't run on macOS 15. MoltenVK's main branch still
  says `nullDescriptor = false`.
- So we fill the gaps in virglrenderer's Venus decoder
  (`third_party/virglrenderer/0007-venus-what-zink-needs.patch`):
  null descriptors become small zeroed objects of our own, depth clip off
  becomes depth clamp (what Metal does anyway), and, for Zink only (it
  names itself when it starts), transform feedback is accepted but not
  captured, and the missing formats are read through the nearest one.
  Zink then reports OpenGL 4.4 core and compatibility, and OpenGL ES 3.1.
  A GL 4.3 compute shader test gives the right results, 330 times faster
  than llvmpipe.
- Vulkan windows: the guest's Venus driver only offers `VK_KHR_swapchain`
  when the host can import sync fds into semaphores, which MoltenVK can't.
  The guest has always waited for the fd itself; the host just has to mark
  the semaphore signaled, which an empty submission does. Window images
  must live in memory the host can share, so they now get host visible
  memory. The guest's compositor (Xorg's glamor, Weston, GNOME Shell) then
  shows the frame with OpenGL through virgl, and macOS's OpenGL can't
  share memory with Vulkan: vrend copies the frame into a texture when
  the compositor uses it (`0008-venus-swapchain.patch`). The same fix
  gives the guest Vulkan 1.3 (it lacked `synchronization2`). Proton and
  DXVK still can't run: they need geometry shaders, which neither MoltenVK
  nor KosmicKrisp has.

Linux guests, next:

- **Transform feedback for real** (Zink, and Vulkan apps). Today it is
  accepted and nothing is written, which is wrong for the few apps that
  use it (some games' particles, some CAD). It would rewrite the vertex
  shader to store its outputs into the buffers, and is only simple for
  non-indexed draws.
- **KosmicKrisp on macOS 26**: load it instead of MoltenVK when the Mac
  has Metal 4. Zink then gets most of what we emulate for real, and its
  speed should be checked against MoltenVK.
- **Zink speed**: glmark2 scores about 1000 on Zink and 1170 on virgl,
  but its terrain scene runs at half virgl's speed (about 210 fps against
  420). It is GPU bound there: the guest and host CPUs are mostly idle,
  so the cost is in the shaders MoltenVK makes from Zink's, or in what
  MoltenVK does per draw. Each Zink frame also waits in the guest for
  the GPU before it is shown, so frames don't overlap. virgl stays the
  default; Zink is per app.
- **No copy for Vulkan windows**: an IOSurface shared by Metal and
  OpenGL instead of copying each frame. The copy is fine at 1280x800.
- **Snapshots with Vulkan, what's left** (they work, see GUIDE.md and
  ARCHITECTURE.md): store the GPU's part of a snapshot as raw bytes, not
  base64 in JSON (1 GiB of Vulkan memory makes a 1.45 GB blob and most of
  a 9 s snapshot); save a Vulkan window's memory still shown after its
  app is gone (today that refuses); query results; GNOME's Wayland
  session not tried yet (Xorg was).
- **A Vulkan fence that comes back early.** Seen while testing: about
  once a minute, a guest's `vkWaitForFences` returns before a buffer copy
  in the same submission is visible; 20 ms later it is. Venus and
  MoltenVK, not snapshots; not looked into yet.

Windows guests:

- **Direct3D 12**, the one most games since 2020 need. It is also the key
  to the next two: Microsoft's "OpenCL, OpenGL and Vulkan Compatibility
  Pack" runs OpenGL and Vulkan apps on top of Direct3D 12.
- **OpenGL 4.3 and up** (VMware has 4.3; we have 4.1 through virgl,
  see [GUIDE.md](GUIDE.md#opengl-in-windows-guests)): Zink over Venus, as
  for Linux, from the same Mesa build. It needs Vulkan in the guest first.
- **Vulkan apps**: the 3D driver package carries a Venus Vulkan driver,
  but Windows' Vulkan loader doesn't load it (error 87: its manifest
  names the DLL without a path).
- **OpenGL without the copy**: frames go back to the guest and through
  GDI. A texture both vrend and DXMT see (an IOSurface) would let the
  compositor take them directly.

## Hardware video decoding

Linux guests decode H.264, HEVC, VP9 and AV1 on the Mac's VideoToolbox
now (`[display] video_decode = true`, see [GUIDE.md](GUIDE.md#video-decoding)):
GStreamer and FFmpeg through V4L2, Firefox through our VA-API driver
(H.264, HEVC and VP9). VMware Fusion and Parallels don't offer this to
Linux guests on Apple Silicon at all.

### Which guest side, and why (October 2026)

A guest needs a driver and an API its players use. What exists:

- **virtio-video** (device ids 30 and 31): a virtio spec draft that went
  through several versions from 2019 to 2023 and was never merged. Its Linux driver never reached
  mainline. Its authors moved to virtio-media. Dead.
- **virtio-media** (device id 48), Google's: it carries V4L2 itself over
  virtio, so the guest sees an ordinary V4L2 decoder (or camera). Android
  and ChromeOS ship it. The device id is in mainline Linux since early
  2026; the driver isn't (posted for review in 2025, v3, still out of
  tree in Linux 7.3). It builds out of tree on Ubuntu 24.04's 6.8 kernel
  with a two-line fix. **This is what we use.** Players that talk to V4L2
  stateful decoders use it as is: GStreamer (`v4l2h264dec`,
  `v4l2h265dec`, `v4l2vp9dec`), FFmpeg (`h264_v4l2m2m`, ...), so mpv and
  anything on FFmpeg. A stateful decoder also suits VideoToolbox: both
  take the plain bitstream.
- **VA-API**, what browsers use on Linux. Two ways: a VA-API driver in
  the guest on top of the V4L2 decoder, or virglrenderer decoding the
  guest's VA-API calls on the host (there is no VA-API on macOS: it would
  need a VideoToolbox backend in the GPU process). Either way VA-API hands
  over parsed parameters and VideoToolbox wants the bitstream, so the
  parameter sets have to be written back out. **We did the guest driver**
  (`scripts/guest/vaapi`): it keeps the decoding behind the existing
  device and helper (no new host code, nothing in the GPU process), and
  works without the 3D GPU for players that copy pictures (Firefox needs
  3D anyway, for dma-bufs). No maintained VA-API driver on top of V4L2
  stateful decoders exists (`libva-v4l2-request` is for stateless ones).
- **Firefox's V4L2 path** (made for the Raspberry Pi, through FFmpeg):
  it only takes pictures as DRM PRIME dma-bufs, which only the Raspberry
  Pi's FFmpeg gives, and which need dma-buf export from the V4L2 driver.
  Not usable on Ubuntu (checked against Firefox's source, 158).
- **Vulkan Video through Venus**: Mesa's Venus driver doesn't expose
  Vulkan Video, and MoltenVK doesn't implement it. Not before both move.

### Left to do

- **AV1 in the VA-API driver**: VA-API passes AV1's tile data and the
  frame header's fields, not the header itself. Writing the sequence and
  frame headers back out is long but mechanical; some fields are coded
  against earlier frames (global motion, segmentation, loop filter
  deltas), so the driver has to keep those per reference slot. Firefox
  says AV1 isn't power efficient here, so YouTube sends it VP9 for now.
- **Chromium and Chrome**: their GPU process doesn't start on the guest's
  virgl OpenGL ("eglCreateContext ES 3.0 failed with EGL_BAD_ATTRIBUTE",
  both the snap's Chromium 155 and Google's arm64 Chrome 155), so there
  is no VA-API decoding to test. Once that works (virgl's EGL, or Zink),
  the snap's Chromium can load our driver with
  `LIBVA_DRIVERS_PATH=$HOME/<dir>` (its wrapper keeps that path).
- **Snap Firefox**: it loads VA-API drivers only from its own runtime.
  Mozilla's deb works.
- **The guest drivers**: ship virtio-media (DKMS package) until it is
  upstream, and follow its upstreaming; package the VA-API driver.
- **Fewer copies, the rest**: a picture now goes from VideoToolbox's
  buffer straight into the guest's (one copy; it crossed a socket and
  was copied again before). For Firefox the VA-API driver copies it into
  its surface and again into the GPU buffers it exports. Handing
  VideoToolbox's IOSurface to virglrenderer as the texture would save
  those, but it means sharing between the video helper and the GPU
  process.
- Firefox in our test setup (sway on virgl, headless) drops about 40% of
  frames whatever decodes them (software too, and for 360p): the display
  path, not decoding. To look at with a real window.
- After restoring a snapshot taken while Firefox ran, its 3D context
  fails (virgl `SUBMIT_3D` errors, software or hardware decoding): a
  3D snapshot matter.
- FFmpeg 6.1's `v4l2m2m` decoders, not ours: a mid-stream size change
  hangs them ("waiting for user to release AVBufferRefs"), they drop
  pictures and their timestamps repeat. GStreamer 1.24 takes no P010 from
  V4L2 and has no V4L2 AV1 decoder.

### Windows guests (research only)

- Edge, Chrome and most apps decode through Direct3D 11 video (DXVA): a
  feature of the graphics driver's user-mode part. Ours is viogpu3d's,
  which forwards Direct3D 11 to DXMT on the Mac (Neptune); as far as we
  know DXMT has no video decoder. The work: the video decoder calls in
  the guest driver and Neptune, and a host side that decodes DXVA's
  parsed picture parameters with VideoToolbox. That is the problem our
  Linux VA-API driver solves (parameters back to a bitstream: H.264 and
  HEVC parameter sets, HEVC slice headers); DXVA's parameters are close
  to VA-API's, so that code can be shared.
- Media Foundation apps (Movies & TV) use DXVA too through Direct3D 11.
  A Media Foundation decoder of our own sending the bitstream to the
  video helper (as virtio-media does) would serve those without the
  graphics driver, but not Edge or Chrome. Windows has no virtio-media
  driver either: that would be a new kernel driver plus the decoder.

## Before it's ready for people other than us

- Test on other Macs (M1 to M4, base models with 8 GB) and macOS versions,
  and days-long runs: sleep and wake with VMs running, full disks.
- USB disks through vroom-netd: give the installed vroom-netd Full Disk
  Access and try a real stick in a guest (macOS 15 refuses USB storage
  to a program without it, root included; tried here without it only:
  macOS lets go of the stick and takes it back, the guest can't read
  it). Other real devices (serial adapters, keyboards, webcams, sound
  cards) not tried yet. The helper could tell the VMM why it gave up on
  a device, so the app shows it (today: the log only). A Developer ID
  signature would keep vroom-netd's permission across updates.
- USB disks as files or as a disk, with a real stick from the app (only
  disk images were tried here, and the folder of a volume): macOS's
  "Removable Volumes" prompt for vroom.app, and `authopen` opening a
  real `/dev/rdiskN` (its password prompt; whether macOS also wants
  Full Disk Access for it is not known). A card reader with several
  disks can only be passed as the device. Linux hosts: neither way yet
  (the device can be passed there).
- Things only a person at the Mac can try: bridged networking with real
  vmnet, a drag out of the guest to Finder, Mail or the Desktop with a
  real mouse, Coherence next to a window tiler, several monitors on
  several Mac screens.
- Apple's `com.apple.vm.device-access` entitlement, for real USB
  passthrough of devices macOS drivers hold (USB disks above all: macOS
  15.3 and later refuses them to any program without Full Disk Access,
  root included). It needs a paid Developer ID and Apple's approval of
  the request; with it, the root helper isn't needed for USB at all.
  Until then a USB disk can be shared as files or used as a disk (GUIDE,
  "USB disks").
- A signed, notarized vroom.app (`scripts/app.sh` builds it, signed ad
  hoc: needs a Developer ID and the hardened runtime), a disk image to
  install from, updates, crash reports.
- Drag and drop in Wayland guests (no live drag either way: an outside
  program can't take part in a Wayland drag; files land on the Desktop).
- Several monitors: one UI scale for all of them (a window per Mac
  screen with its own scale would want per-monitor scales), Direct3D on
  Windows' first monitor only (the others are copied to 2D GPUs),
  Coherence on the first monitor only (the others are off meanwhile).
- Network: adding an adapter while the VM runs needs PCI hotplug (a
  hotplug-capable root port, ACPI); it waits for the next start now.
  New port forwards on a running VM need listening sockets the sandbox
  doesn't allow (the app could open them and hand them over). Host-only
  on macOS gets no DHCP (see NETWORKING.md): netd could run a small DHCP
  server per host-only network. The netd fixes (frame buffers, a busy
  VMM drops frames, not the port) need netd set up again.
- Rosetta for Linux guests (x86_64 Linux programs). Apple only offers it
  through its Virtualization framework; we'd have to look like it.
- Benchmarks against VMware Fusion and Parallels on the same Mac, the
  losses published with the wins.
- Fuzzing the 3D command streams (later).
- Windows: when the desktop's own Direct3D process dies, the screen
  freezes until Win+Ctrl+Shift+B. The guest driver should report a lost
  device for that context (Windows then rebuilds it by itself).
- Windows with 3D and Edge open, suspend and resume (October 17): now
  and then the guest's display stays off after resume (the guest runs,
  its driver says OK, Windows sets no scanout): 3 of 10 resumes here,
  never without Edge. The VMM now watches for it: 15 s after Windows
  says its 3D display is on with nothing shown, it plugs the monitor in
  again, then turns the display device off and on (twice at most). One
  of those runs came back after the device was cycled; the replug step
  hasn't met a failing resume yet. The cause in Windows is not known.
- A Windows guest with 3D stopped once with "undecodable MMIO access at
  0x8200010000" (the 3D device's memory window, about 40 s after login)
  on a build from master 01a3d48. Not seen again: ten minutes on the
  next master with the old and new guest tools, resizes and Coherence.
  If it shows up again, the window's filler memory is the first suspect.
