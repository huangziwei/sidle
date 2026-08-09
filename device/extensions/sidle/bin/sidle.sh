#!/bin/sh
# The home-screen tile (documents/Sidle.sh) and the KUAL menu entry both launch
# this; we exec the static Rust binary.
# Captures any stderr to a sibling log so a non-zero exit isn't silent.
EXT=/mnt/us/extensions/sidle
LOG=/mnt/us/sidle-native.log
echo "[$(date)] launch $(uname -m)" >> "$LOG"
# Apply a staged self-update (written by the picker's in-app Update button) before
# we exec — never overwrite the running binary on FAT (ETXTBSY/corruption). The
# picker sha256-verifies the download before staging it as .new, so this swap is
# unconditional; a USB "Update on Kindle" clears any pending .new, so it can't clobber
# a newer USB push. No chmod needed — FAT has no mode bits and exec already works.
if [ -f "$EXT/bin/sidle.new" ]; then
    mv -f "$EXT/bin/sidle.new" "$EXT/bin/sidle" && echo "[$(date)] applied LAN self-update" >> "$LOG"
fi
# Make sure the reading-log archiver is scheduled. The firmware keeps only ~30
# daily log dumps and prunes the oldest, so a trip longer than a month loses its
# beginning before any sync can collect it; `--archive` copies the event lines
# somewhere permanent. It has to run without anyone opening the picker, hence
# cron — and hence installing it from here, the one thing that definitely runs
# after an update or a fresh install.
#
# Every half hour, round the clock. Cron does not fire while the device is
# suspended, so an entry is only reached if the reader happens to be awake then —
# which is most of the time not. Frequency is what buys coverage, and it is
# nearly free: the work is set by how much log there is to read, not by how often
# we look, so 48 small runs cost about what 4 large ones do. No overnight cutoff,
# because reading at 00:51 is in this device's own history and a cron line that
# does not fire costs nothing. Idempotent — grep before appending, so relaunching
# does not stack entries.
CRONTAB=/etc/crontab/root
CRONLINE="*/30 * * * * $EXT/bin/sidle --archive >/dev/null 2>&1"
if [ -d "$(dirname "$CRONTAB")" ] && ! grep -qF -- "--archive" "$CRONTAB" 2>/dev/null; then
    echo "$CRONLINE" >> "$CRONTAB" && echo "[$(date)] installed archive cron" >> "$LOG"
fi
# Also archive on every launch, so the log is captured even where cron is absent
# or the device is rarely awake on the hour. Backgrounded: the picker must not
# wait on it.
"$EXT/bin/sidle" --archive >/dev/null 2>&1 &

"$EXT/bin/sidle" "$@" 2>> "$LOG"
echo "[$(date)] exit=$?" >> "$LOG"
