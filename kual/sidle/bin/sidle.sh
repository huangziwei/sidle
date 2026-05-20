#!/bin/sh
# KUAL launches this; we exec the static Rust binary.
# Captures any stderr to a sibling log so a non-zero exit isn't silent.
EXT=/mnt/us/extensions/sidle
LOG=/mnt/us/sidle-native.log
echo "[$(date)] launch $(uname -m)" >> "$LOG"
"$EXT/bin/sidle" "$@" 2>> "$LOG"
echo "[$(date)] exit=$?" >> "$LOG"
