#!/usr/bin/env bash
# Q: Does the EC stop at `end` and resume at `start`? How fast do changes take effect?
# Needs AC plugged in. Sets end just below current capacity and watches status.
# Usage: sudo ./02-enforcement.sh
source "$(dirname "$0")/common.sh"; need_root
[ "$(ac)" = 1 ] || { echo "plug in AC first"; exit 1; }
cap=$(cat $BAT/capacity); end=$((cap - 2)); start=$((end - 5))
[ $end -gt 10 ] || { echo "battery too low for this test"; exit 1; }
log "02 start: cap=$cap status=$(cat $BAT/status) thr=$(thr)"
"$PBC" set --start "$start" --end "$end"
for i in $(seq 1 24); do sleep 5; s=$(cat $BAT/status); log "02 t+$((i*5))s status=$s power=$(cat $BAT/power_now)"; [ "$s" != Charging ] && break; done
[ "$(cat $BAT/status)" != Charging ] && log "02 PASS stop: charging halted above end=$end" || log "02 FAIL stop: still charging"
"$PBC" full
for i in $(seq 1 24); do sleep 5; s=$(cat $BAT/status); log "02 t+$((i*5))s after full: status=$s"; [ "$s" = Charging ] && break; done
[ "$(cat $BAT/status)" = Charging ] && log "02 PASS resume: charging resumed after end=100" || log "02 FAIL resume: did not resume (may need replug)"
