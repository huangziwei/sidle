#!/bin/sh
# Probe what busybox ships on this KOA2 firmware. Writes everything we want
# to know into /mnt/us/sidle-probe.txt; replug, read from the Mac.
#
# Phase 0 of the sidle server+KUAL plan: confirm `busybox httpd` is callable
# before we commit Phase 5's helper design to it.

OUT=/mnt/us/sidle-probe.txt

{
  echo "[sidle-probe] start: $(date 2>&1)"
  echo

  echo "=== uname -a ==="
  uname -a 2>&1
  echo

  echo "=== firmware ==="
  cat /mnt/us/system/version.txt 2>&1 || echo "(no version.txt)"
  echo

  echo "=== PATH ==="
  echo "$PATH"
  echo

  echo "=== which busybox ==="
  command -v busybox 2>&1 || echo "(busybox not in PATH)"
  echo

  echo "=== busybox banner ==="
  busybox 2>&1 | head -20
  echo

  echo "=== busybox --list (full applet list) ==="
  busybox --list 2>&1
  echo

  echo "=== httpd applet present? ==="
  busybox --list 2>&1 | grep -i '^httpd$' || echo "(httpd not in --list)"
  echo "  --- httpd -h: ---"
  busybox httpd -h 2>&1 | head -40 || echo "(httpd -h failed)"
  echo

  echo "=== nc applet present? ==="
  busybox --list 2>&1 | grep -i '^nc$' || echo "(nc not in --list)"
  echo

  echo "=== curl ==="
  command -v curl 2>&1 || echo "(no curl)"
  curl --version 2>&1 | head -2
  echo

  echo "=== ifconfig (look for usb0 / wlan0) ==="
  ifconfig 2>&1 || busybox ifconfig 2>&1
  echo

  echo "[sidle-probe] done: $(date 2>&1)"
} > "$OUT" 2>&1
