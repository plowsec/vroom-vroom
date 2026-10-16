#!/bin/sh
# Build a small "tools" disk for Windows guests: vroom-agent.exe and
# vroom-setup.cmd on FAT32 with an MBR (Windows mounts it as D: or so).
# The OpenGL driver goes on it too when scripts/build-windows-gl.sh made it.
# With the virtio-win ISO, its ARM64 drivers go on it too (an installed
# Windows can't read the ISO itself on NVMe). So does a WinFsp MSI if
# WINFSP_MSI names one (else setup downloads it).
# Attach it read-only, then run D:\vroom-setup.cmd as administrator.
#   scripts/make-windows-tools.sh [out.raw [virtio-win.iso]]     (macOS)
set -e
cd "$(dirname "$0")/.."
out=${1:-windows-tools.raw}
vw=$2
exe=target/aarch64-pc-windows-msvc/release/vroom-agent.exe
# Always rebuild: a stale agent on the disk is a confusing bug.
cargo xwin build --release -p vroom-agent --target aarch64-pc-windows-msvc
tmp=$(mktemp -d)
cleanup() {
  for m in "$tmp/mnt" "$tmp/vw"; do hdiutil detach -quiet "$m" 2>/dev/null || true; done
  rm -rf "$tmp"
}
trap cleanup EXIT
hdiutil create -quiet -size 96m -layout MBRSPUD -fs "MS-DOS FAT32" -volname VRTOOLS "$tmp/t.dmg"
hdiutil convert -quiet "$tmp/t.dmg" -format UDTO -o "$tmp/t"
# Windows ignores MBR disks whose signature is zero.
printf 'VRT1' | dd of="$tmp/t.cdr" bs=1 seek=440 conv=notrunc 2>/dev/null
mkdir "$tmp/mnt"
hdiutil attach -quiet -imagekey diskimage-class=CRawDiskImage "$tmp/t.cdr" -mountpoint "$tmp/mnt"
cp "$exe" examples/windows/vroom-setup.cmd "$tmp/mnt/"
if [ -f target/windows-gl/vroom_gl.dll ]; then
  cp target/windows-gl/vroom_gl.dll target/windows-gl/vroom_gl_arm64.dll target/windows-gl/vroom_gl_x64.dll "$tmp/mnt/"
fi
if [ -n "$vw" ]; then
  mkdir "$tmp/vw"
  hdiutil attach -quiet -nobrowse -readonly -mountpoint "$tmp/vw" "$vw"
  for d in viogpudo NetKVM vioserial vioinput viorng viostor viofs; do
    if [ -d "$tmp/vw/$d/w11/ARM64" ]; then
      mkdir -p "$tmp/mnt/$d/w11/ARM64"
      find "$tmp/vw/$d/w11/ARM64" -type f ! -name '*.pdb' -exec cp {} "$tmp/mnt/$d/w11/ARM64/" \;
    fi
  done
fi
if [ -n "$WINFSP_MSI" ]; then cp "$WINFSP_MSI" "$tmp/mnt/"; fi
# macOS leaves ._ files next to copies; pnputil chokes on ._x.inf.
find "$tmp/mnt" -name "._*" -delete
hdiutil detach -quiet "$tmp/mnt"
mv "$tmp/t.cdr" "$out"
echo "built $out"
