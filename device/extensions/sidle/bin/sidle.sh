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
echo "[$(date)] exit=$?" >> "$LOG"
