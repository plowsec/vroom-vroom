#!/bin/sh
# Build the guest tools into target/guest-tools (scripts/app.sh puts them
# in vroom.app; the app and `vroom new` find them there):
#   linux/           vroom-agent (static, any distribution), install-agent.sh,
#                    its systemd units, the session autostart entry, the
#                    GNOME Shell extension
#   windows/         vroom-agent.exe, vroom-setup.cmd, the OpenGL driver if
#                    scripts/build-windows-gl.sh made it, and with the
#                    virtio-win ISO its ARM64 drivers
#   vroom-tools.img  all of it on a FAT32 disk (MBR) labelled VRTOOLS, for
#                    guests without an agent: the Windows files at the top
#                    (D:\vroom-setup.cmd, as before), linux/ in a folder
#
#   scripts/make-guest-tools.sh [virtio-win.iso]        (macOS)
#
# The agent's version comes from git (crates/agent/build.rs); the app
# offers an update to guests whose agent is older.
set -e
cd "$(dirname "$0")/.."
vw=${1:-$VIRTIO_WIN_ISO}
out=target/guest-tools
lin=target/aarch64-unknown-linux-musl/release/vroom-agent
win=target/aarch64-pc-windows-msvc/release/vroom-agent.exe

cargo build --release -p vroom-agent --target aarch64-unknown-linux-musl
if command -v cargo-xwin >/dev/null 2>&1; then
  cargo xwin build --release -p vroom-agent --target aarch64-pc-windows-msvc
else
  echo "no cargo-xwin: Windows tools left out (cargo install cargo-xwin)" >&2
  win=
fi

rm -rf "$out/linux" "$out/windows" "$out/vroom-tools.img"
mkdir -p "$out/linux/gnome-shell"
cp "$lin" "$out/linux/"
for f in install-agent.sh vroom-agent.service vroom-agent-update.path vroom-agent-update.service vroom-agent-session.desktop; do
  cp "examples/linux/$f" "$out/linux/"
done
cp examples/linux/gnome-shell/* "$out/linux/gnome-shell/"

tmp=$(mktemp -d)
devs=
# Attach an image; print its device (/dev/diskN) and remember it, to
# detach it even when its mount point went first.
attach() {
  d=$(hdiutil attach -nobrowse "$@" | awk '/^\/dev\/disk/ {print $1; exit}')
  [ -n "$d" ] || { echo "hdiutil attach $*: no device" >&2; exit 1; }
  echo "$d"
}
cleanup() {
  for d in $devs; do hdiutil detach -quiet -force "$d" 2>/dev/null || { sleep 2; hdiutil detach -quiet -force "$d" 2>/dev/null; } || true; done
  rm -rf "$tmp"
}
trap cleanup EXIT

if [ -n "$win" ]; then
  mkdir -p "$out/windows"
  cp "$win" examples/windows/vroom-setup.cmd "$out/windows/"
  if [ -f target/windows-gl/vroom_gl.dll ]; then
    cp target/windows-gl/vroom_gl.dll target/windows-gl/vroom_gl_arm64.dll target/windows-gl/vroom_gl_x64.dll "$out/windows/"
  fi
  if [ -n "$vw" ]; then
    mkdir "$tmp/vw"
    vwdev=$(attach -readonly -mountpoint "$tmp/vw" "$vw")
    devs="$vwdev $devs"
    # What vroom's devices need: display, network, serial (the agent's
    # port), input, RNG, disks (virtio-blk, virtio-scsi), shared folders,
    # memory balloon.
    for d in viogpudo NetKVM vioserial vioinput viorng viostor vioscsi viofs Balloon; do
      if [ -d "$tmp/vw/$d/w11/ARM64" ]; then
        mkdir -p "$out/windows/$d/w11/ARM64"
        find "$tmp/vw/$d/w11/ARM64" -type f ! -name '*.pdb' -exec cp {} "$out/windows/$d/w11/ARM64/" \;
      fi
    done
  else
    echo "no virtio-win ISO given: the Windows tools have no drivers" >&2
  fi
  if [ -n "$WINFSP_MSI" ]; then cp "$WINFSP_MSI" "$out/windows/"; fi
fi

# The disk image (macOS: hdiutil).
if [ "$(uname)" = Darwin ]; then
  mb=$(( $(du -sm "$out/linux" | cut -f1) + $( [ -d "$out/windows" ] && du -sm "$out/windows" | cut -f1 || echo 0) + 32 ))
  hdiutil create -quiet -size ${mb}m -layout MBRSPUD -fs "MS-DOS FAT32" -volname VRTOOLS "$tmp/t.dmg"
  hdiutil convert -quiet "$tmp/t.dmg" -format UDTO -o "$tmp/t"
  # Windows ignores MBR disks whose signature is zero.
  printf 'VRT1' | dd of="$tmp/t.cdr" bs=1 seek=440 conv=notrunc 2>/dev/null
  mkdir "$tmp/mnt"
  tdev=$(attach -imagekey diskimage-class=CRawDiskImage "$tmp/t.cdr" -mountpoint "$tmp/mnt")
  devs="$tdev $devs"
  if [ -d "$out/windows" ]; then
    cp -R "$out/windows/" "$tmp/mnt/"
    printf '[autorun]\r\nshellexecute=vroom-setup.cmd\r\nicon=vroom-agent.exe\r\nlabel=vroom tools\r\n' > "$tmp/mnt/autorun.inf"
  fi
  cp -R "$out/linux" "$tmp/mnt/linux"
  printf 'vroom guest tools\r\n\r\nWindows: run vroom-setup.cmd (it asks for administrator rights).\r\nLinux: sudo sh linux/install-agent.sh\r\n' > "$tmp/mnt/README.txt"
  # macOS leaves ._ files next to copies; pnputil chokes on ._x.inf.
  find "$tmp/mnt" -name "._*" -delete
  sync
  hdiutil detach -quiet "$tdev" || { sleep 3; hdiutil detach -quiet -force "$tdev"; }
  mv "$tmp/t.cdr" "$out/vroom-tools.img"
fi
ver=$(strings "$lin" | sed -n 's/^VROOM_AGENT_VERSION=//p' | head -1)
echo "built $out (agent $ver)"
