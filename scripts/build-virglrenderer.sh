#!/bin/sh
# Build the virglrenderer bulwark uses for 3D (Venus) on macOS.
#
# Base: slp's macOS fork (what krunkit ships), pinned by hash, plus our
# patches in third_party/virglrenderer/. Needs: meson ninja pkg-config
# libepoxy molten-vk (brew install meson ninja libepoxy molten-vk).
#
# Output: target/virglrenderer/lib/libvirglrenderer.1.dylib, also copied
# next to target/{debug,release}/bulwark where the VMM looks first.
set -eu
cd "$(dirname "$0")/.."
ROOT=$(pwd)
URL=https://gitlab.freedesktop.org/slp/virglrenderer/-/archive/0.10.4e-krunkit/virglrenderer-0.10.4e-krunkit.tar.gz
SHA=09d000623fbdb966cb604eb48c962a0815e8142383e6066d6494809335b76dbb
WORK=$ROOT/target/virglrenderer-build
PREFIX=$ROOT/target/virglrenderer

# Homebrew's formula env normally provides these.
MVK=$(brew --prefix molten-vk)
export LIBRARY_PATH="$MVK/lib${LIBRARY_PATH:+:$LIBRARY_PATH}"
export CPATH="$MVK/include${CPATH:+:$CPATH}"

mkdir -p "$WORK"
cd "$WORK"
[ -f src.tar.gz ] || curl -fsSL -o src.tar.gz "$URL"
echo "$SHA  src.tar.gz" | shasum -a 256 -c - >/dev/null
rm -rf src && mkdir src && tar xzf src.tar.gz -C src --strip-components 1
cd src
for p in "$ROOT"/third_party/virglrenderer/*.patch; do
  patch -s -p1 < "$p"
done
log=$WORK/build.log
{ meson setup build --prefix="$PREFIX" --buildtype=release -Dvenus=true -Drender-server=false -Dtests=false &&
  meson compile -C build && meson install -C build; } >"$log" 2>&1 || { tail -20 "$log"; exit 1; }
for d in debug release; do
  [ -d "$ROOT/target/$d" ] && cp "$PREFIX/lib/libvirglrenderer.1.dylib" "$ROOT/target/$d/"
done
echo "built: $PREFIX/lib/libvirglrenderer.1.dylib"
