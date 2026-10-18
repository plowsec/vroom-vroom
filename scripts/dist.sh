#!/bin/sh
# Release bundle: target/dist/vroom-<version>-<os>-<arch>.tar.gz with the
# signed VMM and GUI (on macOS: vroom.app, built complete by scripts/app.sh,
# 3D and guest tools included), the guest tools, and the docs.
set -e
cd "$(dirname "$0")/.."
if [ "$(uname)" = Darwin ]; then
  scripts/app.sh >/dev/null
else
  scripts/build.sh --release >/dev/null
fi
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
os=$(uname -s | tr '[:upper:]' '[:lower:]')
name=vroom-$ver-$os-$(uname -m)
d=target/dist/$name
rm -rf "$d"
mkdir -p "$d"
note=""
if [ "$(uname)" = Darwin ]; then
  ditto target/dist/vroom.app "$d/vroom.app"
  mkdir -p "$d/licenses"
  cp target/dxmt-build/src/COPYING.LIB "$d/licenses/dxmt-COPYING.LIB"
  ditto target/guest-tools "$d/guest"
else
  cp target/release/vroom target/release/vroom-gui target/release/vroom-netd "$d/"
  if [ -f target/release/libvirglrenderer.so.1 ]; then
    cp target/release/libvirglrenderer.so.1 "$d/"
    note="3D (gpu = \"virtio3d\"): keep libvirglrenderer.so.1 next to vroom."
  fi
fi
cp -R README.md docs examples "$d/"
if [ "$(uname)" = Darwin ]; then
  run='Install:  drag vroom.app to Applications, then open it.
Terminal: vroom.app/Contents/MacOS/vroom (the VMM and its commands),
          vroom.app/Contents/MacOS/vroom-gui vm.toml (one VM'"'"'s window)
New VM:   .../vroom new myvm --os linux --disk 32G --iso installer.iso
It is signed ad hoc, not by a known developer: macOS blocks it the
first time. Allow it in System Settings, Privacy & Security ("Open
Anyway"), or run: xattr -dr com.apple.quarantine vroom.app'
else
  run='Run:      ./vroom-gui vm.toml        (window)   or   ./vroom run vm.toml
New VM:   ./vroom new myvm --os linux --disk 32G --iso installer.iso'
fi
cat > "$d/INSTALL.txt" <<TXT
vroom $ver ($os $(uname -m))

$run
UEFI firmware comes from QEMU's package: brew install qemu (only its
EDK2 files are used). vTPM (tpm = true) needs: brew install swtpm
$note
TXT
tar -C target/dist -czf "target/dist/$name.tar.gz" "$name"
echo "target/dist/$name.tar.gz"
