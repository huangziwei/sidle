#!/bin/sh
# documents/Sidle.sh execs this; it runs the static Rust binary and outlives it.
# The picker's exit status, its stderr log, and the screen it hands back to are
# all settled here.
EXT=/mnt/us/extensions/sidle
# One folder for every app's logs, and what a Sync scans.
LOGS=/mnt/us/logs
mkdir -p "$LOGS"
# Any log left in the USB root moves into $LOGS. The `2>>` redirect below holds
# $LOG open, and a move out from under an open fd on FAT loses the file.
for old in /mnt/us/sidle-native.log /mnt/us/sidle-update.log; do
    if [ -f "$old" ]; then
        cat "$old" >> "$LOGS/${old##*/}" && rm -f "$old"
    fi
done
LOG=$LOGS/sidle-native.log
echo "[$(date)] launch $(uname -m)" >> "$LOG"

# $SIDLE_ORIGIN_VIEW is the screen the tap came from, carried in by the tile.
# `startView` takes `<view_name>:<layer>:<app_uri>` and acts on the view name;
# layer 0 is the top level, which the home screen and the library both are.
#
# On the EXIT trap: whatever ends this script — the picker returning, or
# something above it giving up first — lands on the same screen.
land() {
    echo "[$(date)] origin ${SIDLE_ORIGIN_VIEW:-none}" >> "$LOG"
    case "${SIDLE_ORIGIN_VIEW:-}" in
        KPP_*|LEGACY_*) ;;
        *) return 0 ;;
    esac
    lipc-set-prop com.lab126.appmgrd startView \
        "$SIDLE_ORIGIN_VIEW:0:app://com.lab126.KPPMainApp?view=$SIDLE_ORIGIN_VIEW" \
        2>/dev/null
}
trap land EXIT

# `$EXT/bin/sidle.new` is applied here, one level above the binary it replaces:
# FAT keeps no inode alive under a rename. The picker sha256-verifies a download
# before staging it, and a cable push clears any pending .new.
if [ -f "$EXT/bin/sidle.new" ]; then
    # `$EXT/archive.pid` names a process running from bin/sidle. The rename
    # below frees the clusters it executes from, and dropping the pidfile is
    # what makes the picker start a fresh one from the new binary.
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
# The reading-log archiver starts itself from inside the binary: an update
# writes this script without running it.
"$EXT/bin/sidle" "$@" 2>> "$LOG"
# `$?` from the picker, before the log line below replaces it.
STATUS=$?
echo "[$(date)] exit=$STATUS" >> "$LOG"

exit "$STATUS"
