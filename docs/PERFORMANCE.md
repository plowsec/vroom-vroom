# Performance

How fast vroom is, compared with QEMU (HVF) on the same Mac and the same
guest, and how to measure it again.

## Setup

- Apple M3 Max (16 cores, 64 GB), macOS 15. QEMU 11.1 from Homebrew
  (`-accel hvf -cpu host`, virtio-pci devices, `-netdev user`, default
  cache mode).
- Guest: Ubuntu 24.04 cloud image, 4 vCPUs, 4 GiB, kernel boot unless
  noted. Root disk qcow2; fio runs on a separate 2 GiB data disk
  (filled first, so qcow2 numbers are for allocated clusters).
- vroom: default settings (`sync = "fsync"`), `virtio_transport = "pci"`
  to match QEMU. "before" is commit d4bff82, "after" is this branch.
- The Mac was shared with other work while measuring (other VMs,
  builds). Runs of vroom before, vroom after and QEMU were interleaved;
  numbers are medians of 3 to 6 runs (2 for the qcow2 and NVMe disk
  tables) and still move by 10-20 % between runs. Small differences mean
  nothing.
- Disk data sits in the host's page cache (both VMMs use it), so these
  numbers show what the VMM adds, not what the SSD can do.

## Results

Medians. "4k random read qd1" also shows the time per request; for
vroom's NVMe, and now virtio-blk too, a lone request is done before the
guest's submit call returns, so fio's own latency figure would say
0.3-0.7 µs, which is not what the guest waits.

**Boot, idle, exits (4 vCPUs, 4 GiB)**

| | before | after | QEMU |
|---|---|---|---|
| boot to login, kernel boot (s) | 5.6 | 5.49 | 5.61 |
| boot to login, UEFI (s) | 10.5 | 10.4 | 11.9 |
| idle host CPU, kernel boot (%) | 0.6 | 0.6 | 0.5 |
| idle host CPU, UEFI (%) | 2.9 | 0.8 | 0.5 |
| RSS after boot, kernel boot (MiB) | 957 | 980 | 1113 |
| RSS after boot, UEFI (MiB) | 1176 | 1144 | 1062 |
| MMIO read + syscall in guest (µs) | 1.56 | 1.53 | 1.6 |
| pipe ping-pong between 2 vCPUs (µs) | 30.6 | 30.3 | 30.8 |

**Disk: virtio raw (fio, O_DIRECT)**

| | before | after | QEMU |
|---|---|---|---|
| 4k random read qd1 (IOPS) | 29k | 121k | 29k |
|   = µs per 4k read at qd1 | 34.5 | 8.29 | 34.3 |
| 4k random read qd32 (IOPS) | 270k | 245k | 121k |
| 4k random write qd1 (IOPS) | 28k | 105k | 27k |
| 4k random write qd32 (IOPS) | 203k | 218k | 110k |
| 1M read qd1 (GB/s) | 5.71 | 10.2 | 8.51 |
| 1M read qd32 (GB/s) | 10.1 | 27.2 | 37.5 |
| 1M write qd1 (GB/s) | 6.52 | 10.6 | 7.11 |
| 1M write qd32 (GB/s) | 8.53 | 17 | 16.6 |

**Disk: virtio qcow2 (fio, O_DIRECT)**

| | before | after | QEMU |
|---|---|---|---|
| 4k random read qd1 (IOPS) | 29k | 118k | 29k |
|   = µs per 4k read at qd1 | 34.5 | 8.45 | 34.9 |
| 4k random read qd32 (IOPS) | 281k | 243k | 117k |
| 4k random write qd1 (IOPS) | 30k | 106k | 28k |
| 4k random write qd32 (IOPS) | 223k | 221k | 107k |
| 1M read qd1 (GB/s) | 6.04 | 9.05 | 8.17 |
| 1M read qd32 (GB/s) | 8.87 | 27 | 34.8 |
| 1M write qd1 (GB/s) | 6.5 | 8.65 | 8.14 |
| 1M write qd32 (GB/s) | 7.22 | 17.9 | 17.8 |

**Disk: nvme raw (fio, O_DIRECT)**

