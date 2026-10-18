#!/bin/sh
# Install or update the vroom guest agent in a Linux guest, as root:
#   sudo sh install-agent.sh            (from the guest tools disk, or a
#                                        folder holding vroom-agent and
#                                        these files)
#   install-agent.sh --update           (vroom-agent-update.service, with
#                                        an update the host sent)
# Safe to run again: it replaces what is there and restarts the agent.
set -e
cd "$(dirname "$0")"
update=0
[ "$1" = "--update" ] && update=1
if [ "$(id -u)" != 0 ]; then
  echo "run it as root: sudo sh $0" >&2
  exit 1
fi
[ -f vroom-agent ] || { echo "no vroom-agent next to $0" >&2; exit 1; }
# The project used to be called bulwark: remove that agent.
if [ -f /etc/systemd/system/bulwark-agent.service ]; then
  systemctl disable --now bulwark-agent || true
  rm -f /etc/systemd/system/bulwark-agent.service /usr/local/bin/bulwark-agent /etc/xdg/autostart/bulwark-agent-session.desktop
fi
install -d /usr/local/bin
# A new file, not one written over: running helpers keep the old one
# until they start the new one.
install -m 0755 vroom-agent /usr/local/bin/vroom-agent.new
mv -f /usr/local/bin/vroom-agent.new /usr/local/bin/vroom-agent
install -m 0644 vroom-agent.service /etc/systemd/system/vroom-agent.service
if [ -f vroom-agent-update.path ]; then
  install -m 0644 vroom-agent-update.path vroom-agent-update.service /etc/systemd/system/
fi
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
systemctl enable vroom-agent >/dev/null 2>&1
if [ -f /etc/systemd/system/vroom-agent-update.path ]; then
  systemctl enable --now vroom-agent-update.path >/dev/null 2>&1 || true
fi
# Restart, not just start: an agent already running is the old one.
systemctl restart vroom-agent

# The session helper (clipboard, resizing) starts at each desktop login.
# Start it now for whoever is on screen, so it works without logging out:
# as them, with their desktop's environment (their systemd user manager
# has it: GNOME and KDE put it there), else a process of their session's.
start_helper() {
  uid=$(sed -n 's/^ACTIVE_UID=//p' /run/systemd/seats/seat0 2>/dev/null)
  sid=$(sed -n 's/^ACTIVE=//p' /run/systemd/seats/seat0 2>/dev/null)
  [ -n "$uid" ] && [ "$uid" != 0 ] || return 0
  user=$(id -nu "$uid")
  if systemctl --user -M "$user@" show-environment 2>/dev/null | grep -q '^DISPLAY=\|^WAYLAND_DISPLAY='; then
    if systemd-run --user -M "$user@" --collect --quiet /usr/local/bin/vroom-agent --session 2>/dev/null; then
      echo "session helper started for $user"
      return 0
    fi
  fi
  type=$(loginctl show-session "$sid" -p Type --value 2>/dev/null)
  env=
  for p in $(pgrep -u "$uid" -x gnome-shell) $(pgrep -u "$uid" -x plasmashell) $(pgrep -u "$uid" -x xfce4-session) $(pgrep -n -u "$uid") $(pgrep -u "$uid"); do
    env=$(tr '\0' '\n' < /proc/$p/environ 2>/dev/null | grep -E '^(DISPLAY|WAYLAND_DISPLAY|XAUTHORITY|XDG_RUNTIME_DIR|DBUS_SESSION_BUS_ADDRESS|XDG_SESSION_TYPE|XDG_CURRENT_DESKTOP)=' | tr '\n' ' ')
    case "$env" in *DISPLAY=*) ;; *) env=; continue ;; esac
    # Not a leftover of an earlier session of another kind.
    case "$env" in *XDG_SESSION_TYPE=*) case "$env" in *"XDG_SESSION_TYPE=$type "*) break ;; esac ;; *) break ;; esac
    env=
  done
  [ -n "$env" ] || return 0
  # shellcheck disable=SC2086 # one word per variable
  setsid runuser -u "$user" -- env $env /usr/local/bin/vroom-agent --session </dev/null >/dev/null 2>&1 &
  echo "session helper started for $user"
}
# An update: the helpers that run see their file change and restart.
# By hand: helpers of an older agent may not, so they go.
if [ $update = 0 ]; then
  pkill -f '/usr/local/bin/vroom-agent --session' 2>/dev/null || true
  start_helper
fi
echo "vroom guest agent $(/usr/local/bin/vroom-agent --version 2>/dev/null | cut -d' ' -f2) installed"
