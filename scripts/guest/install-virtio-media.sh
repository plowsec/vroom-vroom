#!/bin/sh
# Run as root in a Linux guest (Debian/Ubuntu) to get the virtio-media
# driver, which hardware video decoding (`[display] video_decode = true`)
# needs. It isn't in mainline Linux yet: this builds Google's driver
# (chromeos/virtio-media, pinned and checked) with DKMS, so it is rebuilt
# for every new kernel.
#
#   sudo sh install-virtio-media.sh
#
# Then /dev/videoN is a "Video decoder": `v4l2-ctl --list-devices`.
set -eu

COMMIT=ed3a56fd39a74bd8117a3cc9c5b8ec8803df19cd
SHA256=c58ac2b374082cc8373c369340f931e2c9fda7e9848765be442456ead38383bb
VER=0.0.8+git.ed3a56f
SRC=/usr/src/virtio-media-$VER

if [ "$(id -u)" != 0 ]; then
	echo "run as root (sudo)" >&2
	exit 1
fi

export DEBIAN_FRONTEND=noninteractive
apt-get install -y dkms build-essential curl "linux-headers-$(uname -r)" v4l-utils

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "https://codeload.github.com/chromeos/virtio-media/tar.gz/$COMMIT" -o "$tmp/src.tar.gz"
echo "$SHA256  $tmp/src.tar.gz" | sha256sum -c -
tar -xzf "$tmp/src.tar.gz" -C "$tmp"

dkms remove -m virtio-media -v "$VER" --all 2>/dev/null || true
rm -rf "$SRC"
mkdir -p "$SRC"
cp "$tmp/virtio-media-$COMMIT/driver/"* "$SRC/"
# Kernels before 6.10 have no two-argument strscpy().
sed -i -E 's/strscpy\((cap->[a-z_]+), ([^,)]+)\);/strscpy(\1, \2, sizeof(\1));/' "$SRC/virtio_media_ioctls.c"
# poll(): a decoder whose picture queue isn't running yet is waiting for
# its first "source change" event, not broken. Report an error there
# only if the bitstream queue isn't running either, as the kernel's own
# mem2mem helpers do (GStreamer's v4l2h264dec stops on the error).
sed -i '0,/\t\t\trc |= EPOLLERR;/s//\t\t\trc |= output_queue->streaming ? 0 : EPOLLERR;/' "$SRC/virtio_media_driver.c"
grep -q 'output_queue->streaming ? 0 : EPOLLERR' "$SRC/virtio_media_driver.c"
# QBUF: count the buffer as queued before asking the device, not after.
# The device may hand it back at once (vroom returns bitstream buffers as
# soon as it has copied them), and the event can be handled before the
# ioctl's answer: the count then went below zero, poll() said "no room"
# and slept through the wakeup (GStreamer stalled for good).
perl -0pi -e 's/(\tret = virtio_media_send_buffer_ioctl\(vfh, VIDIOC_QBUF, b\);\n\tif \(ret\) \{\n)(\t\t\/\* Rollback[^\n]*\n\t\tbuffer->buffer.flags = old_flags;\n\t\treturn ret;\n\t\}\n)\n\tqueue->queued_bufs \+= 1;\n/\tmutex_lock(&session->queues_lock);\n\tqueue->queued_bufs += 1;\n\tmutex_unlock(&session->queues_lock);\n$1\t\tmutex_lock(&session->queues_lock);\n\t\tqueue->queued_bufs -= 1;\n\t\tmutex_unlock(&session->queues_lock);\n$2/' "$SRC/virtio_media_ioctls.c"
grep -q 'queued_bufs -= 1' "$SRC/virtio_media_ioctls.c"
cat > "$SRC/dkms.conf" <<EOF
PACKAGE_NAME="virtio-media"
PACKAGE_VERSION="$VER"
BUILT_MODULE_NAME[0]="virtio-media"
DEST_MODULE_LOCATION[0]="/updates/dkms"
MAKE[0]="make -C \${kernel_source_dir} M=\${dkms_tree}/\${PACKAGE_NAME}/\${PACKAGE_VERSION}/build modules"
CLEAN="true"
AUTOINSTALL="yes"
EOF

dkms add -m virtio-media -v "$VER"
dkms install -m virtio-media -v "$VER"
echo virtio-media > /etc/modules-load.d/virtio-media.conf
modprobe virtio-media
echo "virtio-media installed:"
v4l2-ctl --list-devices 2>/dev/null || true
