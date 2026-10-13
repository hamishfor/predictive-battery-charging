#!/usr/bin/env bash
# Q: How long does 80->100% take on this battery/charger? (top-off duration model)
# Needs AC. First get the battery held at 80%: `sudo pbcctl hold` and wait.
# This script then releases the limit to 100 and logs a sample every 30s.
# Usage: sudo ./04-charge-curve.sh   (leave it running; stop with Ctrl-C once Full)
source "$(dirname "$0")/common.sh"; need_root
f="$OUT/curve-$(date +%Y%m%d-%H%M).csv"
log "04 logging to $f"
"$PBC" full
"$PBC" log --interval 30 | tee "$f"
