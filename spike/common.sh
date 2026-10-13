# Shared helpers for the device-check scripts. Source this file; don't run it.
# Set PBC_BAT=BAT1 to test a second battery.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$HERE")"
PBC="${PBC:-$REPO/target/release/pbcctl}"
OUT="$HERE/out"; mkdir -p "$OUT"
BATNAME="${PBC_BAT:-BAT0}"
BAT=/sys/class/power_supply/$BATNAME
[ -e "$BAT/charge_control_end_threshold" ] || { echo "$BAT has no charge_control_end_threshold. This laptop's driver doesn't expose charge limits." >&2; exit 1; }
[ -x "$PBC" ] || { echo "build first: cargo build --release" >&2; exit 1; }
log() { echo "[$(date -Is)] $*" | tee -a "$OUT/spike.log"; }
need_root() { [ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }; }
thr() { echo "$(cat $BAT/charge_control_start_threshold)/$(cat $BAT/charge_control_end_threshold)"; }
# Online if any mains or USB-C supply is.
ac() { for d in /sys/class/power_supply/*; do t=$(cat "$d/type" 2>/dev/null); case "$t" in Mains|USB) [ "$(cat "$d/online" 2>/dev/null)" = 1 ] && { echo 1; return; };; esac; done; echo 0; }
upower_limit() { busctl get-property org.freedesktop.UPower /org/freedesktop/UPower/devices/battery_$BATNAME org.freedesktop.UPower.Device ChargeThresholdEnabled 2>/dev/null | awk '{print $2}'; }
if [ "$(upower_limit)" = true ] && [ "${PBC_ALLOW_UPOWER:-}" != 1 ]; then
  echo "GNOME's charge limit is on (Settings > Power > Preserve Battery Health). It rewrites the limits and spoils these checks." >&2
  echo "Choose Maximize Charge, and stop pbcd if it's installed (sudo systemctl stop pbcd), then run this again." >&2
  echo "[$(date -Is)] REFUSED $(basename "$0") $*: UPower charge limit is on (Settings > Power > Preserve battery health)" >> "$OUT/spike.log"
  exit 1
fi
