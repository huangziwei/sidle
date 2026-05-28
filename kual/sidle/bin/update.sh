#!/bin/sh
# KUAL "Update Sidle (Wi-Fi)" runs this. A DEDICATED, argless launcher: KUAL did
# not reliably pass `--update` through a `bin/sidle.sh --update` menu action (the
# binary launched the gallery instead), so the flag is hardcoded here. The picker
# pulls its own next binary from sidle-server and stages it as bin/sidle.new; the
# normal launcher (sidle.sh) applies the swap before the next exec.
#
# Logs to its OWN file so the self-update trail isn't buried in the gallery log;
# the binary's stdout+stderr are redirected here too (so panics land as well).
EXT=/mnt/us/extensions/sidle
ULOG=/mnt/us/sidle-update.log
echo "[$(date)] update.sh launch $(uname -m)" >> "$ULOG"
"$EXT/bin/sidle" --update >> "$ULOG" 2>&1
echo "[$(date)] update.sh exit=$?" >> "$ULOG"
