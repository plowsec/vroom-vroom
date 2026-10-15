#!/bin/sh
# Build the virglrenderer vroom uses for 3D on macOS: Venus (Vulkan on
# MoltenVK) and virgl (OpenGL on ANGLE's Metal backend).
#
# Base: slp's macOS fork (what krunkit ships), pinned by hash, plus our
# patches in third_party/virglrenderer/. It links a static libepoxy 1.5.10
# built with EGL and our patch (third_party/libepoxy/), which loads ANGLE
# from the directory virglrenderer is in. ANGLE itself comes from
# scripts/build-angle.sh; without it you get Venus only.
# Needs: meson ninja pkg-config molten-vk (brew install meson ninja molten-vk).
#
# Output: target/virglrenderer/lib/libvirglrenderer.1.dylib, also copied
# next to target/{debug,release}/vroom where the VMM looks first (with
# ANGLE's dylibs, if built).
set -eu
cd "$(dirname "$0")/.."
ROOT=$(pwd)
URL=https://gitlab.freedesktop.org/slp/virglrenderer/-/archive/0.10.4e-krunkit/virglrenderer-0.10.4e-krunkit.tar.gz
SHA=09d000623fbdb966cb604eb48c962a0815e8142383e6066d6494809335b76dbb
EPOXY_URL=https://github.com/anholt/libepoxy/archive/refs/tags/1.5.10.tar.gz
EPOXY_SHA=a7ced37f4102b745ac86d6a70a9da399cc139ff168ba6b8002b4d8d43c900c15
WORK=$ROOT/target/virglrenderer-build
PREFIX=$ROOT/target/virglrenderer
log=$WORK/build.log

# Homebrew's formula env normally provides these.
MVK=$(brew --prefix molten-vk)
export LIBRARY_PATH="$MVK/lib${LIBRARY_PATH:+:$LIBRARY_PATH}"
export CPATH="$MVK/include${CPATH:+:$CPATH}"

mkdir -p "$WORK"
cd "$WORK"
: >"$log"

# libepoxy: static, no GLX/X11. EGL (for ANGLE) needs ANGLE's headers.
egl=no
if [ -d "$ROOT/target/angle/include/EGL" ]; then
  egl=yes
  export CPATH="$ROOT/target/angle/include:$CPATH"
fi
[ -f epoxy.tar.gz ] || curl -fsSL -o epoxy.tar.gz "$EPOXY_URL"
echo "$EPOXY_SHA  epoxy.tar.gz" | shasum -a 256 -c - >/dev/null
rm -rf epoxy && mkdir epoxy && tar xzf epoxy.tar.gz -C epoxy --strip-components 1
(cd epoxy && for p in "$ROOT"/third_party/libepoxy/*.patch; do patch -s -p1 < "$p"; done)
{ meson setup epoxy/build epoxy --prefix="$WORK/epoxy-prefix" --buildtype=release --default-library=static \
    -Degl=$egl -Dglx=no -Dx11=false -Dtests=false &&
  meson install -C epoxy/build; } >>"$log" 2>&1 || { tail -20 "$log"; exit 1; }
export PKG_CONFIG_PATH="$WORK/epoxy-prefix/lib/pkgconfig"

[ -f src.tar.gz ] || curl -fsSL -o src.tar.gz "$URL"
echo "$SHA  src.tar.gz" | shasum -a 256 -c - >/dev/null
rm -rf src && mkdir src && tar xzf src.tar.gz -C src --strip-components 1
cd src
for p in "$ROOT"/third_party/virglrenderer/*.patch; do
  patch -s -p1 < "$p"
done
{ meson setup build --prefix="$PREFIX" --buildtype=release -Dvenus=true -Drender-server=false -Dtests=false \
    -Dcheck-gl-errors=false &&
  meson compile -C build && meson install -C build; } >>"$log" 2>&1 || { tail -20 "$log"; exit 1; }
for d in debug release; do
  [ -d "$ROOT/target/$d" ] || continue
  cp "$PREFIX/lib/libvirglrenderer.1.dylib" "$ROOT/target/$d/"
  for l in libEGL libGLESv2; do
    [ -f "$ROOT/target/angle/$l.dylib" ] && cp "$ROOT/target/angle/$l.dylib" "$ROOT/target/$d/"
  done
done
echo "built: $PREFIX/lib/libvirglrenderer.1.dylib"
