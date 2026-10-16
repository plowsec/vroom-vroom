# Roadmap

What is left before vroom can replace VMware Fusion, Parallels or
VirtualBox for everyday use, roughly in the order we mean to do it. Items
move to the docs (GUIDE, ARCHITECTURE) once they work.

## In progress

- **Isochronous USB transfers** for passed-through devices (USB sound
  cards, webcams macOS lets go of). The Mac's own camera already reaches
  the guest (`camera = true`).
- **Coherence** (VMware: Unity): each guest app window as a window of its
  own on the Mac desktop, without the guest's desktop around it.

## 3D: catching up with VMware and Parallels

Where we are: Linux gets OpenGL 4.1 and Vulkan (off screen only);
Windows gets Direct3D 11. VMware Fusion gives Windows on Apple Silicon
Direct3D 11 and OpenGL 4.3, and Linux OpenGL 4.3.

Linux guests:

- **OpenGL 4.3 and up** (compute shaders). macOS's own OpenGL stops at
  4.1 and never moves. The way past it is OpenGL on top of Vulkan in the
  guest (Mesa's Zink), over Venus. Zink needs a Vulkan feature MoltenVK
  doesn't have (`nullDescriptor`). Paths: LunarG's KosmicKrisp (a newer
  Vulkan-on-Metal driver in Mesa) if it has it, a MoltenVK patch, or
  faking it in virglrenderer's Venus decoder. That gives OpenGL 4.6.
- **Vulkan apps showing a window** (`VK_KHR_swapchain`), which also
  unlocks Proton/DXVK games. The guest's Venus driver needs buffers it
  can hand to the guest's display (DRM format modifiers), which MoltenVK
  doesn't offer either; same paths as above, plus our side of blob
  scanout.
- **Snapshots and suspend while a Vulkan app runs.** Vulkan state lives
  on the Mac's GPU. Like we do for OpenGL, we'd follow what each Vulkan
  app creates and rebuild it on resume; Vulkan has far more kinds of
  objects, so this is the biggest of these items.
- **Hardware video decoding** (H.264, HEVC, VP9, AV1 in browsers and
  players): a virtio video device decoding with the Mac's VideoToolbox,
  and the guest's VA-API on top of it.

Windows guests:

- **Direct3D 12**, the one most games since 2020 need. It is also the key
  to the next two: Microsoft's "OpenCL, OpenGL and Vulkan Compatibility
  Pack" runs OpenGL and Vulkan apps on top of Direct3D 12.
- **OpenGL and Vulkan apps** (VMware has OpenGL 4.3): through that pack
  once Direct3D 12 works, or our own driver path.
- **Hardware video decoding** (Direct3D 11 video: Edge, Chrome, the Movies
  app, Teams), again on VideoToolbox.

## Before it's ready for people other than us

- Test on other Macs (M1 to M4, base models with 8 GB) and macOS versions,
  and days-long runs: sleep and wake with VMs running, full disks.
- Windows 3D: the text that sometimes doesn't draw (an agent is on it).
- USB passthrough with real devices, bridged networking with real
  vmnet (being tested now).
- Signing and notarizing vroom.app (`scripts/app.sh` builds it, signed
  ad hoc: needs a Developer ID and the hardened runtime), a disk image
  to install from, updates, crash reports.
- Multiple monitors, drag and drop from the guest to the Mac (Mac to
  guest works), a live drag in Linux guests (they get files on the
  Desktop for now).
- Rosetta for Linux guests (x86_64 Linux programs). Apple only offers it
  through its Virtualization framework; we'd have to look like it.
- Benchmarks against VMware Fusion and Parallels on the same Mac, the
  losses published with the wins.
- 3D processes per guest context (one GPU process per app, not per VM).
- Fuzzing the 3D command streams (later).
