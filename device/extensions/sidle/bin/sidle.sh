#!/bin/sh
# The home-screen tile (documents/Sidle.sh) launches this; it runs the static
# Rust binary and outlives it, so the picker's status and the screen it hands
# back to are both decided here.
# Captures any stderr to a sibling log so a non-zero exit isn't silent.
EXT=/mnt/us/extensions/sidle
# One folder for every app's logs, rather than a scatter across the USB root.
# This is also what a Sync scans, so the neighbours' logs come back with ours.
LOGS=/mnt/us/logs
mkdir -p "$LOGS"
# Fold in the two logs older builds kept in the root. Self-limiting: once
# they're gone this is a pair of failed tests. Done here rather than in the
# binary because this script's own `2>>` redirect below holds the destination
# open — moving the file out from under an open fd on FAT is how you lose it.
for old in /mnt/us/sidle-native.log /mnt/us/sidle-update.log; do
    if [ -f "$old" ]; then
        cat "$old" >> "$LOGS/${old##*/}" && rm -f "$old"
    fi
done
LOG=$LOGS/sidle-native.log
echo "[$(date)] launch $(uname -m)" >> "$LOG"
# Apply a staged self-update (written by the picker's in-app Update button) before
# we exec — never overwrite the running binary on FAT (ETXTBSY/corruption). The
# picker sha256-verifies the download before staging it as .new, so this swap is
# unconditional; a USB "Update on Kindle" clears any pending .new, so it can't clobber
# a newer USB push. No chmod needed — FAT has no mode bits and exec already works.
if [ -f "$EXT/bin/sidle.new" ]; then
    # Stop the reading-log archiver before the swap. It runs from bin/sidle for
    # weeks at a time, and the user partition is FAT: a rename over the file a
    # process is executing frees the clusters it is running from, there being no
    # inode to keep the old copy alive. It would also go on running the old code
    # indefinitely — its pidfile is exactly what tells the picker an archiver is
    # already up, so nothing would ever replace it. Dropping the pidfile with it
    # is what makes the picker start a fresh one, below, from the new binary.
    PID=$(cat "$EXT/archive.pid" 2>/dev/null)
    case "$(tr '\0' ' ' 2>/dev/null < "/proc/$PID/cmdline")" in
        *--archive-daemon*)
            kill "$PID" 2>/dev/null
            rm -f "$EXT/archive.pid"
            echo "[$(date)] stopped archive daemon (pid $PID) for update" >> "$LOG"
            ;;
    esac
    mv -f "$EXT/bin/sidle.new" "$EXT/bin/sidle" && echo "[$(date)] applied LAN self-update" >> "$LOG"
fi
# The reading-log archiver starts itself from inside the binary, not from here:
# this script reaches a device only through a USB deploy, while the LAN
# self-update ships `bin/sidle` alone, so anything the launcher sets up would be
# absent on a device updated over Wi-Fi.
"$EXT/bin/sidle" "$@" 2>> "$LOG"
# The picker's status, held in a variable: `$?` after the landing below is the
# app manager's answer, and the log and this script's own exit both want the
# picker's.
STATUS=$?
echo "[$(date)] exit=$STATUS" >> "$LOG"

# The screen the tap came from, asked for once the picker is gone. The app
# manager holds nothing of sidle's on its history stack — a `documents/`
# scriptlet is registered without a `lipcId`, so nothing is ever put there —
# and its own fallback is the home screen. The tile carries the originating
# view in; with neither variable set, the manager chooses.
#
# `startView` takes `<view_name>:<layer>:<app_uri>` and acts on the view name,
# so the address is built from the same name it carries. Layer 0 is the top
# level, which the home screen and the library both are.
echo "[$(date)] origin ${SIDLE_ORIGIN_VIEW:-none}" >> "$LOG"
case "${SIDLE_ORIGIN_VIEW:-}" in
    KPP_*|LEGACY_*)
        lipc-set-prop com.lab126.appmgrd startView \
            "$SIDLE_ORIGIN_VIEW:0:app://com.lab126.KPPMainApp?view=$SIDLE_ORIGIN_VIEW" \
            2>/dev/null
        ;;
esac

exit "$STATUS"
