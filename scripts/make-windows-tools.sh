#!/bin/sh
# The guest tools disk for Windows guests: the image scripts/make-guest-tools.sh
# builds (the Windows tools at the top, the Linux tools in linux/), copied
# to out.raw. FAT32 with an MBR: Windows mounts it as D: or so, also on
# NVMe. Attach it read-only, then run D:\vroom-setup.cmd (it asks for
# administrator rights). The app can also put it in as a USB stick.
#   scripts/make-windows-tools.sh [out.raw]     (macOS)
set -e
out=${1:-windows-tools.raw}
case "$out" in /*) ;; *) out="$(pwd)/$out" ;; esac
cd "$(dirname "$0")/.."
scripts/make-guest-tools.sh
cp target/guest-tools/vroom-tools.img "$out"
echo "built $out"
