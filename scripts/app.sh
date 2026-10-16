#!/bin/sh
# Build vroom.app (macOS): the app you start from the Dock, Finder or
# Spotlight. Output: target/dist/vroom.app.
#
# Usage: scripts/app.sh [--install] [--no-build]
#   --install    also copy it to /Applications (replaces an older copy)
#   --no-build   use the binaries already in target/release
#
# Inside the bundle:
#   Contents/MacOS/vroom-gui     the app
#   Contents/MacOS/vroom         the VMM (the app starts it from here)
#   Contents/MacOS/vroom-netd    the root helper the app installs when asked
#   Contents/Frameworks/*.dylib  3D libraries, if built (see dist.sh)
#   Contents/Resources/vroom.icns
#
# Signing is ad hoc: fine on the Mac that built it. To give it to others
# you need a Developer ID, the hardened runtime and notarization; see
# docs/GUIDE.md ("The Mac app").
set -e
cd "$(dirname "$0")/.."
[ "$(uname)" = Darwin ] || { echo "vroom.app is for macOS" >&2; exit 1; }

install=0
build=1
for a in "$@"; do
  case "$a" in
    --install) install=1 ;;
    --no-build) build=0 ;;
    *) echo "usage: scripts/app.sh [--install] [--no-build]" >&2; exit 2 ;;
  esac
done

[ $build = 1 ] && scripts/build.sh --release >/dev/null
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

# 3D: virglrenderer and what it loads from its own directory (MoltenVK,
# DXMT, ANGLE). The GPU process looks in Contents/Frameworks.
libs=""
for l in libvirglrenderer.1 libMoltenVK libdxmt-native libEGL libGLESv2; do
  if [ -f "$r/$l.dylib" ]; then
    mkdir -p "$app/Contents/Frameworks"
    cp "$r/$l.dylib" "$app/Contents/Frameworks/"
    chmod u+w "$app/Contents/Frameworks/$l.dylib"
    libs="$libs $l"
  fi
done
if [ -f target/dxmt-build/src/COPYING.LIB ]; then
  mkdir -p "$app/Contents/Resources/licenses"
  cp target/dxmt-build/src/COPYING.LIB "$app/Contents/Resources/licenses/dxmt-COPYING.LIB"
fi

# Inside out: libraries, the VMM (with the hypervisor entitlement), then
# the bundle, which signs the app's own binary and seals the rest.
sign() {
  out=$(codesign -f -s - "$@" 2>&1) || { echo "$out" >&2; exit 1; }
}
if [ -d "$app/Contents/Frameworks" ]; then
  sign "$app"/Contents/Frameworks/*.dylib
fi
sign -i org.vroom.app.vmm --entitlements scripts/entitlements.plist "$app/Contents/MacOS/vroom"
sign -i org.vroom.app.netd "$app/Contents/MacOS/vroom-netd"
sign "$app"
codesign --verify --strict "$app"
echo "built: $app ($ver)${libs:+, 3D:$libs}"

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
