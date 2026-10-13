#!/usr/bin/env bash
# Build the .deb and the GNOME extension zip into dist-out/.
# Run from anywhere: packaging/build-deb.sh
# Needs: cargo, dpkg-deb, gnome-extensions (from gnome-shell) or zip.
# VERSION defaults to the workspace version in Cargo.toml. CI sets it from the tag.
set -euo pipefail
cd "$(dirname "$0")/.."
# Package directories must be 755 whatever the caller's umask.
umask 022

NAME=predictive-battery-charging
UUID=predictive-battery-charging@pbc
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
ARCH=$(dpkg --print-architecture)
OUT=dist-out
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

# A packaged build reports its plain version, without the git commit.
PBC_RELEASE=1 cargo build --release --locked
mkdir -p "$OUT"

# The extension zip, for gnome-extensions install and extensions.gnome.org.
# gnome-extensions pack checks metadata.json, so prefer it when it's installed.
rm -f "$OUT/$UUID.shell-extension.zip"
if command -v gnome-extensions >/dev/null; then
  gnome-extensions pack --force --extra-source=status.js -o "$OUT" extension
else
  (cd extension && zip -q -r "../$OUT/$UUID.shell-extension.zip" metadata.json extension.js prefs.js status.js)
fi

# Package tree.
R="$STAGE/root"
install -D -m 755 target/release/pbcd "$R/usr/bin/pbcd"
install -D -m 755 target/release/pbcctl "$R/usr/bin/pbcctl"
install -D -m 755 target/release/pbcd-sleep-hook "$R/usr/libexec/pbcd-sleep-hook"
# systemd-sleep runs every executable here. The hook is a no-op unless pbcd wrote a plan.
install -d "$R/usr/lib/systemd/system-sleep"
ln -s /usr/libexec/pbcd-sleep-hook "$R/usr/lib/systemd/system-sleep/pbcd"
install -D -m 644 dist/pbcd.service "$R/usr/lib/systemd/system/pbcd.service"
install -D -m 644 dist/org.pbc.Daemon1.conf "$R/usr/share/dbus-1/system.d/org.pbc.Daemon1.conf"
install -D -m 644 dist/org.pbc.daemon1.policy "$R/usr/share/polkit-1/actions/org.pbc.daemon1.policy"
install -D -m 644 dist/config.toml "$R/etc/pbcd/config.toml"
# System-wide extension: each user enables it once.
EXT="$R/usr/share/gnome-shell/extensions/$UUID"
install -d "$EXT"
install -m 644 extension/metadata.json extension/extension.js extension/prefs.js extension/status.js "$EXT/"
DOC="$R/usr/share/doc/$NAME"
install -D -m 644 README.md "$DOC/README.md"
install -m 644 docs/DEVICE_SUPPORT.md "$DOC/"
install -D -m 644 packaging/debian/copyright "$DOC/copyright"

# Control files.
C="$R/DEBIAN"
install -d "$C"
install -m 755 packaging/debian/postinst packaging/debian/prerm packaging/debian/postrm "$C/"
echo /etc/pbcd/config.toml > "$C/conffiles"
SIZE=$(du -sk --exclude=DEBIAN "$R" | cut -f1)
sed -e "s/@VERSION@/$VERSION/" -e "s/@ARCH@/$ARCH/" -e "s/@SIZE@/$SIZE/" \
  packaging/debian/control > "$C/control"

DEB="$OUT/${NAME}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$R" "$DEB" >/dev/null
echo "$DEB"
echo "$OUT/$UUID.shell-extension.zip"
