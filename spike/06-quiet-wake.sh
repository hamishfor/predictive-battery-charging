#!/usr/bin/env bash
# Q: Can a maintenance wake change only the charge thresholds, with GNOME and
#    apps never thawing, and with the lid/power button always resuming normally?
# Hook source: spike/pbc-spike-quiet.hook. Production hook: crates/pbcd/src/bin/pbcd-sleep-hook.rs.
#
# Usage:
#   sudo ./06-quiet-wake.sh install                 install the sleep hook (no-op without a plan)
#   sudo ./06-quiet-wake.sh arm SECS [hold=N] [hang=1]
#                                                    set 75/80 now, plan a wake in SECS that sets 0/100
#                                                    then close the lid within SECS
#   sudo ./06-quiet-wake.sh report                  after opening the lid
#   sudo ./06-quiet-wake.sh uninstall               remove hook and plan, clear RTC, restore 0/100
#
# Scenarios:
#   A  arm 120, lid closed, wait about 5 min, open lid      one quiet nested wake
#   B  as A but open the lid hours later                  normal resume
#   C  arm 120 hold=20, open lid ~5 s after the wake time  hook exits on lid-open
#   D  arm 300, open the lid at ~2 min                    normal resume, no maintenance
#   E  arm 300, press power button at ~2 min, lid closed  normal resume
#   F  arm 300, unplug AC at ~2 min                       record behaviour
#   G  arm 120 hang=1                                     write times out, normal resume
source "$(dirname "$0")/common.sh"; need_root
HOOK=/usr/lib/systemd/system-sleep/pbc-spike-quiet
RUN=/run/pbc-spike

case "${1:-}" in
  install)
    sed -e "s|@OUT@|$OUT|" -e "s|@PBC@|$PBC|" "$HERE/pbc-spike-quiet.hook" > "$HOOK.tmp"
    chmod 755 "$HOOK.tmp"; mv "$HOOK.tmp" "$HOOK"
    install -d -m 700 "$RUN"
    # Clear any plan left by an earlier run, so the hook is truly inactive until armed.
    rm -f "$RUN/plan" "$RUN/done" "$RUN/armed-at" "$RUN/success-before"
    echo 0 > /sys/class/rtc/rtc0/wakealarm
    log "06 install: hook at $HOOK (inactive until armed; previous plan cleared)"
    echo "next: sudo $0 arm SECS [hold=N] [hang=1], then close the lid"
    ;;
  arm)
    [ -x "$HOOK" ] || { echo "run install first"; exit 1; }
    [ "$(ac)" = 1 ] || { echo "plug in AC first"; exit 1; }
    secs="${2:?seconds}"; shift 2
    hold=0; hang=0
    for a in "$@"; do case "$a" in hold=*) hold=${a#hold=};; hang=1) hang=1;; *) echo "bad arg $a"; exit 2;; esac; done
    install -d -m 700 "$RUN"; rm -f "$RUN/done"
    "$PBC" set --start 75 --end 80 >/dev/null
    at=$(( $(date +%s) + secs ))
    printf 'WAKE_AT=%s\nSTART=0\nEND=100\nHOLD_SECS=%s\nHANG=%s\n' "$at" "$hold" "$hang" > "$RUN/plan"
    cat /sys/power/suspend_stats/success > "$RUN/success-before"
    date -Is > "$RUN/armed-at"
    log "06 armed: wake at $(date -d "@$at" +%T) hold=$hold hang=$hang thr=$(thr). Close the lid now."
    echo ">>> wake scheduled for $(date -d "@$at" +%T). Close the lid now."
    ;;
  report)
    since=$(cat "$RUN/armed-at" 2>/dev/null) || { echo "not armed since the last install: nothing to report"; exit 1; }
    log "06 report: suspend_success $(cat "$RUN/success-before") -> $(cat /sys/power/suspend_stats/success), thr=$(thr), done=$([ -f "$RUN/done" ] && echo yes || echo no)"
    echo "--- hook log"
    awk -v s="[$since" '$0 >= s' "$OUT/spike.log" | grep '06 '
    echo "--- journal: sleep, thaw, lid, bluetooth"
    journalctl --no-pager -o short-precise --since "$(date -d "$since" '+%F %T')" \
      | grep -E 'PM: suspend (entry|exit)|pbc-hook|thawed unit .user.slice|Lid (opened|closed)|bluetoothd|Power key|Delay lock' | grep -v 'kmsg' | head -60
    n=$(journalctl --no-pager --since "$(date -d "$since" '+%F %T')" | grep -c "Successfully thawed unit 'user.slice'")
    echo "--- user.slice thaws since arm: $n (expect 1 per real resume)"
    ;;
  uninstall)
    rm -f "$HOOK" "$RUN/plan" "$RUN/done"
    echo 0 > /sys/class/rtc/rtc0/wakealarm
    "$PBC" full >/dev/null
    log "06 uninstall: hook removed, rtc cleared, thr=$(thr)"
    ;;
  *) sed -n 2,24p "$0"; exit 2 ;;
esac
