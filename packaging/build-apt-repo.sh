#!/usr/bin/env bash
# Build a signed apt repository, for GitHub Pages, from a directory of .debs.
# Usage: packaging/build-apt-repo.sh DEB_DIR OUT_DIR
# Needs: apt-ftparchive (apt-utils), dpkg-deb, and gpg holding the signing key.
# SIGNING_KEY picks the key by id or fingerprint; defaults to gpg's default key.
set -euo pipefail
DEBS=$(realpath "$1")
OUT=$2
SUITE=stable
COMPONENT=main
BASE_URL=${BASE_URL:-https://hamishfor.github.io/predictive-battery-charging}
KEYRING=/etc/apt/keyrings/pbc.gpg
umask 022

rm -rf "$OUT"
install -d "$OUT/pool/$COMPONENT"
cp "$DEBS"/*.deb "$OUT/pool/$COMPONENT/"
cd "$OUT"

ARCHES=$(for d in pool/"$COMPONENT"/*.deb; do dpkg-deb -f "$d" Architecture; done | sort -u)
D=dists/$SUITE
for arch in $ARCHES; do
  B="$D/$COMPONENT/binary-$arch"
  install -d "$B"
  apt-ftparchive --arch "$arch" packages pool > "$B/Packages"
  gzip -9kn "$B/Packages"
done

# Written outside $D, so Release doesn't list itself.
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin="Predictive Battery Charging" \
  -o APT::FTPArchive::Release::Label="Predictive Battery Charging" \
  -o APT::FTPArchive::Release::Suite="$SUITE" \
  -o APT::FTPArchive::Release::Codename="$SUITE" \
  -o APT::FTPArchive::Release::Components="$COMPONENT" \
  -o APT::FTPArchive::Release::Architectures="$(echo "$ARCHES" | tr '\n' ' ' | sed 's/ $//')" \
  release "$D" > Release.tmp
mv Release.tmp "$D/Release"

SIGN=(gpg --batch --yes)
if [ -n "${SIGNING_KEY:-}" ]; then SIGN+=(--local-user "$SIGNING_KEY"); fi
"${SIGN[@]}" --clearsign -o "$D/InRelease" "$D/Release"
"${SIGN[@]}" --detach-sign --armor -o "$D/Release.gpg" "$D/Release"

# The public key, and a sources file pointing at it, for users to fetch.
gpg --export ${SIGNING_KEY:+"$SIGNING_KEY"} > pbc.gpg
[ -s pbc.gpg ] || { echo "No signing key in gpg" >&2; exit 1; }
cat > pbc.sources <<EOF
Types: deb
URIs: $BASE_URL/
Suites: $SUITE
Components: $COMPONENT
Signed-By: $KEYRING
EOF
