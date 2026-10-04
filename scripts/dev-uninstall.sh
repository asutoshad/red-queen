#!/usr/bin/env bash
# Removes everything installed by scripts/dev-install.sh. Run as root.
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }

systemctl disable --now redqueend.service 2>/dev/null || true
rm -f /etc/systemd/system/redqueend.service \
      /etc/dbus-1/system.d/io.github.asutoshad.RedQueen.Daemon.conf \
      /usr/local/bin/redqueend /usr/local/bin/redqueen
rm -rf /var/lib/red-queen
systemctl daemon-reload
systemctl reload dbus
echo "removed"
