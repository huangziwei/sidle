# Sidle

A personal-use macOS desktop app for managing a Kindle library: drag-and-drop
import of EPUB / KFX files, automatic KFX ↔ EPUB conversion via
[boko-kai](./boko-kai/), USB-attached Kindle detection with one-click push to
`/documents`, and auto-pull of DRM-stripped books from the device's `/dedrm/`
folder.

Longer-term goal: a companion KUAL app on a jailbroken Kindle that talks to
a sidle-hosted local EPUB server — pick a book on the device, get the KFX
pushed over wifi, no USB cable needed.

## Layout

- [`sidle/`](./sidle) — Tauri 2 desktop app (Rust + vanilla JS).
- [`boko-kai/`](./boko-kai) — pure-Rust ebook conversion library (KFX, EPUB,
  AZW3, MOBI). Used as a crate.
- `ref/` — calibre's KFX plugins, kept as the reference model for boko-kai.

## Build

```sh
cargo install tauri-cli --version "^2"
cd sidle && cargo tauri build
mv src-tauri/target/release/bundle/macos/sidle.app /Applications/
```

First launch: right-click → **Open** (one-time Gatekeeper bypass; the `.app`
is unsigned).

## Library data

`~/Library/Application Support/sidle/` — survives rebuilds.
