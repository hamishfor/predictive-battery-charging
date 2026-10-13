#!/usr/bin/env bash
# Q: Can we wake from s2idle on an RTC alarm (lid closed, on AC), change the
#    threshold, and re-suspend? Is the wake dark (screen off) and quiet?
#
# Uses an absolute wall-clock timer (OnCalendar, CLOCK_REALTIME_ALARM).
# The first run used OnActiveSec, which counts CLOCK_MONOTONIC. That clock
# stops during suspend, so the timer re-fired on every wake until 120 s of
# awake time had passed (16 wakes).
#
# Usage: sudo ./03-rtc-wake.sh [seconds=120]
# Then close the lid. Watch and listen during the wake.
source "$(dirname "$0")/common.sh"; need_root
secs="${1:-120}"
"$PBC" set --start 75 --end 80
cat > /usr/local/lib/pbc-spike-wake.sh <<EOS
#!/bin/sh
lid=\$(awk '{print \$2}' /proc/acpi/button/lid/*/state 2>/dev/null)
echo "[\$(date -Is)] wake: lid=\$lid ac=\$(cat /sys/class/power_supply/*/online 2>/dev/null | grep -c 1) thr_before=\$(cat $BAT/charge_control_start_threshold)/\$(cat $BAT/charge_control_end_threshold)" >> $OUT/spike.log
$PBC full >> $OUT/spike.log 2>&1
sleep 5
lid=\$(awk '{print \$2}' /proc/acpi/button/lid/*/state 2>/dev/null)
if [ "\$lid" = closed ]; then
  echo "[\$(date -Is)] re-suspending (lid closed)" >> $OUT/spike.log
  systemctl suspend
else
  echo "[\$(date -Is)] lid open: staying awake" >> $OUT/spike.log
fi
EOS
chmod +x /usr/local/lib/pbc-spike-wake.sh
systemctl stop pbc-spike-wake.timer 2>/dev/null || true
when=$(date -d "+${secs} seconds" '+%Y-%m-%d %H:%M:%S')
systemd-run --unit=pbc-spike-wake --on-calendar="$when" --timer-property=WakeSystem=true --timer-property=AccuracySec=1s /usr/local/lib/pbc-spike-wake.sh
before=$(cat /sys/power/suspend_stats/success)
log "03 armed: wake at $when. Suspend now (close lid). Thresholds $(thr). suspend_success=$before"
log "03 afterwards: expect exactly one 'wake:' line after this, then check: cat /sys/power/suspend_stats/success (expect $((before + 2)))"
