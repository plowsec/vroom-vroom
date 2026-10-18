#!/bin/sh
# Windows 11 ARM64 install "stick" for vroom (macOS): a raw FAT32 image
# with an MBR (Windows Setup on NVMe can't read 2 KiB-sector ISOs, and a
# GPT image's EFI partition confuses it), holding the ISO's files, the
# virtio-win ARM64 drivers (pinned, scripts/fetch.sh), the unattend file,
# setup script, agent and WinFsp.
#
#   scripts/make-windows-stick.sh <win11-arm64.iso> [out.raw]
#
# Then attach it (readonly = true, format = "raw", bus = "nvme") next to an
# empty NVMe disk and boot: the install is unattended and WIPES DISK 0.
set -e
[ $# -ge 1 ] || { sed -n '2,11p' "$0"; exit 2; }
[ -f "$1" ] || { echo "$1: no such ISO" >&2; exit 1; }
iso=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
out=${2:-winusb.raw}
case "$out" in /*) ;; *) out="$(pwd)/$out" ;; esac
cd "$(dirname "$0")/.."
vw=$(scripts/fetch.sh virtio-win)
winfsp=$(scripts/fetch.sh winfsp)
exe=target/aarch64-pc-windows-msvc/release/vroom-agent.exe
[ -f "$exe" ] || cargo xwin build --release -p vroom-agent --target aarch64-pc-windows-msvc
tmp=$(mktemp -d)
cleanup() {
  for m in "$tmp/iso" "$tmp/vw" "$tmp/usb"; do hdiutil detach -quiet "$m" 2>/dev/null || true; done
  rm -rf "$tmp"
}
trap cleanup EXIT
mkdir "$tmp/iso" "$tmp/vw" "$tmp/usb"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$tmp/iso" "$iso"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$tmp/vw" "$vw"
[ -d "$tmp/vw/viostor/w11/ARM64" ] || { echo "$vw: no ARM64 drivers in it" >&2; exit 1; }
# Room for the ISO's files plus 1 GiB.
mb=$(( $(du -sm "$tmp/iso" | cut -f1) + 1024 ))
hdiutil create -quiet -size ${mb}m -fs "MS-DOS FAT32" -volname WININST -layout MBRSPUD "$tmp/usb.dmg"
hdiutil attach -quiet -nobrowse -mountpoint "$tmp/usb" "$tmp/usb.dmg"
wim="$tmp/iso/sources/install.wim"
if [ -f "$wim" ] && [ "$(stat -f %z "$wim")" -ge 4294967295 ]; then
  # FAT32 can't hold a file of 4 GiB or more: split (Setup reads .swm).
  command -v wimlib-imagex >/dev/null || { echo "install.wim > 4 GiB: brew install wimlib" >&2; exit 1; }
  rsync -a --exclude boot.catalog --exclude sources/install.wim "$tmp/iso/" "$tmp/usb/"
  wimlib-imagex split "$wim" "$tmp/usb/sources/install.swm" 3800
else
  rsync -a --exclude boot.catalog "$tmp/iso/" "$tmp/usb/"
fi
for d in viogpudo NetKVM vioserial vioinput viorng viostor vioscsi viofs Balloon; do
  [ -d "$tmp/vw/$d/w11/ARM64" ] || { echo "virtio-win has no $d/w11/ARM64" >&2; exit 1; }
  mkdir -p "$tmp/usb/$d/w11"
  cp -R "$tmp/vw/$d/w11/ARM64" "$tmp/usb/$d/w11/"
done
cp examples/windows/autounattend.xml examples/windows/vroom-setup.cmd "$exe" "$winfsp" "$tmp/usb/"
find "$tmp/usb" -name "._*" -delete
hdiutil detach -quiet "$tmp/usb"
hdiutil convert -quiet "$tmp/usb.dmg" -format UDTO -o "$tmp/usb"
# Non-zero disk signature (Windows ignores MBR disks without one) and
# the partition marked active.
printf 'VRMK' | dd of="$tmp/usb.cdr" bs=1 seek=440 conv=notrunc 2>/dev/null
printf '\200' | dd of="$tmp/usb.cdr" bs=1 seek=446 conv=notrunc 2>/dev/null
mv "$tmp/usb.cdr" "$out"
echo "built $out"
