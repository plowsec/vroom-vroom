#!/bin/sh
# Native Linux guest tools. No mounts/root; requires 7z, mtools, sfdisk.
# VROOM_GUEST_ARCH may select x86_64 or aarch64. Windows uses cargo-xwin.
set -eu
cd "$(dirname "$0")/.."
arch=${VROOM_GUEST_ARCH:-$(uname -m)}
case "$arch" in
  x86_64) drivers=amd64 ;;
  aarch64) drivers=ARM64 ;;
  *) echo "unsupported guest architecture: $arch" >&2; exit 1 ;;
esac
for tool in cargo-xwin 7z mformat mcopy sfdisk; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing $tool (needs cargo-xwin, 7zip, mtools, util-linux)" >&2; exit 1; }
done
lin_target=$arch-unknown-linux-musl
win_target=$arch-pc-windows-msvc
rustup target add "$lin_target" "$win_target"
cargo build --release -p vroom-agent --target "$lin_target"
cargo xwin build --release -p vroom-agent --target "$win_target"
vw=$(scripts/fetch.sh virtio-win)
winfsp=$(scripts/fetch.sh winfsp)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/linux/gnome-shell" "$tmp/windows" "$tmp/iso" "$tmp/tree"
cp "target/$lin_target/release/vroom-agent" "$tmp/linux/"
for f in install-agent.sh vroom-agent.service vroom-agent-update.path vroom-agent-update.service vroom-agent-session.desktop; do
  cp "examples/linux/$f" "$tmp/linux/"
done
cp examples/linux/gnome-shell/* "$tmp/linux/gnome-shell/"
cp "target/$win_target/release/vroom-agent.exe" examples/windows/vroom-setup.cmd "$tmp/windows/"
cp "$winfsp" "$tmp/windows/"
7z x -y "-o$tmp/iso" "$vw" >/dev/null
for d in viogpudo NetKVM vioserial vioinput viorng viostor vioscsi viofs Balloon; do
  src=$tmp/iso/$d/w11/$drivers
  [ -d "$src" ] || { echo "virtio-win has no $d/w11/$drivers" >&2; exit 1; }
  mkdir -p "$tmp/windows/$d/w11/$drivers"
  find "$src" -type f ! -name '*.pdb' -exec cp {} "$tmp/windows/$d/w11/$drivers/" \;
done
# The Windows graphics wrapper build scripts currently target ARM64.
# This bundle uses virtio's signed 2D driver on x64.
cp -R "$tmp/windows/." "$tmp/tree/"
cp -R "$tmp/linux" "$tmp/tree/linux"
printf '[autorun]\r\nshellexecute=vroom-setup.cmd\r\nicon=vroom-agent.exe\r\nlabel=vroom tools\r\n' > "$tmp/tree/autorun.inf"
printf 'Windows: run vroom-setup.cmd.\r\nLinux: sudo sh linux/install-agent.sh\r\n' > "$tmp/tree/README.txt"
mb=$(( $(du -sm "$tmp/tree" | cut -f1) + 256 ))
[ "$mb" -ge 512 ] || mb=512
truncate -s "${mb}M" "$tmp/vroom-tools.img"
printf 'label: dos\nlabel-id: 0x31545256\nstart=2048, type=c, bootable\n' | sfdisk "$tmp/vroom-tools.img" >/dev/null
sectors=$((mb * 2048 - 2048))
image=$tmp/vroom-tools.img@@1048576
mformat -i "$image" -F -T "$sectors" -v VRTOOLS ::
for entry in "$tmp/tree/"*; do mcopy -s -i "$image" "$entry" ::; done
out=target/guest-tools
mkdir -p "$out"
# Replace only generated outputs, after every build/packaging step passed.
rm -rf "$out/linux" "$out/windows"
mv "$tmp/linux" "$tmp/windows" "$tmp/vroom-tools.img" "$out/"
echo "built $out for $arch (Windows graphics: virtio 2D)"
