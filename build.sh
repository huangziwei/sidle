#!/bin/sh
# Build sidle desktop app + on-Kindle native picker, install to /Applications.
#
# Three cargo invocations, run sequentially from the workspace root:
#   1. Cross-compile sidle-native for the Kindle (armv7 musl static).
#   2. Build sidle-server (the LAN daemon the release app spawns as a detached
#      child). The release app loads that sidecar from inside its own bundle;
#      the debug build is the one that builds it on demand.
#   3. Build the Tauri desktop app for the host Mac.
# Two things are staged under sidle/desktop between 2 and 3, which `cargo tauri
# build` folds into the bundle. The installed .app then reaches back into this
# checkout for nothing at runtime:
#   - the sidle-server binary as a Tauri sidecar (-> Contents/MacOS/sidle-server)
#   - the device/ mount mirror + armv7 picker as resources (-> Contents/Resources)
# Then ditto the bundle into /Applications, replacing any prior copy.
#
# A script, not `cargo tauri build`'s build.rs: cargo nested inside cargo
# livelocks on the shared workspace lockfile.

set -eu

cd "$(dirname "$0")"

DEVICE_TARGET="armv7-unknown-linux-musleabihf"

# Precheck naming the missing cross target, ahead of cargo's opaque "can't find
# core for armv7-..." panic.
if ! rustup target list --installed | grep -qx "$DEVICE_TARGET"; then
    echo "error: rustup target '$DEVICE_TARGET' is not installed" >&2
    echo "       fix: rustup target add $DEVICE_TARGET" >&2
    exit 1
fi

# Stamp the workspace version into the KUAL menu entry's config.xml, the one
# release artifact outside Cargo's reach. The desktop app pushes the file
# verbatim to the Kindle, where a menu reads it.
#
# [workspace.package].version in the root Cargo.toml is the source of truth. The
# sidle-* crates, the on-device binary among them, take it through
# `version.workspace = true`.
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/^version *= *"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "error: no version found in root Cargo.toml [workspace.package]" >&2; exit 1; }
echo "==> Stamping config.xml version ($VERSION)"
sed -i '' -E "s#<version>[^<]*</version>#<version>${VERSION}</version>#" device/extensions/sidle/config.xml

# One build timestamp (unix seconds) per run, baked into the picker binary
# (build.rs reads SIDLE_BUILD_TS) and written to the sidle.build-ts sidecar the
# server folds into the LAN-update manifest as `built_at`. One clock on both
# sides is what the device compares a self-update against.
BUILD_TS="$(date +%s)"
echo "==> Cross-compiling sidle-native for Kindle ($DEVICE_TARGET)  [build_ts=$BUILD_TS]"
SIDLE_BUILD_TS="$BUILD_TS" cargo build --release --target "$DEVICE_TARGET" -p sidle-native
# The build time next to the binary DeploySource points at on the dev path.
# build.rs bakes the same value into the binary.
printf '%s' "$BUILD_TS" > "target/$DEVICE_TARGET/release/sidle-native.build-ts"

# The cross-built picker at the path it installs to, completing the `device/`
# mount mirror. Both files are gitignored build products.
mkdir -p device/extensions/sidle/bin
cp "target/$DEVICE_TARGET/release/sidle-native" device/extensions/sidle/bin/sidle
cp "target/$DEVICE_TARGET/release/sidle-native.build-ts" \
    device/extensions/sidle/bin/sidle.build-ts

echo "==> Building sidle-server (LAN daemon: app spawns it; the Kindle reaches it)"
cargo build --release -p sidle-server

# Everything the bundle carries for a standalone .app. Tauri names sidecars
# `<path>-<target-triple>` and strips the suffix when copying into
# Contents/MacOS; a host-native build takes the host triple.
#
# The device resources reproduce the `device/` mount mirror
# DeploySource::from_resource_root() reads under Contents/Resources/resources/
# device. Each file sits at the path it installs to; etc/server.conf is absent.
HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$HOST_TRIPLE" ] || { echo "error: could not read host target triple from rustc -vV" >&2; exit 1; }

# The PDF→KFX cover and selectable-text layer render through macOS PDFKit / Core
# Graphics, system frameworks. No libpdfium is fetched or bundled.

echo "==> Staging sidle-server sidecar ($HOST_TRIPLE) + device resources for bundling"
# The one place this path is written. Both staging dirs and the `cargo tauri
# build` below hang off it, and .gitignore names the same two. A rename touches
# all four together.
APP_DIR="sidle/desktop"
SIDECAR_DIR="$APP_DIR/binaries"
RES_DEVICE="$APP_DIR/resources/device"
rm -rf "$SIDECAR_DIR" "$RES_DEVICE"
mkdir -p "$SIDECAR_DIR" "$RES_DEVICE/extensions/sidle/bin" "$RES_DEVICE/documents"
cp target/release/sidle-server "$SIDECAR_DIR/sidle-server-$HOST_TRIPLE"
cp device/extensions/sidle/config.xml   "$RES_DEVICE/extensions/sidle/config.xml"
cp device/extensions/sidle/menu.json    "$RES_DEVICE/extensions/sidle/menu.json"
cp device/extensions/sidle/bin/sidle.sh "$RES_DEVICE/extensions/sidle/bin/sidle.sh"
cp device/documents/Sidle.sh            "$RES_DEVICE/documents/Sidle.sh"
# The picker at the path it installs to, not off to one side: the packaged app
# walks this tree, and a binary anywhere else is a binary the walk cannot find.
cp "target/$DEVICE_TARGET/release/sidle-native" \
    "$RES_DEVICE/extensions/sidle/bin/sidle"
cp "target/$DEVICE_TARGET/release/sidle-native.build-ts" \
    "$RES_DEVICE/extensions/sidle/bin/sidle.build-ts"

# bokai is a second app in the same tree, built by build-bokai.sh — which this
# script deliberately never calls, so the engine and the product can ship on
# their own lines. Staged when a cross-build has left one, skipped when it has
# not; a packaged app then simply has no bokai to offer.
#
# $BOKAI_BIN is the hardfloat ABI, one of the two build-bokai.sh stages.
BOKAI_BIN="bokai-armhf"
if [ -f "device/extensions/bokai/bin/$BOKAI_BIN" ]; then
    mkdir -p "$RES_DEVICE/extensions/bokai/bin"
    cp "device/extensions/bokai/config.xml" "$RES_DEVICE/extensions/bokai/config.xml"
    cp "device/extensions/bokai/bin/$BOKAI_BIN" \
        "$RES_DEVICE/extensions/bokai/bin/$BOKAI_BIN"
else
    echo "==> no cross-built bokai in device/extensions/bokai — not staging it"
fi

echo "==> Building sidle desktop app"
# From inside the app dir. With `tauri.conf.json` in the cwd the CLI stops there
# and walks no further up the tree, leaving the build independent of what the
# directories are named. Config paths resolve against the config file, which
# holds `frontendDist: "../web"` and the bundle output under workspace `target/`.
(cd "$APP_DIR" && cargo tauri build)

echo "==> Installing to /Applications/Sidle.app"
SRC="target/release/bundle/macos/Sidle.app"
DST="/Applications/Sidle.app"
if [ ! -d "$SRC" ]; then
    echo "error: expected bundle not found at $SRC" >&2
    exit 1
fi

rm -rf "$DST"
ditto "$SRC" "$DST"

echo "==> Done."
