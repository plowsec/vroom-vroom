# Performance

vroom vs QEMU (HVF), same Mac, same guest. How to rerun: [Measuring](#measuring).

## Setup

| | |
|---|---|
| host | Apple M3 Max (16 cores, 64 GB), macOS 15 |
| QEMU | 11.1, Homebrew; `-accel hvf -cpu host`, virtio-pci, `-netdev user`, default cache mode |
| guest | Ubuntu 24.04 cloud image, 4 vCPUs, 4 GiB, kernel boot unless noted. Root disk qcow2; fio on a separate 2 GiB data disk, filled first (qcow2 numbers = allocated clusters) |
| vroom | defaults (`sync = "fsync"`), `virtio_transport = "pci"` to match QEMU. "before" = d4bff82, "after" = this branch |

- Mac shared with other work (VMs, builds). vroom before/after and QEMU runs interleaved.
- Medians of 3–6 runs (2 for the qcow2 and NVMe disk tables); still ±10–20% run to run. Small differences mean nothing.
- Disk data in the host page cache (both VMMs): numbers show VMM overhead, not SSD speed.

## Results

Medians. "µs per 4k read at qd1" is the real per-request time. fio's own latency for vroom NVMe and virtio-blk at qd1 reads 0.3–0.7 µs: a lone request completes before the guest's submit call returns, so fio times nothing.

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

**Disk from the SSD** (virtio raw, 60 GB random data, too big for host cache, `scripts/bench-cold.sh`). What a cold-cache workload sees; the tables above mostly measure VMM cost.

| | vroom | QEMU |
|---|---|---|
| 4k random read qd1 (IOPS) | 9.1k | 7.9k |
|   mean / p99 latency (µs) | 109 / 126 | 125 / 191 |
| 4k random read qd32 (IOPS) | 136k | 121k |
|   mean / p99 latency (µs) | 234 / 448 | 265 / 485 |
| 64k random read qd1 (IOPS) | 5.2k | 4.7k |

## What changed

| Change | Detail |
|---|---|
| parallel disk requests | virtio-blk: in-flight requests go to I/O threads, 4 per disk, up to 32 while requests wait. NVMe: big transfers, flushes, commands queued behind others go to I/O threads (was: all on the doorbell vCPU). Pause (snapshot, suspend), reset, queue deletion wait for them: paused VM has no I/O in flight |
| lone small request on the vCPU | virtio-blk at qd1 runs on the kicking vCPU, like NVMe: no thread wakeups (tens of µs each on macOS). Switches to I/O threads when several are in flight; rechecks now and then |
| virtio-blk queue per vCPU | completion interrupts the asking vCPU directly |
| no bounce buffers | `preadv`/`pwritev` straight on guest pages (bounds-checked, no Rust references into guest memory). qcow2: cluster lookup under its lock, data I/O outside it, one call per run of adjacent clusters. Freeing clusters waits for that I/O (no reuse, e.g. as L2 table, under a live request). New cluster written once (old contents + new data) instead of twice |
| fewer interrupts | NVMe: one interrupt per doorbell, not per command |
| PCI MMIO without a global lock | was: all BAR accesses under one lock (a NIC kick waited for a disk flush). Now a lock per device |
| NAT batches | guest-queued frames go through the TCP/IP stack together: one host write per connection per batch, not per 1500-byte frame |
| NAT buffers | first 16 TCP connections: 512 KiB buffers, not 64 KiB. Buffer = TCP window; 64 KiB caps a stream near 6 Gbit/s at tens of µs RTT. Max 512 connections still ≤ ~80 MiB of NAT memory |
| NAT checksums | guest frames no longer re-checked (came through RAM; a guest can send any bytes with a valid checksum anyway) |
| gateway pings | answered (were dropped: a smoltcp feature was off) |
| VM window | damage rectangles from the VMM; window converts only what changed into a reused buffer (hidpi work). 5120x2880: 8 ms per frame → fraction of a ms for a changed text line |
| memory | kernel-booted VM keeps the kernel file as read, unpacks again on reset (was: unpacked kernel kept, ~50 MB for Ubuntu). VMM heap after kernel boot ~85 → 45 MB (rest: mostly kernel file, initrd). RSS in the table is mostly guest-touched pages |
| ramfb (UEFI framebuffer) | was: copied from guest RAM into a fresh 4 MB buffer 30×/s and compared. Now 2 reused buffers, only changed rows sent, 4×/s once still, 1×/s with no window. UEFI idle 2.9% → 0.8% of a core |

## 3D display

Ubuntu 24.04, Xorg in the app window, M3 Max. "before" = master 344e5e8 (each GL frame read back and copied 4× on the CPU; Vulkan window frames copied into GL by the CPU). "after" = IOSurface paths ([ARCHITECTURE.md](ARCHITECTURE.md#the-screen-straight-from-the-gpu-macos)). CPU = cores used by app / VM process (vCPUs included) / GPU process.

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

- glmark2: 6 scenes (build, texture, shading, terrain, refract, desktop; 4 on Wayland), 5 s each.
- Light scenes gain most (1900 to 2800 fps): no waiting for copies, and GL fences polled every 250 µs instead of 1 ms. Without the fence change they stopped near 1200 fps, below before (the copies had been waking the device thread early).
- GPU-heavy terrain: +10–15%.

## Guest features

Behind [GUIDE.md](GUIDE.md). M3 Max unless said.

### OpenGL (Linux guests)

Ubuntu 22.04 live desktop (X11), 1280×800, 4 vCPUs; glmark2 2021.02, default 800×600 window; `opengl_host = "apple"` vs llvmpipe in the same guest:

| | llvmpipe (CPU) | `apple` |
|---|---|---|
| glmark2 score (full run) | 676 | 1059 |
| terrain | 13 fps | 388 to 448 fps |
| refract | 40 fps | 981 to 1071 fps |
| desktop (blur) | 159 fps | 892 to 1006 fps |

Real work: many times faster, guest CPUs stay free.

### Zink

| GNOME Wayland | Zink | virgl |
|---|---|---|
| glmark2 | ~1000 | ~1170 |
| terrain (heaviest GPU scene) | ~210 fps | ~420 fps |

- virgl for the desktop; Zink for apps needing >4.1.
- Compute shader test (1M invocations, 4096 `sin` each): 9.4 ms on Zink, 3.1 s on llvmpipe.

### Vulkan apps through snapshots

Vulkan apps survive snapshots and suspend, windows included (vkcube in Xorg, OpenGL apps on Zink).

- Each app's renderer keeps the commands that made its objects, the latest recording of each command buffer, the latest descriptor writes.
- Snapshot: stop the renderer between two commands, wait for the GPU, copy out memory and images.
- Resume: fresh renderer replays the commands, restores contents, continues from the app's command ring. App notices nothing.

Ubuntu 24.04, 4 GiB guest:

| Running | Kept per app | Saving it | Building it again |
|---|---|---|---|
| a compute loop (8 MiB of buffers) | 44 commands, 17.6 MiB | 6 to 9 ms | 0.4 s |
| an off-screen drawing test (textures with layers and mipmaps) | 58 commands, 18.4 MiB | 8 to 13 ms | 0.46 s |
| vkcube in an Xorg window | 216 commands, 17.8 MiB | 10 to 16 ms | 0.47 to 0.52 s |
| glxgears on Zink | 380 commands, 28.9 MiB | | 0.44 s |
| the compute loop with 1 GiB of buffers | 44 commands, 1033 MiB | 0.33 s | 0.8 s |

- ~9 MiB of each app's share: the guest driver's own command and reply memory.
- All four open (vkcube, glxgears, compute loop, drawing test): suspend 5.2 s, resume 5.6 s; all continued. Both tests self-check: no wrong values, same pictures.
- 1 GiB of Vulkan memory: snapshot 9 s, 2 GB on disk (GPU part stored as base64 for now, a third larger).

### Direct3D 11 suspend (Windows guests)

M3 Max, 8 GB guest, Edge + a Direct3D app open:

| | suspends | crash reports | refused | hung resume |
|---|---|---|---|---|
| before | 20 | 77 (~4 per suspend: Explorer, Start menu, text input host, DWM, the app) | 1 | 1 |
| after | 42 | 0 in the Direct3D driver | 0 | 0 |

- Suspend ~13 s, resume ~10 s.
- 2–4 resumes of 12: Windows shows nothing on its 3D display for ~1 min, then recovers. vroom toggles the display device meanwhile (sometimes helps). Stays black: suspend and resume again.
- Plus a Zink OpenGL app and a Vulkan app (8 suspends): no Direct3D driver crash; Vulkan app stops at each save (lost device); OpenGL app keeps running, except one crash in our OpenGL driver.

### OpenGL and Vulkan in Windows guests

Frames come back from the Mac, drawn into the window with GDI (the compositor runs on Direct3D, can't see Vulkan images). 640x480, M3 Max:

| | |
|---|---|
| Vulkan spinning cube | ~1000 fps |
| OpenGL spinning triangle, no vsync | 385 (Zink), 350 (virgl). Default vsync: 60 |
| compute shader over 1M values | 5–10 ms Vulkan, 16 ms OpenGL |

Big windows are slower.

### Video decoding

Guest 4 vCPUs, decoding to nowhere. Hardware: GStreamer or our V4L2 client. Software: FFmpeg on all 4 vCPUs.

| Stream | Hardware: time, guest CPU | Software: time, guest CPU |
|---|---|---|
| H.264 1080p, 12 Mbit/s, 600 pictures | 1.2 s, 0.15 s | 1.3 to 1.9 s, 4.5 to 5.5 s |
| H.264 4K, 30 Mbit/s, 300 pictures | 2.2 s, 0.1 s | 3.4 s, 11.4 s |
| VP9 1080p, 10 Mbit/s, 600 pictures | 2.0 s, 0.25 s | 3.9 s, 6.6 s |
| AV1 1080p, 4 Mbit/s, 900 pictures | 1.0 s, 0.2 s | 1.9 s, 3.2 s |

140 to 900 pictures/s.

Firefox, 1080p files, its decoding process, CPUs:

| | software | hardware |
|---|---|---|
| H.264 12 Mbit/s | 0.24 | 0.02 |
| VP9 10 Mbit/s | 0.35 | 0.02 |
| HEVC 8 Mbit/s | 0.52 | 0.02 |

Bit-identical pictures (FFmpeg VA-API decode vs FFmpeg's own decoders): H.264 with B-frames, slices, scaling matrices, weighted prediction; HEVC 8/10-bit with B-frame pyramids, slices, open GOPs; VP9 8/10-bit with hidden frames.

Chrome 155, 1080p30, CPUs (1.0 = one guest CPU busy), before/after Chrome could use the GPU:

| | Before: GPU process, page, whole guest | Now: GPU process, page, whole guest |
|---|---|---|
| H.264, 12 Mbit/s | 0.04, 0.24, 0.45 | 0.13, 0.02, 0.29 |
| VP9, 22 Mbit/s | 0.04, 0.35, 0.49 | 0.16, 0.02, 0.30 |
| Video call with itself, 720p VP8 (camera) | 0.04, 0.17, 0.43 | 0.04, 0.19, 0.48 |
| The same in VP9 | | 0.09, 0.14, 0.49 |

- Decoding leaves the page's process; the GPU process does the copies into GPU memory.
- A call costs about the same: sending was software either way (H.264 no longer: [Video encoding](#video-encoding)).
- Driver no longer spins waiting for a picture (since 2026-10-18; it polled free bitstream buffers too, nearly always there). Chrome, 1080p30 H.264 12 Mbit/s, Wayland: GPU process 0.13 → 0.06, whole guest 0.35 → 0.28 (same clip, two runs each). Get it: rerun `install-vaapi.sh`.

GNOME on Xorg, mean of two runs, before/after X11 support (2026-10-17):

| | Before: GPU process, page, whole guest | Now: GPU process, page, whole guest |
|---|---|---|
| H.264, 12 Mbit/s | 0.04, 0.28, 0.44 | 0.09, 0.02, 0.28 |
| VP9, 22 Mbit/s | 0.04, 0.49, 0.64 | 0.15, 0.02, 0.34 |
| HEVC, 8 Mbit/s | doesn't play | 0.10, 0.02, 0.28 |

No dropped pictures either way. GPU process share includes BGRA conversion (~0.03 CPU at 1080p30).

### Video encoding

FFmpeg 6.1, 300 NV12 frames at 4 Mbit/s, 4 vCPUs. Time, guest CPU (user + system); PSNR vs source:

| | 1280x720 | 1920x1080 |
|---|---|---|
| `h264_v4l2m2m` | 0.46 s, 0.15 s; 42.0 dB | 0.84 s, 0.64 s; 39.6 dB |
| `hevc_v4l2m2m` | | 0.87 s, 0.66 s |
| `h264_vaapi` | 0.93 s, 0.98 s; 44.8 dB | 1.48 s, 1.77 s; 40.6 dB |
| `hevc_vaapi` | 0.93 s, 1.00 s; 46.1 dB | 1.35 s, 1.50 s (1920x1088) |
| `libx264 -preset veryfast` (software) | 0.77 s, 2.10 s; 46.2 dB | 1.23 s, 3.64 s; 41.0 dB |

- 200 to 650 frames/s.
- V4L2: guest CPU barely works. VA-API: FFmpeg's upload + driver copies ~3 ms guest CPU per frame.
- FFmpeg's V4L2 encoders: key frame every 12 frames by default (`-g` changes it): hence lower PSNR.
- All decode back cleanly. GStreamer `v4l2h264enc` (720p) and `v4l2h265enc` (1080p, 300 frames) work too.

Chrome 155, call with itself (H.264, 720p camera, ≤2.5 Mbit/s, received via hardware decoder). CPUs: GPU process, page, whole guest; mean of three 30 s runs:

| | OpenH264 (software) | VideoToolbox |
|---|---|---|
| Wayland: the VM's camera (test colour bars, about 170 kbit/s) | 0.06, 0.11, 0.44 | 0.08, 0.07, 0.44 |
| Wayland: a noisy 720p clip as the camera (2.5 Mbit/s) | 0.07, 0.16, 0.47 | 0.10, 0.09, 0.45 |
| Xorg: the VM's camera | 0.06, 0.09, 0.41 | 0.08, 0.06, 0.41 |
| Xorg: the noisy clip | 0.07, 0.15, 0.44 | 0.10, 0.09, 0.42 |

- Encoding leaves the page process (−0.03 to −0.07 CPU); GPU process +0.02 to +0.03 (converts each camera frame to NV12 for the driver). Whole guest: about even at these sizes.
- Noisy clip: OpenH264 sent 960x540 in every run; VideoToolbox kept 1280x720 in 2 runs of 6.
- Per frame (Chrome `totalEncodeTime` / `framesEncoded`): 5–7.5 ms VideoToolbox vs 2–3.5 ms OpenH264; ~5 ms of it is VideoToolbox's low-latency encoder.

## Trade-offs

- virtio-blk 4 KiB random qd32: ~10% below before (guest sends each request to its own vCPU's queue, kicks more often); still ~2× QEMU. NVMe at qd32 unchanged.
- I/O thread pool: fixed 4 looked best from host cache, but from SSD allowed only 4 reads at once: 4 KiB random qd32 = 43k IOPS vs QEMU 121k. Pool now grows to requests in flight (≤32): 136k from SSD. From cache: 4 KiB qd32 251k → 205k IOPS (QEMU 120k); 1 MiB reads 25 → 35 GB/s.
- virtio-blk has one queue per vCPU (≤16): suspend images of VMs with virtio disks from older vroom don't resume ("queue layout mismatch"). Start with `--fresh`.
- Vulkan and Direct3D contexts: a process each ([SECURITY.md](SECURITY.md)). vs one shared process, M3 Max, busy host, a few runs each:

  | | one process | a process per context |
  |---|---|---|
  | glmark2 (Ubuntu 24.04, GL, off-screen) | 488, 508, 498 | 430, 444, 540 |
  | Vulkan compute, 1M invocations | 4.1-4.4 TFLOP/s | 4.2-4.4 TFLOP/s |
  | small Vulkan program, start to end | 0.35 s | 0.40 s |
  | Edge window (Windows 11), first / later | 1.49 / 0.35-0.40 s | 1.42-1.78 / 0.29-0.44 s |
  | Notepad window | 52-82 ms | 57-87 ms |

  - GL unchanged (stays in the GPU process; glmark2 ±10% run to run here). Drawing unchanged.
  - Cost: a new context, ~50 ms (start process, load MoltenVK or DXMT, make a Metal device).
  - Memory: Windows desktop with Edge = 15–20 contexts, +200–400 MB total.

## Looked at, left alone

- **Idle CPU**: ~0.5% of a host core, like QEMU. (QEMU's UEFI runs had no display device; vroom always has ramfb with UEFI.) macOS 15 HVF handles guest WFI itself (our WFI emulation, 1 ms IPI recheck, is never reached): halt polling gained nothing, dropped. Spinning in device workers before sleep: slower on a busy Mac (competes with vCPUs), dropped.
- **MMIO exits**: ~QEMU's (~1.6 µs per guest register read, syscall included).
- **Boot**: dominated by the guest (systemd, cloud-init); both VMMs within a few hundred ms.

## Still slow, and what it would take

| | Why | Fix |
|---|---|---|
| NVMe 4 KiB random qd32 ~100k IOPS | MMIO exit per doorbell + interrupt on the submitting vCPU per command | NVMe doorbell buffer (shadow doorbells, QEMU has it) |
| network | no offloads: 1500-byte frames, guest computes every checksum; smoltcp checksums each received frame | TSO + checksum offload on virtio-net (NAT only: it terminates TCP, big segments fine) |
| VM window, 2D | without 3D (or when something covers the 3D screen) Slint uploads the whole image per frame | partial texture updates: needs a lower-level path than `slint::Image` |
| NAT latency (~60 µs 1-byte TCP RTT via port forward) | thread wakeups: host socket → device worker → vCPU and back | polling; cost more CPU than it saved on a busy Mac |
| UDP | not pushed past the test's 3 Gbit/s | |

## Measuring

`scripts/bench.py`: boots a guest under vroom or QEMU, runs tests over ssh, prints a JSON line per run (appended to `WORKDIR/results.jsonl`). `--table results.jsonl`: medians per tag. Test details: the script's header.

Work dir, once (~3 GB):

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

Boot once with vroom (cloud-init installs packages). Then in the guest replace cloud-init's network config with DHCP on any `e*` interface (cloud-init's matches the first NIC's MAC; QEMU's differs).

```bash
scripts/build.sh --release
scripts/bench.py $W --tag after                      # boot, idle, exits, fio, network
scripts/bench.py $W --vmm qemu --tag qemu
scripts/bench.py $W --bus nvme --fmt qcow2 --tests fio
scripts/bench.py $W --boot uefi --tests boot,idle
scripts/bench.py --table $W/results.jsonl
```

- Data disk: fresh per run, deleted after.
- Network tests need `allow_host_loopback` (script sets it): guest→host traffic goes to `10.0.2.2`, which both NATs map to host loopback.
