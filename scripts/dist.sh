#!/bin/sh
# Release bundle: target/dist/vroom-<version>-<os>-<arch>.tar.gz with
# the signed VMM and GUI (on macOS as vroom.app), guest agents and helpers
# that were built, and the docs. Build the optional parts first if you want them in:
#   scripts/build-virglrenderer.sh          (3D; macOS)
#   scripts/build-dxmt.sh                   (Direct3D for Windows guests)
#   cargo xwin build --release -p vroom-agent --target aarch64-pc-windows-msvc
#   cargo build --release -p vroom-agent --target aarch64-unknown-linux-musl
set -e
cd "$(dirname "$0")/.."
scripts/build.sh --release >/dev/null
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
os=$(uname -s | tr '[:upper:]' '[:lower:]')
name=vroom-$ver-$os-$(uname -m)
d=target/dist/$name
rm -rf "$d"
mkdir -p "$d"
note=""
if [ "$(uname)" = Darwin ]; then
  # The app, with the VMM and the 3D libraries inside (scripts/app.sh).
  scripts/app.sh --no-build >/dev/null
  ditto target/dist/vroom.app "$d/vroom.app"
  if [ -f target/dxmt-build/src/COPYING.LIB ]; then
    mkdir -p "$d/licenses"
    cp target/dxmt-build/src/COPYING.LIB "$d/licenses/dxmt-COPYING.LIB"
  fi
else
  cp target/release/vroom target/release/vroom-gui "$d/"
  if [ -f target/release/libvirglrenderer.so.1 ]; then
    cp target/release/libvirglrenderer.so.1 "$d/"
    note="3D (gpu = \"virtio3d\"): keep libvirglrenderer.so.1 next to vroom."
  fi
fi
win=target/aarch64-pc-windows-msvc/release/vroom-agent.exe
lin=target/aarch64-unknown-linux-musl/release/vroom-agent
if [ -f "$win" ]; then mkdir -p "$d/guest/windows" && cp "$win" examples/windows/vroom-setup.cmd examples/windows/autounattend.xml "$d/guest/windows/"; fi
if [ -f "$lin" ]; then mkdir -p "$d/guest/linux" && cp "$lin" "$d/guest/linux/"; fi
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
