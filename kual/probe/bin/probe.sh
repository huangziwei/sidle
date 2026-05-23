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

  # --- Input devices: find the bezel page-button device + its keycodes, so the
  #     native picker can grab it (stop the framework repainting on a press) and
  #     map the buttons to prev/next. The touchscreen is already known; we need
  #     the *separate* gpio-keys/button device's Name, Handlers (eventN), and
  #     KEY= bitmask.
  echo "=== /proc/bus/input/devices ==="
  cat /proc/bus/input/devices 2>&1 || echo "(no /proc/bus/input/devices)"
  echo

  # --- Live key capture. Read every event node in parallel for ~12s while the
  #     user presses the page buttons; whichever node yields EV_KEY records is
  #     the button device. Read-only (no EVIOCGRAB), so it can't lock out the
  #     power/home buttons. Background-cat + kill instead of `timeout` to stay
  #     robust across busybox arg-order variants.
  echo "=== key-event capture (~12s — PRESS BOTH PAGE BUTTONS REPEATEDLY) ==="
  : > /tmp/sidle-cap.pids
  for dev in /dev/input/event*; do
    [ -e "$dev" ] || continue
    base=$(basename "$dev")
    cat "$dev" > "/tmp/sidle-cap.$base.bin" 2>/dev/null &
    echo "$!" >> /tmp/sidle-cap.pids
  done
  sleep 12
  while read pid; do kill "$pid" 2>/dev/null; done < /tmp/sidle-cap.pids
  rm -f /tmp/sidle-cap.pids
  # 16-byte records on this kernel: type@8-9, code@10-11, value@12-15 (LE).
  # EV_KEY=0x0001, value 1=press / 0=release. Non-empty dumps = that device saw
  # input during the window.
  for f in /tmp/sidle-cap.event*.bin; do
    [ -e "$f" ] || continue
    echo "--- $f ($(wc -c < "$f" 2>/dev/null) bytes) ---"
    hexdump -C "$f" 2>&1
    rm -f "$f"
  done
  echo

  echo "[sidle-probe] done: $(date 2>&1)"
} > "$OUT" 2>&1
