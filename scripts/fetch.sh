#!/bin/sh
# Download a pinned dependency into target/downloads/ (once) and print its
# path. The SHA-256 is checked every time; a wrong one is deleted.
#   scripts/fetch.sh virtio-win    virtio-win ISO: Windows drivers for
#                                  vroom's devices (Red Hat, GPLv2/BSD)
#   scripts/fetch.sh winfsp        WinFsp MSI: what virtiofs.exe mounts
#                                  shared folders on (GPLv3 + FLOSS exception)
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
  *)
    echo "usage: scripts/fetch.sh virtio-win|winfsp" >&2
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
