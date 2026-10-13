#!/usr/bin/env bash
PBC_ALLOW_UPOWER=1  # this test toggles UPower itself
# Q: How do UPower's EnableChargeThreshold and a UPower restart interact with our writes?
# Usage: sudo ./05-upower.sh
source "$(dirname "$0")/common.sh"; need_root
dev=/org/freedesktop/UPower/devices/battery_$BATNAME
up() { busctl call org.freedesktop.UPower $dev org.freedesktop.UPower.Device EnableChargeThreshold b "$1"; sleep 1; }
en() { busctl get-property org.freedesktop.UPower $dev org.freedesktop.UPower.Device ChargeThresholdEnabled | awk '{print $2}'; }
log "05 initial: upower_enabled=$(en) thr=$(thr)"
up true;  log "05 after UPower enable: thr=$(thr)"
up false; log "05 after UPower disable: thr=$(thr)"
"$PBC" set --start 61 --end 67; log "05 our write: thr=$(thr) upower_enabled=$(en)"
systemctl restart upower; sleep 3; log "05 after upower restart (disabled): thr=$(thr)"
up true; systemctl restart upower; sleep 3; log "05 after upower restart (enabled): thr=$(thr)"
"$PBC" set --start 61 --end 67; systemctl restart upower; sleep 3; log "05 our write while enabled + restart: thr=$(thr)"
up false; "$PBC" full; log "05 restored: thr=$(thr) upower_enabled=$(en)"
