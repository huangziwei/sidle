# device/ — what sidle puts on your Kindle

Native picker for the [sidle](../README.md) LAN server. Tap a book on
your jailbroken Kindle, file lands in `/mnt/us/documents/Sidle/`, the
stock Kindle reader opens it. No browser, no sync — just a download
button shaped like a cover grid.

It is **not a KUAL app**. The picker's front door is a jailbreak-hotfix
scriptlet the library indexes as a home-screen tile; KUAL is an optional
second door onto the very same launcher. See [Launching](#launching).

Tested on KOA2 firmware 5.16.2.1.1. The framebuffer + touch code is
generation-agnostic but only verified on this device.

## Layout

This directory is a **mirror of the Kindle's mount root**: every file sits at
the path it lands on the device, so `device/documents/Sidle.sh` is pushed to
`<mount>/documents/Sidle.sh`. Adding a file to the deploy means dropping it
here at its device path and adding one slot in `sidle/desktop/src/device/deploy.rs`.

```
device/
├── documents/
│   └── Sidle.sh            home-screen tile (hotfix scriptlet) — the launcher
├── extensions/sidle/
│   ├── bin/
│   │   ├── sidle           armv7-musleabihf static Rust binary (gitignored)
│   │   └── sidle.sh        wrapper: applies staged updates, logs, execs
│   ├── etc/
│   │   ├── server.conf     your Mac's IP + auth token (gitignored)
│   │   ├── server.conf.example
│   │   └── ca.pem          the CA the picker pins (pushed from the library root)
│   ├── config.xml          KUAL menu metadata — optional
│   └── menu.json           KUAL entry "Sidle" → bin/sidle.sh — optional
├── assets/cover.svg        source for the tile's embedded cover art
└── make-tile.sh            re-embeds that cover into documents/Sidle.sh
```

Two files there are outside the mirror: `assets/` and `make-tile.sh` are
sources that *produce* mirrored files, and are never pushed. Neither is
`etc/server.conf.example` — it's a template for humans; the real
`server.conf` is rendered per-device at install time.

## Launching

`documents/Sidle.sh` carries `# Name:` and `# Icon:` headers that the
jailbreak hotfix reads, indexing it as a library tile named **Sidle** (red
蛇行 cover, embedded as base64). Tapping the tile runs
`extensions/sidle/bin/sidle.sh`.

`config.xml` + `menu.json` register the same wrapper as a KUAL menu entry.
They are the only two KUAL-specific files here; delete them and the app still
installs, launches, and self-updates — you just lose the menu entry.

The scriptlet mechanism is not KUAL's: KUAL itself ships as one of these
scriptlets (see `ref/KUAL.sh`, same header format). Both are consumers of the
same jailbreak.

## Install / Update

The device needs a jailbreak and an `/mnt/us/extensions/` directory. After
that, every push is one click:

1. Build the binary:
   ```
   cargo build --release --target armv7-unknown-linux-musleabihf -p sidle-native
   ```
2. Open the sidle desktop app, plug Kindle via USB, click the device
   pill → **Install on Kindle** (or **Update on Kindle** if files are
   out of date). The button copies the binary, the mirrored files, and
   writes `etc/server.conf` with the live LAN IP + sidle-server port +
   token — staleness is content-hashed per file so already-synced files
   are skipped. The same button also handles the most common silent
   failure (token rotated after a `.server-token` regen) by always
   re-rendering `server.conf` from the running server's current state.
3. Eject the Kindle. Tap the **Sidle** tile on the home screen.

### Manual install (first-time bootstrap or button unavailable)

If `/Volumes/Kindle/extensions/sidle/` doesn't exist yet, or the
desktop app isn't running, you can do it by hand. Because this directory
mirrors the mount, it's a straight copy:

1. Build the binary as above.
2. Plug Kindle via USB, copy `device/extensions/` and `device/documents/`
   into `/Volumes/Kindle/`, replacing `extensions/sidle/bin/sidle` with the
   freshly built binary.
3. Copy `etc/server.conf.example` to `etc/server.conf`, fill in:
   - `HOST` — your Mac's LAN IP
   - `PORT` — sidle-server port (default `8731`)
   - `TOKEN` — contents of `~/Library/Application Support/sidle/.server-token`
4. Eject the Kindle. Tap the **Sidle** tile.

If the picker launches but blanks back to the home screen with no toast, tail
`/mnt/us/logs/sidle-native.log` on the next plug — a "token rejected" line
means `.server-token` rotated and the on-device `server.conf` is
stale. Click **Update on Kindle** in the desktop app to resync.

## Changing the tile's cover

Edit `assets/cover.svg`, then:

```
device/make-tile.sh
```

That renders the SVG at 1440×2200 (`rsvg-convert`), quantizes it to an 8-bit
palette (`pngquant` — the tile ships inline as base64, so the size matters),
writes `assets/cover.png`, and rewrites **only** the `# Icon:` line of
`documents/Sidle.sh`. Both tools are optional; the script reports which step
it skipped. The scriptlet's body is hand-edited — the generator never touches
it.

## Usage

- 3×3 grid of covers (newest-first, server-side sort).
- Tap a cover → file downloads in 1–10s → Kindle library auto-indexes.
- Tap the **top-left 200×200 region** to exit cleanly.
- **Update** button (right of the search bar): pulls the picker's own next
  binary from sidle-server over the LAN, sha256-verifies it, and stages it as
  `bin/sidle.new`; `bin/sidle.sh` swaps it in on the next launch, so reopen
  Sidle to apply. This is the everyday self-update — no USB, no second tile.
  (Break-glass: if the gallery won't boot, `sidle --update` runs the same
  pull with a minimal UI over a shell.)

## Known issues

- After exit, the top status bar (time/battery/wifi) sometimes shows
  stale until you trigger any framework redraw (swipe down for settings,
  tap any home tile, etc.). The library tile area itself redraws fine.
  Cause: `appmgrd start home` only repaints the home app, not the
  pillow chrome.
- Library > 9 books overflows the grid; pagination is the next milestone.
- Re-launching while the picker is still running can SIGKILL the binary,
  leaving `cvm` SIGSTOP'd → screen appears frozen → power-hold reboots. The
  tile guards against this with a `pidof` check; the KUAL menu entry does
  not. Robust signal handling is on the to-do list.

## Logs

Every launch appends to `/mnt/us/logs/sidle-native.log`; the LAN self-update
keeps its own trail in `/mnt/us/logs/sidle-update.log`. Inspect over USB when
something goes wrong, or read them in the desktop app's Files tab — a Sync
brings back everything under `/mnt/us/logs/`, this picker's logs and any other
app's that writes there.

## Architecture

- Cross-compiled Rust → static-musl armv7l binary. Pure-Rust toolchain
  via `rust-lld` (no zig, no Docker, no system C compiler).
- Direct mmap of `/dev/fb0` + `MXCFB_SEND_UPDATE` ioctl (v3 layout, 88B).
- Raw evdev on `/dev/input/eventN` with `EVIOCGRAB` for exclusive input.
- Framework lifecycle via `killall STOP cvm` / `killall CONT cvm` (with
  `appmgrd start home` to nudge the framework into repainting on exit).
- HTTPS via `ureq` 3 + `rustls` with the pure-Rust RustCrypto provider, JSON via
  serde. TLS everywhere, including on the LAN: the picker pins the CA at
  `etc/ca.pem` as its **only** trust root (the Mozilla root set is not compiled
  in at all), so no public CA can issue a certificate it will accept. There is
  no plaintext fall-back and the scheme is not configurable.
- Text via `fontdue` + Kindle's bundled TBGothic font.
- Images via `image` (JPEG/PNG features only).
