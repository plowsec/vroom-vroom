#!/bin/sh
# Release bundle: target/dist/bulwark-<version>-<os>-<arch>.tar.gz with
# the signed VMM and GUI, guest agents and helpers that were built, and the
# docs. Build the optional parts first if you want them in:
#   scripts/build-virglrenderer.sh          (3D; macOS)
#   cargo xwin build --release -p bulwark-agent --target aarch64-pc-windows-msvc
#   cargo build --release -p bulwark-agent --target aarch64-unknown-linux-musl
set -e
cd "$(dirname "$0")/.."
scripts/build.sh --release >/dev/null
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
os=$(uname -s | tr '[:upper:]' '[:lower:]')
name=bulwark-$ver-$os-$(uname -m)
d=target/dist/$name
rm -rf "$d"
mkdir -p "$d"
cp target/release/bulwark target/release/bulwark-gui "$d/"
note=""
if [ -f target/release/libvirglrenderer.1.dylib ]; then
  cp target/release/libvirglrenderer.1.dylib "$d/"
  note="3D (gpu = \"virtio3d\") needs: brew install molten-vk libepoxy"
fi
win=target/aarch64-pc-windows-msvc/release/bulwark-agent.exe
lin=target/aarch64-unknown-linux-musl/release/bulwark-agent
if [ -f "$win" ]; then mkdir -p "$d/guest/windows" && cp "$win" examples/windows/bulwark-setup.cmd examples/windows/autounattend.xml "$d/guest/windows/"; fi
if [ -f "$lin" ]; then mkdir -p "$d/guest/linux" && cp "$lin" "$d/guest/linux/"; fi
cp -R README.md docs examples "$d/"
cat > "$d/INSTALL.txt" <<TXT
bulwark $ver ($os $(uname -m))

Run:      ./bulwark-gui vm.toml        (window)   or   ./bulwark run vm.toml
New VM:   ./bulwark new myvm --os linux --disk 32G --iso installer.iso
UEFI firmware comes from QEMU's package: brew install qemu (only its
EDK2 files are used). vTPM (tpm = true) needs: brew install swtpm
$note
Keep bulwark and bulwark-gui side by side; both are signed with the
hypervisor entitlement (re-sign with scripts/sign.sh if you rebuild).
TXT
tar -C target/dist -czf "target/dist/$name.tar.gz" "$name"
echo "target/dist/$name.tar.gz"