| | before | after | QEMU |
|---|---|---|---|
| 4k random read qd1 (IOPS) | 116k | 116k | 27k |
|   = µs per 4k read at qd1 | 8.61 | 8.65 | 37.3 |
| 4k random read qd32 (IOPS) | 115k | 111k | 141k |
| 4k random write qd1 (IOPS) | 100k | 101k | 26k |
| 4k random write qd32 (IOPS) | 100k | 103k | 122k |
| 1M read qd1 (GB/s) | 8.3 | 12.4 | 9.73 |
| 1M read qd32 (GB/s) | 8.66 | 27.2 | 21.8 |
| 1M write qd1 (GB/s) | 8.57 | 11.7 | 9.12 |
| 1M write qd32 (GB/s) | 6.35 | 15.8 | 15.5 |

**Disk: nvme qcow2 (fio, O_DIRECT)**

| | before | after | QEMU |
|---|---|---|---|
| 4k random read qd1 (IOPS) | 114k | 113k | 29k |
|   = µs per 4k read at qd1 | 8.74 | 8.83 | 34.6 |
| 4k random read qd32 (IOPS) | 114k | 108k | 143k |
| 4k random write qd1 (IOPS) | 102k | 108k | 28k |
| 4k random write qd32 (IOPS) | 102k | 93k | 123k |
| 1M read qd1 (GB/s) | 7.67 | 12.2 | 9.71 |
| 1M read qd32 (GB/s) | 8.33 | 27 | 20.8 |
| 1M write qd1 (GB/s) | 7.6 | 11.3 | 9.21 |
| 1M write qd32 (GB/s) | 5.8 | 15.5 | 14.9 |

**Network (NAT, virtio-net)**

| | before | after | QEMU |
|---|---|---|---|
| TCP guest -> host (Gbit/s) | 3.88 | 20.9 | 3.37 |
| TCP host -> guest (Gbit/s) | 7.47 | 17.7 | 8.75 |
| UDP guest -> host, 3 Gbit/s offered (Gbit/s) | 3 | 3 | 1.9 |
| TCP host -> guest, port forward (Gbit/s) | 6.37 | 17.7 | 8.06 |
| TCP guest -> host, port forward (Gbit/s) | 3.9 | 22.3 | 3.26 |
| 1-byte TCP round trip, port forward (µs) | 63 | 61 | 73 |
| ping to the gateway (ms) | - | 0.229 | 0.309 |

## What changed

- **Disk requests run in parallel.** virtio-blk handed one request at a
  time to the disk image from a single thread. Now several requests in
  flight go to I/O threads: four per disk, more (up to 32) while requests
  wait for one. NVMe ran every command on the
  vCPU that rang the doorbell; now big transfers, flushes and commands
  queued behind others go to I/O threads too. Pausing (snapshots,
  suspend), resets and queue deletion wait for them, so a paused VM still
  has no I/O in flight.
- **A lone small request runs on the vCPU.** While the guest sends one
  request at a time (queue depth 1), virtio-blk now handles it on the
  vCPU that kicked the queue, as NVMe already did: no thread wakeups,
  which cost tens of microseconds each on macOS. It switches to the I/O
  threads as soon as the guest keeps several in flight, and checks now
  and then whether it went back to one at a time.
- **A virtio-blk queue per vCPU**, so each completion interrupts the vCPU
  that asked instead of waking another one first.
- **No bounce buffers.** Disk data moves between the image file and guest
  RAM with `preadv`/`pwritev` on the guest pages (bounds-checked, no Rust
  references into guest memory). qcow2 looks clusters up under its lock
  and does the data I/O outside it, one call per run of adjacent
  clusters; freeing clusters waits for that I/O, so a cluster can't be
  reused (say, as an L2 table) under a request still using it. A newly
  allocated cluster is written once (old contents and new data together)
  instead of twice.
- **Fewer interrupts.** NVMe sends one interrupt per doorbell, not one
  per command.
- **PCI MMIO without a global lock.** All PCI BAR accesses went through
  one lock, so a vCPU ringing the network card waited for another doing
  a disk flush. Each device keeps its own lock.
