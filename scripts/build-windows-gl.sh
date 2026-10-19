#!/bin/sh
# Build vroom's OpenGL and Vulkan drivers for Windows guests ("ICDs"), both
# on the viogpu3d kernel driver:
# - Vulkan: Mesa's Venus, which runs the guest's Vulkan in virglrenderer's
#   Venus on the Mac (MoltenVK), as for Linux guests;
# - OpenGL: Mesa's OpenGL with Zink on that Vulkan (OpenGL 4.4), or with
#   its virgl driver (vrend on the Mac's OpenGL, 4.1) when there is no
#   Vulkan.
#
# Base: the Mesa fork viogpu3d's user-mode drivers come from
# (osy/virtio-win-mesa), pinned by commit and hash, with our patches in
# third_party/mesa/. Cross-compiled from macOS with clang-cl, the Windows
# SDK and CRT cargo-xwin downloaded (scripts/make-guest-tools.sh runs
# cargo xwin first), and rust-lld.
# Needs: meson ninja bison llvm uv (brew install meson ninja bison llvm uv),
# rustup's stable toolchain; uv makes the venv Mesa's generators run in
# (scripts/pyenv.sh).
#
# Output in target/windows-gl/: vroom_gl_arm64.dll (ARM64 apps),
# vroom_gl_x64.dll (x64 apps, emulated), and vroom_gl.dll, the one Windows
# loads: an ARM64X file with no code of its own that sends each kind of
# process to its build. Vulkan: vroom_vk_arm64.dll and vroom_vk_x64.dll,
# each with its manifest (vroom_vk_*.json) for the Vulkan loader; the
# agent names both in the display device's VulkanDriverName.
set -eu
cd "$(dirname "$0")/.."
ROOT=$(pwd)
. scripts/pyenv.sh
URL=https://codeload.github.com/osy/virtio-win-mesa/tar.gz/bf536bba65454900d782fa7417bc95c1608d7483
SHA=f3863c3720f8e0b74bc648f703803656984d9fcee6615ad0e9f8c01365abc1b1
WORK=$ROOT/target/windows-gl-build
OUT=$ROOT/target/windows-gl
XWIN=${XWIN_CACHE_DIR:-$HOME/Library/Caches/cargo-xwin}/xwin
LLVM=$(brew --prefix llvm)/bin
BISON=$(brew --prefix bison)/bin
RUST=$(rustc +stable --print sysroot)
log=$WORK/build.log

[ -d "$XWIN/sdk/include/um" ] || { echo "no Windows SDK in $XWIN: run scripts/make-guest-tools.sh once"; exit 1; }
LLD=$(find "$RUST/lib/rustlib" -name rust-lld -type f | head -1)
[ -n "$LLD" ] || { echo "no rust-lld in $RUST"; exit 1; }
mkdir -p "$WORK/bin" "$OUT"
: >"$log"

# Tools under the names meson and clang-cl look for.
cat >"$WORK/bin/lld-link" <<EOF
#!/bin/sh
DYLD_FALLBACK_LIBRARY_PATH="$RUST/lib" exec "$LLD" -flavor link "\$@"
EOF
cat >"$WORK/bin/llvm-rc" <<EOF
#!/bin/sh
exec "$LLVM/llvm-rc" /I "$XWIN/sdk/include/um" /I "$XWIN/sdk/include/shared" /I "$XWIN/sdk/include/ucrt" /I "$XWIN/crt/include" "\$@"
EOF
chmod +x "$WORK/bin/lld-link" "$WORK/bin/llvm-rc"
# clang-cl reads a file name that starts with /U as its /U option (meson
# passes absolute paths, and macOS homes are in /Users): name the same
# file through the data volume.
cat >"$WORK/bin/clang-cl" <<EOF
#!/bin/sh
for a; do
  shift
  case \$a in /U*) [ -e "\$a" ] && a=/System/Volumes/Data\$a ;; esac
  set -- "\$@" "\$a"
done
exec "$LLVM/clang-cl" "\$@"
EOF
chmod +x "$WORK/bin/clang-cl"
ln -sf "$LLVM/llvm-lib" "$WORK/bin/llvm-lib"

pyvenv "$ROOT/target/pyenv"
export PATH="$WORK/bin:$ROOT/target/pyenv/bin:$BISON:$PATH"

