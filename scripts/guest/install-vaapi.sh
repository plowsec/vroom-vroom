#!/bin/sh
# Run as root in a Linux guest (Debian/Ubuntu) to let browsers and other
# VA-API players decode video on the Mac: builds vroom's VA-API driver
# (vaapi/ next to this script), installs it, and makes it the default
# (LIBVA_DRIVER_NAME=vroom, for sessions that start after this).
#
#   sudo sh install-vaapi.sh
#
# Needs the virtio-media driver first (install-virtio-media.sh) and a VM
# with `video_decode = true`; dma-buf export (Firefox) needs `gpu =
# "virtio3d"`. Check with `vainfo` after logging in again.
set -eu

if [ "$(id -u)" != 0 ]; then
	echo "run as root (sudo)" >&2
	exit 1
fi
here=$(cd "$(dirname "$0")" && pwd)

export DEBIAN_FRONTEND=noninteractive
apt-get install -y build-essential pkg-config libva-dev libdrm-dev libgbm-dev linux-libc-dev vainfo

make -C "$here/vaapi" clean
make -C "$here/vaapi"
make -C "$here/vaapi" install

# The default driver for everything that uses VA-API.
if ! grep -q '^LIBVA_DRIVER_NAME=' /etc/environment; then
	echo 'LIBVA_DRIVER_NAME=vroom' >> /etc/environment
fi
mkdir -p /etc/environment.d
echo 'LIBVA_DRIVER_NAME=vroom' > /etc/environment.d/90-vroom-vaapi.conf

echo "vroom VA-API driver installed:"
LIBVA_DRIVER_NAME=vroom vainfo 2>/dev/null | grep -E "Driver version|VAProfile" || true
echo "Log in again (or reboot) for browsers to pick it up."
