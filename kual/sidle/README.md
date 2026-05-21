# sidle (KUAL extension)

Native picker for the [sidle](../../README.md) LAN server. Tap a book on
your jailbroken Kindle, file lands in `/mnt/us/documents/Sidle/`, the
stock Kindle reader opens it. No browser, no sync — just a download
button shaped like a cover grid.

Tested on KOA2 firmware 5.16.2.1.1. The framebuffer + touch code is
generation-agnostic but only verified on this device.

## Files

```
extensions/sidle/
├── config.xml              KUAL extension metadata
├── menu.json               single entry "Sidle" → bin/sidle.sh
├── bin/
│   ├── sidle               armv7-musleabihf static Rust binary
│   └── sidle.sh            wrapper script, logs invocations
└── etc/
    └── server.conf         your Mac's IP + sidle-server auth token
```

## Install / Update

After a one-time KUAL bootstrap (KUAL itself + `/mnt/us/extensions/`
present on the device), every subsequent push is one click:

1. Build the binary:
   ```
   cargo build --release --target armv7-unknown-linux-musleabihf -p sidle-native
   ```
2. Open the sidle desktop app, plug Kindle via USB, click the device
   pill → **Install KUAL** (or **Update KUAL** if files are out of
   date). The button copies the binary, the bundle files, and writes
   `etc/server.conf` with the live LAN IP + sidle-server port + token
   — staleness is content-hashed per file so already-synced files are
   skipped. The same button also handles the most common silent
   failure (token rotated after a `.server-token` regen) by always
   re-rendering `server.conf` from the running server's current state.
3. Eject the Kindle. KUAL → **Sidle**.

### Manual install (first-time bootstrap or button unavailable)

If `/Volumes/Kindle/extensions/sidle/` doesn't exist yet, or the
desktop app isn't running, you can do it by hand:

1. Build the binary as above.
2. Plug Kindle via USB, copy `kual/sidle/` to
   `/Volumes/Kindle/extensions/`, replacing `bin/sidle` with the
   freshly built binary.
3. Copy `etc/server.conf.example` to `etc/server.conf`, fill in:
   - `HOST` — your Mac's LAN IP
   - `PORT` — sidle-server port (default `8731`)
   - `TOKEN` — contents of `~/Library/Application Support/sidle/.server-token`
4. Eject the Kindle. KUAL → **Sidle**.

If the picker launches but blanks back to KUAL with no toast, tail
`/mnt/us/sidle-native.log` on the next plug — a "token rejected" line
means `.server-token` rotated and the on-device `server.conf` is
stale. Click **Update KUAL** in the desktop app to resync.

## Usage

- 3×3 grid of covers (newest-first, server-side sort).
- Tap a cover → file downloads in 1–10s → Kindle library auto-indexes.
- Tap the **top-left 200×200 region** to exit cleanly.

## Known issues

- After exit, the top status bar (time/battery/wifi) sometimes shows
  stale until you trigger any framework redraw (swipe down for settings,
  tap any home tile, etc.). The library tile area itself redraws fine.
  Cause: `appmgrd start home` only repaints the home app, not the
  pillow chrome.
- Library > 9 books overflows the grid; pagination is the next milestone.
- KUAL's invocation can SIGKILL the binary if the user re-launches
  while it's still running. That leaves `cvm` SIGSTOP'd → screen
  appears frozen → power-hold reboots. Robust signal handling is on the
  to-do list.

## Logs

Every launch appends to `/mnt/us/sidle-native.log`. Inspect over USB
when something goes wrong.

## Architecture

- Cross-compiled Rust → static-musl armv7l binary. Pure-Rust toolchain
  via `rust-lld` (no zig, no Docker, no system C compiler).
- Direct mmap of `/dev/fb0` + `MXCFB_SEND_UPDATE` ioctl (v3 layout, 88B).
- Raw evdev on `/dev/input/eventN` with `EVIOCGRAB` for exclusive input.
- Framework lifecycle via `killall STOP cvm` / `killall CONT cvm` (with
  `appmgrd start home` to nudge the framework into repainting on exit).
- HTTP via `ureq` (no TLS — LAN only), JSON via serde.
- Text via `fontdue` + Kindle's bundled TBGothic font.
- Images via `image` (JPEG/PNG features only).
