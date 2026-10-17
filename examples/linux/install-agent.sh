#!/bin/sh
# Install the vroom guest agent in a Linux guest (run as root, from the
# directory holding vroom-agent and these files).
set -e
# The project used to be called bulwark: remove that agent.
if [ -f /etc/systemd/system/bulwark-agent.service ]; then
  systemctl disable --now bulwark-agent || true
  rm -f /etc/systemd/system/bulwark-agent.service /usr/local/bin/bulwark-agent /etc/xdg/autostart/bulwark-agent-session.desktop
fi
install -m 0755 vroom-agent /usr/local/bin/vroom-agent
install -m 0644 vroom-agent.service /etc/systemd/system/vroom-agent.service
install -d /etc/xdg/autostart
install -m 0644 vroom-agent-session.desktop /etc/xdg/autostart/vroom-agent-session.desktop
# Mesa 25.2's Asahi Vulkan driver crashes every Vulkan app when it probes
# a virtio GPU without its native context (the vroom one): leave it out.
if ls /usr/share/vulkan/icd.d/asahi_icd*.json >/dev/null 2>&1; then
  install -d /etc/environment.d /etc/profile.d
  echo 'VK_LOADER_DRIVERS_DISABLE=*asahi*' > /etc/environment.d/90-vroom-vulkan.conf
  echo 'export VK_LOADER_DRIVERS_DISABLE="*asahi*"' > /etc/profile.d/vroom-vulkan.sh
fi
# GNOME: the extension that hides the top bar in Coherence (GNOME Shell
# finds it at the next login; the agent turns it on and off).
if command -v gnome-shell >/dev/null 2>&1 && [ -d gnome-shell ]; then
  v=$(gnome-shell --version 2>/dev/null | sed -n 's/^GNOME Shell \([0-9]*\).*/\1/p')
  d=/usr/share/gnome-shell/extensions/vroom-coherence@vroom
  install -d $d
  install -m 0644 gnome-shell/metadata.json $d/metadata.json
  if [ -n "$v" ] && [ "$v" -lt 45 ]; then
    install -m 0644 gnome-shell/extension-legacy.js $d/extension.js
  else
    install -m 0644 gnome-shell/extension.js $d/extension.js
  fi
fi
systemctl daemon-reload
systemctl enable --now vroom-agent
echo "agent service running; the session helper starts at the next desktop login"
