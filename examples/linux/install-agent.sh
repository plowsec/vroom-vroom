#!/bin/sh
# Install the bulwark guest agent in a Linux guest (run as root, from the
# directory holding bulwark-agent and these files).
set -e
install -m 0755 bulwark-agent /usr/local/bin/bulwark-agent
install -m 0644 bulwark-agent.service /etc/systemd/system/bulwark-agent.service
install -d /etc/xdg/autostart
install -m 0644 bulwark-agent-session.desktop /etc/xdg/autostart/bulwark-agent-session.desktop
systemctl daemon-reload
systemctl enable --now bulwark-agent
echo "agent service running; the session helper starts at the next desktop login"