- **NAT batches.** Frames the guest queues go through the TCP/IP stack
  together: one host write per connection per batch instead of one per
  1500-byte frame. The first 16 TCP connections get 512 KiB buffers
  instead of 64 KiB: the buffer is the TCP window, and with the guest
  tens of microseconds away the window was the limit (64 KiB caps a
  stream near 6 Gbit/s). A guest opening the maximum of 512 connections
  still can't make the NAT allocate more than about 80 MiB. The NAT no
  longer re-checks checksums of frames from the guest (they came through
  RAM; a guest can send any bytes with a right checksum anyway).
- **Pings to the gateway answer** (they were dropped: a smoltcp feature
  was off).
- **VM window.** Damage rectangles from the VMM, and a window that
  converts only what changed into a reused buffer, landed separately
  (the hidpi work); on a 5120x2880 screen that turns an 8 ms conversion
  per frame into a fraction of a millisecond for a changed text line.
- **Memory.** A kernel-booted VM kept the unpacked kernel (about 50 MB
  for Ubuntu's) for resets; it keeps the file as read and unpacks it
  again on reset. The VMM's own heap after a kernel boot went from about
  85 MB to 45 MB (most of what is left is the kernel file and initrd).
  RSS in the table is mostly guest pages the guest touched, so it moves
  with what the guest did.
- **ramfb** (the firmware framebuffer, always there with UEFI) was
  copied out of guest RAM into a fresh 4 MB buffer 30 times a second and
  compared with the last copy. Now two buffers are reused, only the rows
  that changed go to the window, it looks 4 times a second once the
  screen is still, and once a second with no window at all. UEFI guests
  idle at 0.8 % of a core instead of 2.9 %.

**Disk from the SSD (virtio raw, 60 GB of random data, host cache can't
hold it, `scripts/bench-cold.sh`)**

| | vroom | QEMU |
|---|---|---|
| 4k random read qd1 (IOPS) | 9.1k | 7.9k |
|   mean / p99 latency (µs) | 109 / 126 | 125 / 191 |
| 4k random read qd32 (IOPS) | 136k | 121k |
|   mean / p99 latency (µs) | 234 / 448 | 265 / 485 |
| 64k random read qd1 (IOPS) | 5.2k | 4.7k |

These are the numbers a real workload with a cold cache sees; the tables
above mostly measure the VMM's own cost. fio's latency for vroom at queue
depth 1 in the tables above (well under a microsecond) is not real: the
request finishes before fio's submit call returns, so fio times nothing.
The per-request time in the "µs per 4k read" rows is the real one.

## 3D display

Ubuntu 24.04, Xorg in the app's window, M3 Max. "before" is master
344e5e8 (each GL frame read back and copied four times on the CPU on its
way to the window, Vulkan window frames copied into GL with the CPU),
"after" the IOSurface paths (ARCHITECTURE.md, "The screen straight from
the GPU"). CPU: cores used by the app / the VM process (vCPUs included)
/ the GPU process.

| | before | after |
|---|---|---|
| glmark2, 1280x760 window, screen 1280x800 | 1306 | 1932 |
| CPU during it | 0.69 / 0.50 / 0.70 | 0.03 / 0.37 / 0.72 |
| glmark2, 2560x1560, screen 2560x1600 | 617 | 1827 |
| CPU during it | 0.98 / 0.57 / 0.57 | 0.04 / 0.36 / 0.70 |
| glmark2 on Zink, 2560x1560 | 228 | 759 |
| `vkcube` 1280x760, screen 1280x800 | 546 fps | 1004 fps |
| `vkcube` 2560x1560, screen 2560x1600 | 188 fps | 781 fps |
| GNOME on Wayland, glmark2 1200x700 | 996 | 1973 |

glmark2 is six scenes (build, texture, shading, terrain, refract,
desktop; four on Wayland), 5 s each. Light scenes gain most (1900 to
2800 fps): their frames no longer wait for the copies, and GL fences are
looked at every 250 µs instead of every millisecond. Without the second
change they stopped near 1200 fps, below before, because the copies had
been waking the device thread early. The GPU-heavy terrain scene gains
10 to 15 %.

## Guest features

Measurements behind [GUIDE.md](GUIDE.md); M3 Max unless said.

### OpenGL (Linux guests)

Speed, Ubuntu 22.04 live desktop (X11), 1280×800, 4 vCPUs, M3 Max,
glmark2 2021.02 at its default 800×600 window, `opengl_host = "apple"`
against llvmpipe in the same guest:

| | llvmpipe (CPU) | `apple` |
|---|---|---|
| glmark2 score (full run) | 676 | 1059 |
| terrain | 13 fps | 388 to 448 fps |
| refract | 40 fps | 981 to 1071 fps |
| desktop (blur) | 159 fps | 892 to 1006 fps |

Anything with real work is many times faster, and the guest's CPUs stay
free.

### Zink

Zink is a little slower than virgl for ordinary drawing: in a GNOME
Wayland session, glmark2 scores about 1000 on Zink and 1170 on virgl, and
its terrain scene, the heaviest for the GPU, runs at about 210 fps against
420. Keep virgl for the desktop and use Zink for apps that need more than
4.1. A compute shader test (1M invocations, 4096
`sin` each) takes 9.4 ms on Zink, 3.1 s on llvmpipe.

### Vulkan apps through snapshots

Vulkan apps keep running through snapshots and suspend too, windows
included (vkcube in Xorg, OpenGL apps on Zink). Each Vulkan app's
renderer keeps the commands that made its objects (and the latest
recording of each command buffer, the latest descriptor writes); a
snapshot stops the app's renderer between two commands, waits for the
GPU, and copies out its memory and images. On resume a fresh renderer
replays the commands, puts the contents back and carries on where the
app's command ring stood. The app notices nothing. Measured, Ubuntu 24.04,
4 GiB guest:

| Running | Kept per app | Saving it | Building it again |
|---|---|---|---|
| a compute loop (8 MiB of buffers) | 44 commands, 17.6 MiB | 6 to 9 ms | 0.4 s |
| an off-screen drawing test (textures with layers and mipmaps) | 58 commands, 18.4 MiB | 8 to 13 ms | 0.46 s |
| vkcube in an Xorg window | 216 commands, 17.8 MiB | 10 to 16 ms | 0.47 to 0.52 s |
| glxgears on Zink | 380 commands, 28.9 MiB | | 0.44 s |
| the compute loop with 1 GiB of buffers | 44 commands, 1033 MiB | 0.33 s | 0.8 s |

About 9 MiB of each app's share is the guest driver's own command and
reply memory. With vkcube, glxgears, the compute loop and the drawing
test all open, suspend took 5.2 s and resume 5.6 s, and all four went on
(the two tests check their results: no wrong values, the same pictures as
before). With 1 GiB of Vulkan memory the snapshot took 9 s and 2 GB on
disk (the GPU's part is stored as base64 for now, a third larger).

### Direct3D 11 suspend (Windows guests)

Measured (M3 Max, 8 GB guest, Edge and a Direct3D app open, 20
suspends and resumes): before, 77 crash reports (about 4 per suspend:
Explorer, the Start menu, the text input host, DWM, the app), one
suspend refused, one resume that hung; after (42 suspends), no crash
in the Direct3D driver, no refusal, no hang. Suspend about 13 s,
resume about 10 s. In 2 to 4 resumes of 12, Windows shows nothing on
its 3D display for about a minute, then comes back by itself; vroom
turns the display device off and on meanwhile, which sometimes helps.
If it stays black, suspend and resume again. With an OpenGL (Zink)
and a Vulkan app open as well (8 suspends): no crash in the Direct3D
driver; the Vulkan app stops at each save (lost device), the OpenGL
one keeps running, except once when it crashed in our OpenGL driver.

### OpenGL and Vulkan in Windows guests

Frames come back from the Mac and are drawn into the window with GDI
(the desktop compositor runs on Direct3D and can't see Vulkan's
images). Measured at 640x480 (M3 Max): a Vulkan spinning cube about
1000 frames per second; a spinning triangle in OpenGL 385 on Zink, 350
on virgl, without vsync (OpenGL apps wait for vsync by default: 60). A
compute shader over 1M values takes 5 to 10 ms in Vulkan, 16 ms in
OpenGL. Big windows are slower.

### Video decoding

### What it saves

On an M3 Max, guest with 4 vCPUs, decoding to nowhere (hardware:
GStreamer or our V4L2 client; software: FFmpeg on all 4 vCPUs):

| Stream | Hardware: time, guest CPU | Software: time, guest CPU |
|---|---|---|
| H.264 1080p, 12 Mbit/s, 600 pictures | 1.2 s, 0.15 s | 1.3 to 1.9 s, 4.5 to 5.5 s |
| H.264 4K, 30 Mbit/s, 300 pictures | 2.2 s, 0.1 s | 3.4 s, 11.4 s |
| VP9 1080p, 10 Mbit/s, 600 pictures | 2.0 s, 0.25 s | 3.9 s, 6.6 s |
| AV1 1080p, 4 Mbit/s, 900 pictures | 1.0 s, 0.2 s | 1.9 s, 3.2 s |

That is 140 to 900 pictures a second, far more than playing needs.

Firefox playing 1080p files, its decoding process: H.264 at 12 Mbit/s
from 0.24 CPUs to 0.02, VP9 at 10 Mbit/s from 0.35 to 0.02, HEVC at
8 Mbit/s from 0.52 to 0.02. The
picture is the same (bit for bit, checked with FFmpeg's VA-API decoding
against FFmpeg's own decoders: H.264 with B-frames, slices, scaling
matrices, weighted prediction; HEVC 8 and 10-bit with B-frame pyramids,
slices, open GOPs; VP9 8 and 10-bit with hidden frames).

Chrome 155, 1080p at 30 frames a second, in CPUs (1.0 is one guest CPU
busy), before and after Chrome could use the GPU:

| | Before: GPU process, page, whole guest | Now: GPU process, page, whole guest |
|---|---|---|
| H.264, 12 Mbit/s | 0.04, 0.24, 0.45 | 0.13, 0.02, 0.29 |
| VP9, 22 Mbit/s | 0.04, 0.35, 0.49 | 0.16, 0.02, 0.30 |
| Video call with itself, 720p VP8 (camera) | 0.04, 0.17, 0.43 | 0.04, 0.19, 0.48 |
| The same in VP9 | | 0.09, 0.14, 0.49 |

The decoding moves out of the page's process; the GPU process does the
copies into GPU memory. A call costs about the same: sending was in
software either way (for H.264 it no longer is, see
[Video encoding](#video-encoding)).

Since October 18, 2026 the driver no longer spins while it waits for a
picture (it asked `poll()` about free bitstream buffers too, which are
nearly always there): Chrome playing a 1080p30 H.264 clip at 12 Mbit/s
under Wayland, GPU process 0.13 to 0.06, whole guest 0.35 to 0.28 (same
clip, old and new driver, two runs each). Run `install-vaapi.sh` again
to get it.

The same videos in GNOME on Xorg (mean of two runs), before and after
the X11 support (October 17, 2026):

| | Before: GPU process, page, whole guest | Now: GPU process, page, whole guest |
|---|---|---|
| H.264, 12 Mbit/s | 0.04, 0.28, 0.44 | 0.09, 0.02, 0.28 |
| VP9, 22 Mbit/s | 0.04, 0.49, 0.64 | 0.15, 0.02, 0.34 |
| HEVC, 8 Mbit/s | doesn't play | 0.10, 0.02, 0.28 |

No pictures dropped either way. The GPU process's share includes the
conversion to BGRA (about 0.03 of a CPU at 1080p30).

### Video encoding

FFmpeg 6.1 encoding 300 frames of NV12 at 4 Mbit/s (time, then guest
CPU: user and system), on an M3 Max with 4 vCPUs; PSNR of the result
against its source:

| | 1280x720 | 1920x1080 |
|---|---|---|
| `h264_v4l2m2m` | 0.46 s, 0.15 s; 42.0 dB | 0.84 s, 0.64 s; 39.6 dB |
| `hevc_v4l2m2m` | | 0.87 s, 0.66 s |
| `h264_vaapi` | 0.93 s, 0.98 s; 44.8 dB | 1.48 s, 1.77 s; 40.6 dB |
| `hevc_vaapi` | 0.93 s, 1.00 s; 46.1 dB | 1.35 s, 1.50 s (1920x1088) |
| `libx264 -preset veryfast` (software) | 0.77 s, 2.10 s; 46.2 dB | 1.23 s, 3.64 s; 41.0 dB |

That is 200 to 650 frames a second. Through V4L2 the guest's CPU barely
works; through VA-API, FFmpeg's upload and the driver's copies cost
about 3 ms of guest CPU a frame. FFmpeg's V4L2 encoders put a key frame
every 12 frames by default (`-g` changes that), hence their lower PSNR.
All of these decode back cleanly. GStreamer's `v4l2h264enc` (720p,
decodes back cleanly) and `v4l2h265enc` (1080p, 300 frames) work too.

Chrome 155 in a call with itself (H.264, 720p camera, at most 2.5
Mbit/s, received through the hardware decoder), in CPUs (1.0 is one
guest CPU busy): GPU process, page, whole guest; mean of three 30-second
runs each:

| | OpenH264 (software) | VideoToolbox |
|---|---|---|
| Wayland: the VM's camera (test colour bars, about 170 kbit/s) | 0.06, 0.11, 0.44 | 0.08, 0.07, 0.44 |
| Wayland: a noisy 720p clip as the camera (2.5 Mbit/s) | 0.07, 0.16, 0.47 | 0.10, 0.09, 0.45 |
| Xorg: the VM's camera | 0.06, 0.09, 0.41 | 0.08, 0.06, 0.41 |
| Xorg: the noisy clip | 0.07, 0.15, 0.44 | 0.10, 0.09, 0.42 |

The encoding leaves the page's process (0.03 to 0.07 of a CPU less); the
GPU process takes 0.02 to 0.03 more: it converts each camera frame to
NV12 for the driver, which hands it to the device. For the guest as a
whole it comes out about even at these sizes. With the noisy clip,
OpenH264 sent 960x540 at that bitrate in every run; VideoToolbox kept
1280x720 in two runs of six. Each frame takes longer: 5 to 7.5 ms from Chrome's
side (`totalEncodeTime` over `framesEncoded`), against 2 to 3.5 ms for
OpenH264; about 5 ms of it is VideoToolbox's low-latency encoder itself.

## Trade-offs

- 4 KiB random I/O at queue depth 32 on virtio-blk is about 10 % below
  where it was (the guest now sends each request to its own vCPU's queue
  and kicks more often), still about twice QEMU's. NVMe at that depth is
  unchanged.
- A fixed four I/O threads looked best with the data in the host cache,
  but from the SSD it allowed only four reads at once: 4 KiB random reads
  at queue depth 32 ran at 43k IOPS, against QEMU's 121k. The pool now
  grows to the number of requests in flight (up to 32): 136k from the
  SSD. From the cache, 4 KiB at queue depth 32 drops from 251k to 205k
  IOPS (QEMU: 120k) and 1 MiB reads go from 25 to 35 GB/s.
- The virtio-blk device now has one queue per vCPU (up to 16), so a
  suspend image made by an older vroom of a VM with virtio disks no
  longer resumes ("queue layout mismatch"): start it with `--fresh`.
- Vulkan and Direct3D contexts each run in a process of their own (see
  SECURITY.md). Measured against one shared process, M3 Max, a busy
  host, so a few runs each:

  | | one process | a process per context |
  |---|---|---|
  | glmark2 (Ubuntu 24.04, GL, off-screen) | 488, 508, 498 | 430, 444, 540 |
  | Vulkan compute, 1M invocations | 4.1-4.4 TFLOP/s | 4.2-4.4 TFLOP/s |
  | small Vulkan program, start to end | 0.35 s | 0.40 s |
  | Edge window (Windows 11), first / later | 1.49 / 0.35-0.40 s | 1.42-1.78 / 0.29-0.44 s |
  | Notepad window | 52-82 ms | 57-87 ms |

  GL doesn't change (it stays in the GPU process; glmark2 moves 10 % from
  run to run here). Drawing doesn't either. What costs is a new context:
  about 50 ms to start the process, load MoltenVK or DXMT and make a
  Metal device. Memory: a Windows desktop with Edge open runs 15 to 20
  contexts, 200 to 400 MB more in all.

## Looked at, left alone

- **Idle CPU.** An idle guest costs about half a percent of one host
  core, like QEMU. (QEMU's UEFI runs here had no display device at all;
  vroom always has ramfb with UEFI.) On macOS 15, HVF handles the guest's WFI itself (our WFI
  emulation, with its 1 ms re-check for IPIs, is never reached), so
  spinning before sleeping (KVM's halt polling) gained nothing and was
  dropped. Spinning in the device workers before they sleep made things
  slower on a busy Mac (the spinning thread competes with the vCPUs), so
  that was dropped too.
- **MMIO exits** cost about the same as QEMU's (~1.6 µs per guest
  register read, syscall included).
- **Boot** is dominated by the guest (systemd, cloud-init): both VMMs
  reach the login prompt within a few hundred milliseconds of each other.

## Still slow, and what it would take

- **4 KiB random I/O at queue depth 32 on NVMe** stays near 100k IOPS:
  every command costs an MMIO exit for the doorbell and an interrupt on
  the submitting vCPU. NVMe's doorbell buffer (shadow doorbells, which
  QEMU implements) would let the guest skip most doorbell exits.
- **Network** has no offloads: the guest sends and receives 1500-byte
  frames and computes every checksum. TSO and checksum offload on
  virtio-net (NAT only: it terminates TCP, so big segments are fine for
  it) would cut the guest's per-packet work; on the receive side smoltcp
  computes a checksum per frame the guest could skip.
- **VM window, 2D**: without 3D (and whenever something covers the 3D
  screen) Slint uploads the whole image to the GPU each frame. Partial
  texture updates need a lower-level path than `slint::Image`.
- **NAT latency** (a 1-byte TCP round trip through a port forward takes
  about 60 µs) is thread wakeups: host socket to the device worker to the
  vCPU and back. Polling instead of sleeping would cut it, at a CPU cost
  that hurt more than it helped on a busy Mac.
- **UDP** throughput wasn't pushed past the 3 Gbit/s the test offers.

## Measuring

`scripts/bench.py` boots a guest under vroom or QEMU, runs the tests over
ssh and prints one JSON line per run (also appended to
`WORKDIR/results.jsonl`); `scripts/bench.py --table results.jsonl` prints
the medians per tag as above.

One-time setup of a work directory (about 3 GB):

```bash
W=~/vroom-bench; mkdir -p $W/seed; cd $W
cp ../images/ubuntu/{vmlinuz,initrd} .
qemu-img convert -O qcow2 ../images/ubuntu/noble.img root.qcow2
qemu-img resize root.qcow2 12G
ssh-keygen -t ed25519 -N '' -f id_bench
# seed/user-data: cloud-config with ubuntu's ssh key = id_bench.pub,
#   packages: [fio, iperf3]; seed/meta-data: instance-id, hostname
hdiutil makehybrid -iso -joliet -default-volume-name cidata -o seed.iso seed
brew install iperf3
```

Boot it once with vroom so cloud-init installs the packages, then in the
guest replace cloud-init's network config with one that runs DHCP on any
`e*` interface (cloud-init's matches the first NIC's MAC, and QEMU's
differs).

Then, for example:

```bash
scripts/build.sh --release
scripts/bench.py $W --tag after                      # boot, idle, exits, fio, network
scripts/bench.py $W --vmm qemu --tag qemu
scripts/bench.py $W --bus nvme --fmt qcow2 --tests fio
scripts/bench.py $W --boot uefi --tests boot,idle
scripts/bench.py --table $W/results.jsonl
```

What each test does is in the script's header. The data disk is made
fresh for every run and deleted after it. Network tests need
`allow_host_loopback` (the script sets it): guest-to-host traffic goes to
`10.0.2.2`, which both NATs map to the host's loopback.
