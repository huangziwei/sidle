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
# The reading-log archiver schedules itself from inside the binary, not from
# here. This script only reaches a device through a USB deploy, while the LAN
# self-update ships `bin/sidle` alone — so anything the launcher installs is
# invisible to the update path people actually use. Learned the hard way.
"$EXT/bin/sidle" "$@" 2>> "$LOG"
echo "[$(date)] exit=$?" >> "$LOG"
