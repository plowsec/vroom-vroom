#!/bin/sh
# The guest tools disk for Windows guests: the image scripts/make-guest-tools.sh
# builds (vroom-agent.exe, vroom-setup.cmd and, with the virtio-win ISO,
# its ARM64 drivers at the top; the Linux tools in linux/), copied to
# out.raw. FAT32 with an MBR: Windows mounts it as D: or so, also on NVMe.
# A WinFsp MSI goes on it if WINFSP_MSI names one (else setup downloads it).
# Attach it read-only, then run D:\vroom-setup.cmd (it asks for
# administrator rights). The app can also put it in as a USB stick.
#   scripts/make-windows-tools.sh [out.raw [virtio-win.iso]]     (macOS)
set -e
cd "$(dirname "$0")/.."
out=${1:-windows-tools.raw}
scripts/make-guest-tools.sh "$2"
cp target/guest-tools/vroom-tools.img "$out"
echo "built $out"
