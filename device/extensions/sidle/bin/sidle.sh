#!/bin/sh
EXT=/mnt/us/extensions/sidle
LOGS=/mnt/us/logs
mkdir -p "$LOGS"
for old in /mnt/us/sidle-native.log /mnt/us/sidle-update.log; do
    if [ -f "$old" ]; then
        cat "$old" >> "$LOGS/${old##*/}" && rm -f "$old"
    fi
done
LOG=$LOGS/sidle-native.log
echo "[$(date)] launch $(uname -m)" >> "$LOG"

# `$SIDLE_ORIGIN_VIEW` names the view the tap came from. `startView` takes
# `<view_name>:<layer>:<app_uri>`; layer 0 is the top level.
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

if [ -f "$EXT/bin/sidle.new" ]; then
    mv -f "$EXT/bin/sidle.new" "$EXT/bin/sidle" && echo "[$(date)] applied LAN self-update" >> "$LOG"
fi
"$EXT/bin/sidle" "$@" 2>> "$LOG"
STATUS=$?
echo "[$(date)] exit=$STATUS" >> "$LOG"

exit "$STATUS"
