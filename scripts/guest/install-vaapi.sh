#!/bin/sh
# Run as root in a Linux guest (Debian/Ubuntu) to let browsers and other
# VA-API players decode video on the Mac: builds vroom's VA-API driver
# (vaapi/ next to this script), installs it, and makes it the default
# (LIBVA_DRIVER_NAME=vroom, for sessions that start after this).
#
#   sudo sh install-vaapi.sh
#
# Needs the virtio-media driver first (install-virtio-media.sh) and a VM
# with `video_decode = true`; dma-buf export (Firefox, Chrome) needs `gpu =
# "virtio3d"`. Check with `vainfo` after logging in again.
#
# For Chrome and Chromium it also:
# - names the GPU's render node (--render-node-override). Under X11 they
#   look for the GPU on PCI only, and vroom's isn't on PCI; Wayland tells
#   them. The Chromium snap reads it from CHROMIUM_FLAGS. Google
#   Chrome (deb) reads no flags file: wrappers in /usr/local/bin add it,
#   and a copy of Chrome's menu entry starts them. An apt hook redoes
#   that after every package change (/usr/local/sbin/vroom-chrome-x11).
# - copies the driver to /var/lib/snapd/lib/gl: snaps load drivers only
#   from their own runtime and from there.
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

# The GPU's render node.
node=
for p in /dev/dri/by-path/*virtio*-render /dev/dri/renderD*; do
	if [ -e "$p" ]; then
		node=$(readlink -f "$p")
		break
	fi
done

# Where snaps (Chromium) find the driver.
drivers=$(pkg-config --variable=driverdir libva)
snapgl=
if [ -d /var/lib/snapd ]; then
	snapgl=/var/lib/snapd/lib/gl
	mkdir -p "$snapgl"
	install -m 644 "$drivers/vroom_drv_video.so" "$snapgl/vroom_drv_video.so"
fi

# The default driver for everything that uses VA-API, and the settings
# above. /etc/environment keeps what is already set there.
mkdir -p /etc/environment.d
env_d=/etc/environment.d/90-vroom-vaapi.conf
: > "$env_d"
setenv() {
	if ! grep -q "^$1=" /etc/environment; then
		echo "$1=$2" >> /etc/environment
	fi
	echo "$1=$2" >> "$env_d"
}
setenv LIBVA_DRIVER_NAME vroom
if [ -n "$snapgl" ]; then
	setenv LIBVA_DRIVERS_PATH "$drivers:$snapgl"
fi
if [ -n "$node" ]; then
	setenv CHROMIUM_FLAGS "--render-node-override=$node"
fi

# Google Chrome (deb), now and after every package change, so a Chrome
# installed or updated later is covered.
cat > /usr/local/sbin/vroom-chrome-x11 <<'EOF'
#!/bin/sh
# From vroom's install-vaapi.sh. Google Chrome under X11 decodes video on
# the Mac only if told the GPU's render node. This makes wrappers that
# add --render-node-override (/usr/local/bin/google-chrome{,-stable}) and
# a copy of Chrome's menu entry that starts them
# (/usr/local/share/applications/google-chrome.desktop), or removes them
# when Chrome is gone. apt runs it after every package change. To undo:
# delete this file, /etc/apt/apt.conf.d/90vroom-chrome-x11 and the files
# above.
node=
for p in /dev/dri/by-path/*virtio*-render /dev/dri/renderD*; do
	if [ -e "$p" ]; then
		node=$(readlink -f "$p")
		break
	fi
done
entry=/usr/share/applications/google-chrome.desktop
chrome=
if [ -n "$node" ] && [ -x /usr/bin/google-chrome-stable ] && [ -f "$entry" ]; then
	chrome=1
fi
# Only touch files we made.
ours() { [ ! -e "$1" ] || grep -q vroom-chrome-x11 "$1"; }
for name in google-chrome google-chrome-stable; do
	f=/usr/local/bin/$name
	ours "$f" || continue
	if [ -n "$chrome" ]; then
		printf '#!/bin/sh\n# Made by /usr/local/sbin/vroom-chrome-x11.\nexec /usr/bin/google-chrome-stable --render-node-override=%s "$@"\n' "$node" > "$f"
		chmod 755 "$f"
	else
		rm -f "$f"
	fi
done
f=/usr/local/share/applications/google-chrome.desktop
if ours "$f"; then
	if [ -n "$chrome" ]; then
		mkdir -p /usr/local/share/applications
		{
			echo "# Made by /usr/local/sbin/vroom-chrome-x11 from $entry."
			sed 's#/usr/bin/google-chrome-stable#/usr/local/bin/google-chrome-stable#g' "$entry"
		} > "$f"
	else
		rm -f "$f"
	fi
fi
exit 0
EOF
chmod 755 /usr/local/sbin/vroom-chrome-x11
echo 'DPkg::Post-Invoke { "if [ -x /usr/local/sbin/vroom-chrome-x11 ]; then /usr/local/sbin/vroom-chrome-x11; fi"; };' \
	> /etc/apt/apt.conf.d/90vroom-chrome-x11
/usr/local/sbin/vroom-chrome-x11

echo "vroom VA-API driver installed:"
LIBVA_DRIVER_NAME=vroom vainfo 2>/dev/null | grep -E "Driver version|VAProfile" || true
if [ -z "$node" ]; then
	echo "No 3D GPU (gpu = \"virtio3d\"): Chrome and Chromium will decode video in software."
fi
echo "Log in again (or reboot) for browsers to pick it up."
