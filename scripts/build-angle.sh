#!/bin/sh
# Build ANGLE (libEGL + libGLESv2, Metal backend only) for OpenGL in
# Linux guests: virglrenderer replays the guest's GL on it (see
# scripts/build-virglrenderer.sh and docs/GUIDE.md, "3D acceleration").
#
# Source: chromium.googlesource.com/angle/angle at the commit Chrome's
# stable release ships, fetched shallow with depot_tools. Tests, traces,
# other backends and their dependencies are left out.
# Needs: Xcode with the Metal toolchain (Xcode 26 downloads it separately:
# xcodebuild -downloadComponent MetalToolchain), python3, ninja. The
# checkout takes about 3 GB in target/angle-build while building (removed
# afterwards; KEEP=1 keeps it). Output: 6 MB. Takes about 10 minutes,
# mostly downloading.
#
# Output: target/angle/lib{EGL,GLESv2}.dylib, also copied next to
# target/{debug,release}/vroom where the VMM looks for them.
set -eu
cd "$(dirname "$0")/.."
ROOT=$(pwd)
# ANGLE's Metal backend compiles its internal shaders with `metal`, which
# Xcode 26 no longer installs with itself. Say so now: the failure comes
# 10 minutes in, after the checkout.
xcrun metal --version >/dev/null 2>&1 ||
  { echo "no Metal compiler: xcodebuild -downloadComponent MetalToolchain" >&2; exit 1; }
# Chrome 156 (stable, October 2026).
REV=3b89d84e9d59f75675ac4e2bef58fc068a9f0577
WORK=$ROOT/target/angle-build
OUT=$ROOT/target/angle

mkdir -p "$WORK" "$OUT"
cd "$WORK"
[ -d depot_tools ] || git clone -q --depth 1 https://chromium.googlesource.com/chromium/tools/depot_tools.git
# Last in PATH: its ninja wrapper needs a bootstrap we skip; use the system ninja.
export PATH="$PATH:$WORK/depot_tools" DEPOT_TOOLS_UPDATE=0 DEPOT_TOOLS_METRICS=0

# Only what libEGL/libGLESv2 need. None = don't fetch.
skip='VK-GL-CTS/src SwiftShader catapult dawn glmark2/src rapidjson/src perfetto
  vulkan-validation-layers/src lunarg-vulkantools/src vulkan-tools/src cherry OpenCL-Docs/src
  libjpeg_turbo libpng/src nasm clang-format/script protobuf llvm/src clspv/src OpenCL-CTS/src
  OpenCL-ICD-Loader/src'
tools='rust perf md_browser memory skia_goldctl/mac_amd64 skia_goldctl/mac_arm64'
{
  echo 'solutions = [{ "name": "angle", "url": "https://chromium.googlesource.com/angle/angle.git",'
  echo '  "deps_file": "DEPS", "managed": False, "custom_vars": {"checkout_angle_cl_deps": False},'
  echo '  "custom_deps": {'
  for d in $skip; do echo "    \"angle/third_party/$d\": None,"; done
  for d in $tools; do echo "    \"angle/tools/$d\": None,"; done
  echo '    "angle/buildtools/reclient": None,'
  echo '  },'
  echo '}]'
} > .gclient
# Hooks are skipped: the toolchain (clang, gn) comes as plain deps; the
# one hook we need (LASTCHANGE) runs below.
gclient sync --nohooks --no-history --shallow -j8 -r "angle@$REV" >sync.log 2>&1 || { tail -20 sync.log; exit 1; }
cd angle
python3 build/util/lastchange.py -o build/util/LASTCHANGE

mkdir -p out/Release
cat > out/Release/args.gn <<EOF
is_debug = false
is_component_build = false
target_cpu = "arm64"
angle_enable_metal = true
angle_enable_vulkan = false
angle_enable_gl = false
angle_enable_null = false
angle_enable_swiftshader = false
angle_enable_wgpu = false
angle_enable_cl = false
angle_build_tests = false
build_angle_deqp_tests = false
angle_has_rapidjson = false
enable_rust = false
angle_enable_perfetto = false
treat_warnings_as_errors = false
symbol_level = 0
use_siso = false
EOF
log=$WORK/build.log
{ buildtools/mac/gn gen out/Release && ninja -C out/Release libEGL libGLESv2; } >"$log" 2>&1 || { tail -30 "$log"; exit 1; }
# EGL headers: libepoxy needs them to build its EGL side.
rm -rf "$OUT/include" && mkdir -p "$OUT/include"
cp -R include/EGL include/KHR "$OUT/include/"
for l in libEGL libGLESv2; do
  cp "out/Release/$l.dylib" "$OUT/"
  for d in debug release; do
    [ -d "$ROOT/target/$d" ] && cp "$OUT/$l.dylib" "$ROOT/target/$d/"
  done
done
[ "${KEEP:-0}" = 1 ] || rm -rf "$WORK"
echo "built: $OUT/libEGL.dylib $OUT/libGLESv2.dylib"
