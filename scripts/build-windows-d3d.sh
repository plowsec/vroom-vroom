#!/bin/sh
# Build vroom_d3d, the wrapper around viogpu3d's Direct3D driver that keeps
# apps from crashing when vroom turns the 3D display off for a save
# (scripts/guest/d3d/vroom_d3d.c). Cross-compiled from macOS with clang-cl,
# the Windows SDK and CRT cargo-xwin downloaded (run
# scripts/make-guest-tools.sh once), and rust-lld. A few seconds.
#
# Output in target/windows-gl/: vroom_d3d_arm64.dll (ARM64 apps),
# vroom_d3d_x64.dll (x64 apps, emulated) and vroom_d3d.dll, the ARM64X
# forwarder Windows loads, which sends each kind of process to its build.
set -eu
cd "$(dirname "$0")/.."
ROOT=$(pwd)
OUT=$ROOT/target/windows-gl
WORK=$ROOT/target/windows-d3d-build
XWIN=${XWIN_CACHE_DIR:-$HOME/Library/Caches/cargo-xwin}/xwin
LLVM=$(brew --prefix llvm)/bin
RUST=$(rustc +stable --print sysroot)

[ -d "$XWIN/sdk/include/um" ] || { echo "no Windows SDK in $XWIN: run scripts/make-guest-tools.sh once"; exit 1; }
LLD=$(find "$RUST/lib/rustlib" -name rust-lld -type f | head -1)
[ -n "$LLD" ] || { echo "no rust-lld in $RUST"; exit 1; }
lld() { DYLD_FALLBACK_LIBRARY_PATH="$RUST/lib" "$LLD" -flavor link "$@"; }
mkdir -p "$WORK" "$OUT"
cd "$WORK"
# clang-cl reads a path that starts with /U as its /U option: name the same
# files through the data volume.
data() { case $1 in /Users/*) echo "/System/Volumes/Data$1" ;; *) echo "$1" ;; esac; }
X=$(data "$XWIN")
SRC=$(data "$ROOT/scripts/guest/d3d/vroom_d3d.c")
inc="/imsvc$X/crt/include /imsvc$X/sdk/include/ucrt /imsvc$X/sdk/include/um /imsvc$X/sdk/include/shared"
for v in aarch64:arm64 x86_64:x64; do
  t=${v%%:*} n=${v##*:}
  # shellcheck disable=SC2086
  "$LLVM/clang-cl" --target="$t-pc-windows-msvc" /c /O2 /MT /W4 /WX $inc "$SRC" /Fo"vroom_d3d_$n.obj"
  lld /nologo /dll /out:"$OUT/vroom_d3d_$n.dll" "vroom_d3d_$n.obj" \
    "/libpath:$XWIN/crt/lib/$t" "/libpath:$XWIN/sdk/lib/um/$t" "/libpath:$XWIN/sdk/lib/ucrt/$t" kernel32.lib
  rm -f "$OUT/vroom_d3d_$n.lib" "$OUT/vroom_d3d_$n.exp"
done

# The ARM64X forwarder, as for OpenGL (scripts/build-windows-gl.sh).
for v in arm64 x64; do
  { echo "LIBRARY vroom_d3d"; echo "EXPORTS"; for f in OpenAdapter10_2 OpenAdapter12; do echo "  $f=vroom_d3d_$v.$f"; done; } >"$v.def"
done
for t in aarch64:arm64 arm64ec:ec; do
  for s in entry loadcfg; do
    # shellcheck disable=SC2086
    "$LLVM/clang-cl" --target="${t%%:*}-pc-windows-msvc" /c /Zl /O2 /GS- $inc "$(data "$ROOT/third_party/mesa/arm64x/$s.c")" /Fo"${s}_${t##*:}.obj"
  done
done
"$LLVM/clang" --target=arm64ec-pc-windows-msvc -c "$ROOT/third_party/mesa/arm64x/chpe.S" -o chpe_ec.obj
lld /dll /entry:vroom_gl_entry /nodefaultlib /machine:arm64x /def:x64.def /defarm64native:arm64.def \
  /out:"$OUT/vroom_d3d.dll" entry_arm64.obj entry_ec.obj loadcfg_arm64.obj loadcfg_ec.obj chpe_ec.obj
rm -f "$OUT/vroom_d3d.lib" "$OUT/vroom_d3d.exp"
ls -l "$OUT"/vroom_d3d*
