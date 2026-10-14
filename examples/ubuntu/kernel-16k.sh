#!/bin/sh
# Build a 16 KiB-page kernel for an Ubuntu arm64 guest, inside the guest.
# Needed for 3D (Venus) on macOS hosts: HVF maps guest memory in 16 KiB
# pages, so GPU buffers must be placed at 16 KiB boundaries.
#
# Usage (as root, ~20 GB free in WORK, 8 vCPUs recommended):
#   sudo sh kernel-16k.sh [WORK]
# Then copy /boot/vmlinuz-*-16k and /boot/initrd.img-*-16k out for a
# direct kernel boot, or boot it through grub with UEFI.
set -eu
VER=6.8.12
WORK=${1:-/var/tmp/k16}
mkdir -p "$WORK" && cd "$WORK"
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential bc bison flex libssl-dev \
  libelf-dev dwarves rsync kmod cpio debhelper-compat wget xz-utils zstd >/dev/null
[ -d linux-$VER ] || { wget -q https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$VER.tar.xz; tar xf linux-$VER.tar.xz; }
cd linux-$VER
cp "/boot/config-$(uname -r)" .config
scripts/config --disable ARM64_4K_PAGES --enable ARM64_16K_PAGES \
  --disable DEBUG_INFO_DWARF5 --disable DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT --disable DEBUG_INFO_BTF \
  --enable DEBUG_INFO_NONE --set-str SYSTEM_TRUSTED_KEYS "" --set-str SYSTEM_REVOCATION_KEYS "" \
  --set-str LOCALVERSION "-16k"
make olddefconfig >/dev/null
make -j"$(nproc)" bindeb-pkg >"$WORK/build.log" 2>&1
dpkg -i "$WORK"/linux-image-$VER-16k_*.deb
echo "installed: /boot/vmlinuz-$VER-16k"
