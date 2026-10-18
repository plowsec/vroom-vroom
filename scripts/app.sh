#!/bin/sh
# Build vroom.app (macOS): the app you start from the Dock, Finder or
# Spotlight, complete from a fresh clone: the VMM, GUI and root helper,
# 3D on the Mac (ANGLE, DXMT, virglrenderer) and the guest tools. Long
# builds run only when their inputs changed. Output: target/dist/vroom.app.
# Needs: Xcode, rustup, cargo-xwin, and brew install meson ninja bison
# pkg-config llvm llvm@15 molten-vk.
#
# Usage: scripts/app.sh [--install] [--no-build]
#   --install    also copy it to /Applications (replaces an older copy)
#   --no-build   package what target/ has
#
# Inside the bundle:
#   Contents/MacOS/vroom-gui     the app
#   Contents/MacOS/vroom         the VMM (the app starts it from here)
#   Contents/MacOS/vroom-netd    the root helper the app installs when asked
#   Contents/Frameworks/*.dylib  3D libraries, if built (see dist.sh)
#   Contents/Resources/vroom.icns
#   Contents/Resources/guest-tools  the guest agent, its installers, drivers
#
# Signing is ad hoc: fine on the Mac that built it. To give it to others
# you need a Developer ID, the hardened runtime and notarization; see
# docs/GUIDE.md ("The Mac app").
set -e
cd "$(dirname "$0")/.."
[ "$(uname)" = Darwin ] || { echo "vroom.app is for macOS" >&2; exit 1; }
. scripts/stamp.sh

install=0
build=1
for a in "$@"; do
  case "$a" in
    --install) install=1 ;;
    --no-build) build=0 ;;
    *) echo "usage: scripts/app.sh [--install] [--no-build]" >&2; exit 2 ;;
  esac
done

if [ $build = 1 ]; then
  scripts/build.sh --release >/dev/null
  # 3D on the Mac: virglrenderer loads ANGLE and DXMT, so they come first.
  if ! fresh target/angle/.inputs scripts/build-angle.sh; then
    scripts/build-angle.sh
    stamp target/angle/.inputs scripts/build-angle.sh
  fi
  if ! fresh target/dxmt/.inputs scripts/build-dxmt.sh; then
    scripts/build-dxmt.sh
    stamp target/dxmt/.inputs scripts/build-dxmt.sh
  fi
  vrend="scripts/build-virglrenderer.sh third_party/virglrenderer third_party/libepoxy target/angle/.inputs target/dxmt/.inputs"
  if ! fresh target/virglrenderer/.inputs $vrend; then
    scripts/build-virglrenderer.sh
    stamp target/virglrenderer/.inputs $vrend
  fi
  scripts/make-guest-tools.sh
fi
r=target/release
for b in vroom vroom-gui; do
  [ -f "$r/$b" ] || { echo "no $r/$b: run scripts/build.sh --release" >&2; exit 1; }
done

ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app=target/dist/vroom.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$r/vroom-gui" "$r/vroom" "$r/vroom-netd" "$app/Contents/MacOS/"
sed "s/@VERSION@/$ver/g" packaging/macos/Info.plist >"$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"
cp packaging/macos/vroom.icns "$app/Contents/Resources/"
# The guest tools (scripts/make-guest-tools.sh): what the app sends to a
# guest's agent, puts in as a USB stick, and builds ready-made VMs with.
[ -f target/guest-tools/vroom-tools.img ] || { echo "no guest tools: scripts/make-guest-tools.sh" >&2; exit 1; }
cp -R target/guest-tools "$app/Contents/Resources/guest-tools"

# 3D: virglrenderer and what it loads from its own directory (MoltenVK,
# DXMT, ANGLE). The GPU process looks in Contents/Frameworks.
mkdir -p "$app/Contents/Frameworks" "$app/Contents/Resources/licenses"
for f in target/virglrenderer/lib/libvirglrenderer.1.dylib "$(brew --prefix molten-vk)/lib/libMoltenVK.dylib" \
  target/dxmt/libdxmt-native.dylib target/angle/libEGL.dylib target/angle/libGLESv2.dylib; do
  [ -f "$f" ] || { echo "no $f: scripts/app.sh builds it (without --no-build)" >&2; exit 1; }
  cp "$f" "$app/Contents/Frameworks/"
  chmod u+w "$app/Contents/Frameworks/$(basename "$f")"
done
cp target/dxmt-build/src/COPYING.LIB "$app/Contents/Resources/licenses/dxmt-COPYING.LIB"

# Inside out: libraries, the VMM (with the hypervisor entitlement), then
# the bundle, which signs the app's own binary and seals the rest.
sign() {
  out=$(codesign -f -s - "$@" 2>&1) || { echo "$out" >&2; exit 1; }
}
sign "$app"/Contents/Frameworks/*.dylib
sign -i org.vroom.app.vmm --entitlements scripts/entitlements.plist "$app/Contents/MacOS/vroom"
sign -i org.vroom.app.netd "$app/Contents/MacOS/vroom-netd"
sign "$app"
codesign --verify --strict "$app"
echo "built: $app ($ver)"

if [ $install = 1 ]; then
  dest=/Applications/vroom.app
  # macOS kills a VM whose binary is replaced under it.
  if pgrep -f "^$dest/Contents/MacOS/vroom " >/dev/null 2>&1; then
    echo "VMs are running from $dest: suspend or stop them first" >&2
    exit 1
  fi
  rm -rf "$dest"
  ditto "$app" "$dest"
  # Tell Finder about the new copy (icon, Open With).
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$dest" 2>/dev/null || true
  echo "installed: $dest"
fi