cd "$WORK"
[ -f src.tar.gz ] && ! echo "$SHA  src.tar.gz" | shasum -a 256 -c - >/dev/null 2>&1 && rm src.tar.gz
[ -f src.tar.gz ] || curl -fsSL -o src.tar.gz "$URL"
echo "$SHA  src.tar.gz" | shasum -a 256 -c - >/dev/null
rm -rf src && mkdir src && tar xzf src.tar.gz -C src --strip-components 1
for p in "$ROOT"/third_party/mesa/*.patch; do
  (cd src && patch -s -p1 < "$p")
done

# One build per architecture. clangcl-compat.h fills in MSVC intrinsics
# clang-cl lacks.
build() { # <meson cpu_family> <clang triple> <xwin lib dir> <gl dll name> <vulkan dll name>
  cat >"cross-$1.ini" <<EOF
[constants]
inc = ['/imsvc$XWIN/crt/include', '/imsvc$XWIN/sdk/include/ucrt', '/imsvc$XWIN/sdk/include/um', '/imsvc$XWIN/sdk/include/shared']
flags = ['--target=$2', '-fms-compatibility-version=19.40', '-fuse-ld=lld', '-Wno-unused-command-line-argument', '/FI$ROOT/third_party/mesa/clangcl-compat.h'] + inc
[binaries]
c = ['$WORK/bin/clang-cl'] + flags
cpp = ['$WORK/bin/clang-cl'] + flags
c_ld = '$WORK/bin/lld-link'
cpp_ld = '$WORK/bin/lld-link'
ar = '$WORK/bin/llvm-lib'
windres = '$WORK/bin/llvm-rc'
pkg-config = 'false'
[host_machine]
system = 'windows'
cpu_family = '$1'
cpu = '$1'
endian = 'little'
EOF
  export LIB="$XWIN/crt/lib/$3;$XWIN/sdk/lib/um/$3;$XWIN/sdk/lib/ucrt/$3"
  rm -rf "build-$1"
  { meson setup "build-$1" src --cross-file "cross-$1.ini" --buildtype=release -Db_ndebug=true \
      -Db_vscrt=mt --default-library=static -Dplatforms=windows -Dgallium-drivers=virgl,zink \
      -Dvulkan-drivers=virtio -Dllvm=disabled -Dzlib=disabled -Dzstd=disabled -Dvideo-codecs= \
      -Degl=disabled -Dgles1=disabled -Dgles2=disabled -Dglx=disabled -Dbuild-tests=false \
      -Dgallium-wgl-dll-name="$4" &&
    ninja -C "build-$1" src/gallium/targets/wgl/"$4".dll src/virtio/vulkan/vulkan_virtio.dll; } >>"$log" 2>&1 ||
    { tail -20 "$log"; exit 1; }
  cp "build-$1/src/gallium/targets/wgl/$4.dll" "$OUT/"
  cp "build-$1/src/virtio/vulkan/vulkan_virtio.dll" "$OUT/$5.dll"
}
build aarch64 aarch64-pc-windows-msvc aarch64 vroom_gl_arm64 vroom_vk_arm64
build x86_64 x86_64-pc-windows-msvc x86_64 vroom_gl_x64 vroom_vk_x64

# The ARM64X forwarder for OpenGL: ARM64 processes see exports forwarded
# to the ARM64 build, x64 ones (its ARM64EC view) the x64 build. Windows
# looks for those next to it.
mkdir -p arm64x && cd arm64x
FUNCS="DrvCopyContext DrvCreateContext DrvCreateLayerContext DrvDeleteContext DrvDescribeLayerPlane
  DrvDescribePixelFormat DrvGetLayerPaletteEntries DrvGetProcAddress DrvPresentBuffers DrvRealizeLayerPalette
  DrvReleaseContext DrvSetCallbackProcs DrvSetContext DrvSetLayerPaletteEntries DrvSetPixelFormat
  DrvShareLists DrvSwapBuffers DrvSwapLayerBuffers DrvValidateVersion"
for v in arm64 x64; do
  { echo "LIBRARY vroom_gl"; echo "EXPORTS"; for f in $FUNCS; do echo "  $f=vroom_gl_$v.$f"; done; } >"$v.def"
done
inc="/imsvc$XWIN/crt/include /imsvc$XWIN/sdk/include/ucrt /imsvc$XWIN/sdk/include/um /imsvc$XWIN/sdk/include/shared"
for t in aarch64:arm64 arm64ec:ec; do
  for s in entry loadcfg; do
    # shellcheck disable=SC2086
    clang-cl --target="${t%%:*}-pc-windows-msvc" /c /Zl /O2 /GS- $inc "$ROOT/third_party/mesa/arm64x/$s.c" /Fo"${s}_${t##*:}.obj" >>"$log" 2>&1
  done
done
"$LLVM/clang" --target=arm64ec-pc-windows-msvc -c "$ROOT/third_party/mesa/arm64x/chpe.S" -o chpe_ec.obj
lld-link /dll /entry:vroom_gl_entry /nodefaultlib /machine:arm64x /def:x64.def /defarm64native:arm64.def \
  /out:"$OUT/vroom_gl.dll" entry_arm64.obj entry_ec.obj loadcfg_arm64.obj loadcfg_ec.obj chpe_ec.obj >>"$log" 2>&1 ||
  { tail -20 "$log"; exit 1; }
rm -f "$OUT/vroom_gl.lib"
# Vulkan gets one manifest per build instead: the loader opens a driver
# with LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, which a forwarder's target
# doesn't get, and it skips a DLL of the other architecture. It reads a
# library path with a separator as relative to the manifest.
for v in arm64 x64; do
  printf '%s\n' '{' '    "file_format_version": "1.0.1",' '    "ICD": {' \
    "        \"library_path\": \".\\\\vroom_vk_$v.dll\"," '        "api_version": "1.3.0"' '    }' '}' >"$OUT/vroom_vk_$v.json"
done
ls -l "$OUT"
