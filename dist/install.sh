#!/usr/bin/env bash
# Install the pbcd daemon system-wide. Build first with: cargo build --release
# Install: sudo dist/install.sh
# Uninstall: sudo dist/install.sh --uninstall (keeps settings and history)
# Uninstall and delete settings and history: sudo dist/install.sh --purge
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(id -u)" = 0 ] || { echo "Run with sudo." >&2; exit 1; }
case "${1:-}" in
  --uninstall|--purge)
    systemctl disable --now pbcd.service 2>/dev/null || true
    # Leave the battery charging normally, with no limit and no pending wake.
    [ -x /usr/local/bin/pbcd ] && /usr/local/bin/pbcd --release || true
    rm -f /usr/lib/systemd/system-sleep/pbcd
    rm -f /etc/systemd/system/pbcd.service /usr/share/dbus-1/system.d/org.pbc.Daemon1.conf \
          /usr/share/polkit-1/actions/org.pbc.daemon1.policy \
          /usr/local/bin/pbcd /usr/local/bin/pbcctl /usr/local/libexec/pbcd-sleep-hook
    rm -rf /run/pbcd
    systemctl daemon-reload
    [ "$1" = --purge ] && rm -rf /var/lib/pbcd /etc/pbcd
    echo "pbcd removed."; exit 0 ;;
  "") ;;
  *) echo "usage: $0 [--uninstall|--purge]" >&2; exit 2 ;;
esac
if dpkg-query -W -f='${Status}' predictive-battery-charging 2>/dev/null | grep -q "ok installed"; then
  echo "The predictive-battery-charging package is installed. Remove it first: sudo apt remove predictive-battery-charging" >&2
  exit 1
fi
for b in pbcd pbcctl pbcd-sleep-hook; do [ -x target/release/$b ] || { echo "Missing target/release/$b. Build first: cargo build --release" >&2; exit 1; }; done
install -m 755 target/release/pbcd target/release/pbcctl /usr/local/bin/
install -d -m 755 /usr/local/libexec
install -m 755 target/release/pbcd-sleep-hook /usr/local/libexec/
# systemd-sleep runs every executable in this directory. The hook is a no-op unless pbcd wrote a plan.
ln -sf /usr/local/libexec/pbcd-sleep-hook /usr/lib/systemd/system-sleep/pbcd
install -m 644 dist/org.pbc.Daemon1.conf /usr/share/dbus-1/system.d/
install -m 644 dist/org.pbc.daemon1.policy /usr/share/polkit-1/actions/
# The unit is written for the .deb's /usr/bin. A source install lives in /usr/local.
sed 's|/usr/bin/pbcd|/usr/local/bin/pbcd|' dist/pbcd.service > /etc/systemd/system/pbcd.service
chmod 644 /etc/systemd/system/pbcd.service
install -d -m 755 /etc/pbcd
[ -f /etc/pbcd/config.toml ] || install -m 644 dist/config.toml /etc/pbcd/
systemctl reload dbus 2>/dev/null || true
systemctl daemon-reload
systemctl enable pbcd.service
# Restart rather than `enable --now`, which would leave an old daemon running after an upgrade.
systemctl restart pbcd.service
sleep 1
systemctl --no-pager status pbcd.service | head -5
busctl call org.pbc.Daemon1 /org/pbc/Daemon1 org.pbc.Daemon1 StatusJson
