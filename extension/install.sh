#!/usr/bin/env bash
# Install the Predictive Battery Charging GNOME Shell extension for the current user.
# Run without sudo: extension/install.sh
# Uninstall: extension/install.sh --uninstall
set -euo pipefail
cd "$(dirname "$0")"
UUID=predictive-battery-charging@pbc
if [ "${1:-}" = --uninstall ]; then
  gnome-extensions disable "$UUID" 2>/dev/null || true
  gnome-extensions uninstall "$UUID" 2>/dev/null || rm -rf "$HOME/.local/share/gnome-shell/extensions/$UUID"
  echo "Removed $UUID."; exit 0
fi
gjs -m tests/status-test.js >/dev/null || { echo "Status tests failed. Not installing." >&2; exit 1; }
# Remove the extension under its old name, if present.
OLD=smart-charging@pbc
if gnome-extensions info "$OLD" >/dev/null 2>&1 || [ -d "$HOME/.local/share/gnome-shell/extensions/$OLD" ]; then
  gnome-extensions disable "$OLD" 2>/dev/null || true
  gnome-extensions uninstall "$OLD" 2>/dev/null || rm -rf "$HOME/.local/share/gnome-shell/extensions/$OLD"
  echo "Removed the old $OLD extension."
fi
out=$(mktemp -d)
gnome-extensions pack --force --extra-source=status.js -o "$out" .
gnome-extensions install --force "$out/$UUID.shell-extension.zip"
rm -rf "$out"
if gnome-extensions info "$UUID" >/dev/null 2>&1; then
  gnome-extensions enable "$UUID"
  echo "Installed and enabled $UUID."
else
  # GNOME Shell on Wayland only discovers new extensions at login.
  echo "Installed $UUID. Log out and back in, then run: gnome-extensions enable $UUID"
fi
