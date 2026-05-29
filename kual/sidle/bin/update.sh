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
# Record which picker version is actually on the device. `sidle --version` reads
# it from the binary itself — the only source that stays accurate after a Wi-Fi
# update (that swaps the binary but not config.xml). The new launcher only ever
# ships next to a binary that supports --version (USB push writes both; a Wi-Fi
# update swaps just the binary), so the call is safe; the fallback covers a hiccup.
echo "[$(date)] update.sh launch $(uname -m) $("$EXT/bin/sidle" --version 2>/dev/null || echo 'sidle ?')" >> "$ULOG"
"$EXT/bin/sidle" --update >> "$ULOG" 2>&1
echo "[$(date)] update.sh exit=$?" >> "$ULOG"
