#!/usr/bin/env bash
# Q: Do thresholds survive reboot and full power-off (EC persistence)?
# Usage: sudo ./01-persistence.sh set    -> writes 61/67, then you reboot or power off
#        sudo ./01-persistence.sh check  -> after boot, reports what the EC kept
#        sudo ./01-persistence.sh restore
source "$(dirname "$0")/common.sh"; need_root
case "${1:-}" in
  set)     "$PBC" set --start 61 --end 67; cat /proc/sys/kernel/random/boot_id > "$OUT/01-boot_id"
           log "01 set: $(thr). Now reboot (or power off, wait 30s, power on) then run: check" ;;
  check)   [ -f "$OUT/01-boot_id" ] || { echo "run 'set' first"; exit 1; }
           if [ "$(cat /proc/sys/kernel/random/boot_id)" = "$(cat "$OUT/01-boot_id")" ]; then
             echo "still the same boot as 'set': reboot or power off first"; exit 1; fi
           t=$(thr); kind="boot $(uptime -s)"
           if [ "$t" = "61/67" ]; then log "01 PASS: thresholds persisted ($t), $kind"; else log "01 FAIL: got $t, expected 61/67, $kind"; fi
           rm -f "$OUT/01-boot_id" ;;
  restore) "$PBC" full; log "01 restored: $(thr)" ;;
  *) sed -n 2,6p "$0"; exit 2 ;;
esac
