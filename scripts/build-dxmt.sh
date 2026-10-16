#!/bin/sh
# Build DXMT, the Direct3D 11 on Metal library vroom uses for 3D in
# Windows guests (with the viogpu3d driver in the guest). virglrenderer
# (scripts/build-virglrenderer.sh) loads it from its own directory.
#
# Base: UTM's DXMT fork, pinned by commit, with its submodules.
# License: LGPL 2.1 (COPYING.LIB): shipped as its own dylib, unchanged.
# Needs: git meson ninja llvm@15 (brew install meson ninja llvm@15).
#
# Output: target/dxmt/libdxmt-native.dylib, also copied next to
# target/{debug,release}/vroom.
set -eu
cd "$(dirname "$0")/.."
ROOT=$(pwd)
URL=https://github.com/utmapp/dxmt.git
REV=66e27f3759ff79e41a791ec40e2a70ef6d0c964c
WORK=$ROOT/target/dxmt-build
OUT=$ROOT/target/dxmt
log=$WORK/build.log
LLVM=$(brew --prefix llvm@15)

mkdir -p "$WORK" "$OUT"
: >"$log"
cd "$WORK"
if [ ! -d src/.git ]; then
  git clone -q "$URL" src
fi
{ git -C src fetch -q origin "$REV" && git -C src checkout -q "$REV" &&
  git -C src submodule update -q --init --recursive; } >>"$log" 2>&1 || { tail -20 "$log"; exit 1; }
rm -rf src/build
{ meson setup src/build src --buildtype=release -Dnative_llvm_path="$LLVM" &&
  meson compile -C src/build; } >>"$log" 2>&1 || { tail -20 "$log"; exit 1; }
cp src/build/src/dxmt-native/libdxmt-native.dylib "$OUT/"
for d in debug release; do
  [ -d "$ROOT/target/$d" ] && cp "$OUT/libdxmt-native.dylib" "$ROOT/target/$d/"
done
echo "built: $OUT/libdxmt-native.dylib"
