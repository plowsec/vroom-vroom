#!/bin/sh
# Download a pinned dependency into target/downloads/ (once) and print its
# path. The SHA-256 is checked every time; a wrong one is deleted.
#   scripts/fetch.sh virtio-win    virtio-win ISO: Windows drivers for
#                                  vroom's devices (Red Hat, GPLv2/BSD)
#   scripts/fetch.sh winfsp        WinFsp MSI: what virtiofs.exe mounts
#                                  shared folders on (GPLv3 + FLOSS exception)
#   scripts/fetch.sh aavmf         Debian's EDK2 for arm64 VMs: Secure Boot
#                                  firmware, Microsoft keys (BSD-2-Clause-Patent)
#   scripts/fetch.sh ms-*-2023     Microsoft's 2023 Secure Boot certificates
set -eu
cd "$(dirname "$0")/.."

case "${1:-}" in
  virtio-win)
    url=https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/archive-virtio/virtio-win-0.1.302-1/virtio-win-0.1.302.iso
    sha=303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d
    ;;
  winfsp)
    # Also in examples/windows/vroom-setup.cmd (WINFSP_MSI, WINFSP_SHA256).
    url=https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi
    sha=073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a
    ;;
  aavmf)
    url=https://deb.debian.org/debian/pool/main/e/edk2/qemu-efi-aarch64_2025.02-8+deb13u1_all.deb
    sha=a00b2411a79c8aeafd95a7c868ac3cd1aab592f1af9fff965f02d81a625276ed
    ;;
  # Microsoft's 2023 Secure Boot CAs (scripts/make-firmware.sh enrolls them).
  ms-windows-ca-2023)
    url="https://raw.githubusercontent.com/microsoft/secureboot_objects/3d762aecbaecd12402efd21ba89c8c7bd40da4bd/PreSignedObjects/DB/Certificates/windows%20uefi%20ca%202023.der"
    sha=076f1fea90ac29155ebf77c17682f75f1fdd1be196da302dc8461e350a9ae330
    ;;
  ms-uefi-ca-2023)
    url="https://raw.githubusercontent.com/microsoft/secureboot_objects/3d762aecbaecd12402efd21ba89c8c7bd40da4bd/PreSignedObjects/DB/Certificates/microsoft%20uefi%20ca%202023.der"
    sha=f6124e34125bee3fe6d79a574eaa7b91c0e7bd9d929c1a321178efd611dad901
    ;;
  ms-option-rom-ca-2023)
    url="https://raw.githubusercontent.com/microsoft/secureboot_objects/3d762aecbaecd12402efd21ba89c8c7bd40da4bd/PreSignedObjects/DB/Certificates/microsoft%20option%20rom%20uefi%20ca%202023.der"
    sha=e5be3e64c6e66a281457ecdece0d6d0787577aad2a3a0144262c10c14ba8d8f1
    ;;
  ms-kek-ca-2023)
    url="https://raw.githubusercontent.com/microsoft/secureboot_objects/3d762aecbaecd12402efd21ba89c8c7bd40da4bd/PreSignedObjects/KEK/Certificates/microsoft%20corporation%20kek%202k%20ca%202023.der"
    sha=3cd3f0309edae228767a976dd40d9f4affc4fbd5218f2e8cc3c9dd97e8ac6f9d
    ;;
  *)
    echo "usage: scripts/fetch.sh virtio-win|winfsp|aavmf|ms-windows-ca-2023|ms-uefi-ca-2023|ms-option-rom-ca-2023|ms-kek-ca-2023" >&2
    exit 2
    ;;
esac

dir=target/downloads
f=$dir/$(basename "$url")
mkdir -p "$dir"
check() { [ "$(shasum -a 256 "$1" | cut -d' ' -f1)" = "$sha" ]; }
if [ -f "$f" ] && ! check "$f"; then
  rm -f "$f"
fi
if [ ! -f "$f" ]; then
  echo "fetching $url" >&2
  curl -fL --retry 5 --retry-delay 5 -o "$f.part" "$url"
  check "$f.part" || { rm -f "$f.part"; echo "$url: wrong SHA-256" >&2; exit 1; }
  mv "$f.part" "$f"
fi
echo "$f"
