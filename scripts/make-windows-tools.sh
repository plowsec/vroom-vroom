#!/bin/sh
# Build a small "tools" disk for Windows guests: bulwark-agent.exe and
# bulwark-setup.cmd on FAT32 with an MBR (Windows mounts it as D: or so).
# Attach it read-only, then run D:\bulwark-setup.cmd as administrator.
#   scripts/make-windows-tools.sh [out.raw]     (macOS)
set -e
cd "$(dirname "$0")/.."
out=${1:-windows-tools.raw}
exe=target/aarch64-pc-windows-msvc/release/bulwark-agent.exe
[ -f "$exe" ] || cargo xwin build --release -p bulwark-agent --target aarch64-pc-windows-msvc
tmp=$(mktemp -d)
trap 'hdiutil detach -quiet "$tmp/mnt" 2>/dev/null; rm -rf "$tmp"' EXIT
hdiutil create -quiet -size 64m -layout MBRSPUD -fs "MS-DOS FAT32" -volname BWTOOLS "$tmp/t.dmg"
hdiutil convert -quiet "$tmp/t.dmg" -format UDTO -o "$tmp/t"
# Windows ignores MBR disks whose signature is zero.
printf 'BWT1' | dd of="$tmp/t.cdr" bs=1 seek=440 conv=notrunc 2>/dev/null
mkdir "$tmp/mnt"
hdiutil attach -quiet -imagekey diskimage-class=CRawDiskImage "$tmp/t.cdr" -mountpoint "$tmp/mnt"
cp "$exe" examples/windows/bulwark-setup.cmd "$tmp/mnt/"
rm -f "$tmp/mnt/._"*
hdiutil detach -quiet "$tmp/mnt"
mv "$tmp/t.cdr" "$out"
echo "built $out"
