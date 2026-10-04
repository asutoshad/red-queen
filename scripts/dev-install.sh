#!/usr/bin/env bash
# Installs the daemon from a local release build for testing, as a real
# hardened systemd service on the system bus. Run as root:
#
#   cargo build --release
#   sudo scripts/dev-install.sh
#
# Remove it again with scripts/dev-uninstall.sh.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="$root/target/release"

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
for f in redqueend redqueen; do
    [ -x "$bin/$f" ] || { echo "missing $bin/$f: run 'cargo build --release' first" >&2; exit 1; }
done

install -Dm755 "$bin/redqueend" /usr/local/bin/redqueend
install -Dm755 "$bin/redqueen" /usr/local/bin/redqueen
sed 's|^ExecStart=.*|ExecStart=/usr/local/bin/redqueend|' \
    "$root/packaging/systemd/redqueend.service" > /etc/systemd/system/redqueend.service
install -Dm644 "$root/packaging/dbus/io.github.asutoshad.RedQueen.Daemon.conf" \
    /etc/dbus-1/system.d/io.github.asutoshad.RedQueen.Daemon.conf

install -Dm644 "$root/packaging/polkit/io.github.asutoshad.RedQueen.policy" \
    /usr/share/polkit-1/actions/io.github.asutoshad.RedQueen.policy

systemctl daemon-reload
systemctl reload dbus
systemctl enable redqueend.service
systemctl restart redqueend.service
echo
systemctl --no-pager --lines=5 status redqueend.service || true
echo
echo "Try: redqueen daemon status && redqueen status"
