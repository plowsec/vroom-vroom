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

Where we are: Linux gets OpenGL 4.1 (virgl), Vulkan 1.3 in windows and
off screen (Venus), and OpenGL 4.4 for the apps you start on Zink (below);
Windows gets Direct3D 11. VMware Fusion gives Windows on Apple Silicon
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
- **Zink speed and gaps**: about half virgl's speed on glmark2's terrain,
  and glmark2's refract scene fails (a depth render target Zink can't
  make on MoltenVK). virgl stays the default; Zink is per app.
- **No copy for Vulkan windows**: an IOSurface shared by Metal and
  OpenGL instead of copying each frame. The copy is fine at 1280x800.
- **Snapshots and suspend while a Vulkan app runs.** Vulkan state lives
  on the Mac's GPU. Like we do for OpenGL, we'd follow what each Vulkan
  app creates and rebuild it on resume; Vulkan has far more kinds of
  objects, so this is the biggest of these items.
- **Hardware video decoding** (H.264, HEVC, VP9, AV1 in browsers and
  players): a virtio video device decoding with the Mac's VideoToolbox,
  and the guest's VA-API on top of it.
- **Xwayland under GNOME on virgl**: its GL context breaks at login
  (`vrend_clear_surface: ... Illegal surface 0`), so X11 apps in a
  GNOME Wayland session show nothing. Not new; seen with the old library
  too.

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
- A signed, notarized app with an installer and updates; crash reports.
- Multiple monitors, drag and drop from the guest to the Mac (Mac to
  guest works), a live drag in Linux guests (they get files on the
  Desktop for now).
- Rosetta for Linux guests (x86_64 Linux programs). Apple only offers it
  through its Virtualization framework; we'd have to look like it.
- Benchmarks against VMware Fusion and Parallels on the same Mac, the
  losses published with the wins.
- 3D processes per guest context (one GPU process per app, not per VM).
- Fuzzing the 3D command streams (later).
