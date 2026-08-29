#!/bin/sh
# Build sidle desktop app + on-Kindle native picker, install to /Applications.
# Each cargo invocation below takes its own target directory, leaving target/ to
# the bundling step. The installed .app reads nothing from this checkout.

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

# config.xml is the one release artifact outside Cargo's reach, and the app
# pushes it verbatim to the Kindle. $VERSION comes from
# [workspace.package].version, which every sidle-* crate takes as well.
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/^version *= *"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "error: no version found in root Cargo.toml [workspace.package]" >&2; exit 1; }
CONFIG_XML="device/extensions/sidle/config.xml"
if grep -q "<version>${VERSION}</version>" "$CONFIG_XML"; then
    echo "==> config.xml already at $VERSION"
else
    echo "==> Stamping config.xml version ($VERSION)"
    sed -i '' -E "s#<version>[^<]*</version>#<version>${VERSION}</version>#" "$CONFIG_XML"
fi

# $BUILD_TS reaches the picker through SIDLE_BUILD_TS and the LAN-update manifest
# as `built_at`, which a device self-update compares against. Its value is the
# newest mtime among the files below, sidle-native's whole input.
BUILD_TS="$(find sidle/native/src sidle/native/Cargo.toml sidle/native/build.rs \
    Cargo.toml Cargo.lock rust-toolchain.toml .cargo/config.toml \
    -type f -exec stat -f '%m' {} + 2>/dev/null | sort -rn | head -1)"
case "$BUILD_TS" in
    ''|*[!0-9]*) BUILD_TS="$(date +%s)" ;;
esac
echo "==> Cross-compiling sidle-native for Kindle ($DEVICE_TARGET)  [build_ts=$BUILD_TS]"
KINDLE_TARGET="target/kindle"
SIDLE_BUILD_TS="$BUILD_TS" CARGO_TARGET_DIR="$KINDLE_TARGET" \
    cargo build --release --target "$DEVICE_TARGET" -p sidle-native
# The build time next to the binary DeploySource points at on the dev path.
# build.rs bakes the same value into the binary.
STAMP_FILE="$KINDLE_TARGET/$DEVICE_TARGET/release/sidle-native.build-ts"
if [ "$(cat "$STAMP_FILE" 2>/dev/null)" != "$BUILD_TS" ]; then
    printf '%s' "$BUILD_TS" > "$STAMP_FILE"
fi

# The cross-built picker at the path it installs to, completing the `device/`
# mount mirror. Both files are gitignored build products.
mkdir -p device/extensions/sidle/bin
cp -p "$KINDLE_TARGET/$DEVICE_TARGET/release/sidle-native" device/extensions/sidle/bin/sidle
cp -p "$KINDLE_TARGET/$DEVICE_TARGET/release/sidle-native.build-ts" \
    device/extensions/sidle/bin/sidle.build-ts

# $AUX_TARGET holds one feature resolution: these two alongside the app resolve
# sha2, digest, rustls, subtle and chacha20 differently, and a dependency
# resolved two ways is a separate unit with its own hash.
AUX_TARGET="target/aux"
echo "==> Building sidle-server and sidle-cli ($AUX_TARGET)"
CARGO_TARGET_DIR="$AUX_TARGET" cargo build --release -p sidle-server -p sidle-cli

# Tauri names a sidecar `<path>-$HOST_TRIPLE` and strips the suffix copying it
# into Contents/MacOS. $RES_DEVICE mirrors `device/` under
# Contents/Resources, each file at the path it installs to.
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
# Both host binaries stage the same way. sidle-server is spawned by the app;
# sidle-cli is spawned by nobody and rides in for signing, and the symlink at the
# end puts it on a PATH.
for host_bin in sidle-server sidle-cli; do
    cp -p "$AUX_TARGET/release/$host_bin" "$SIDECAR_DIR/$host_bin-$HOST_TRIPLE"
done
cp -p device/extensions/sidle/config.xml   "$RES_DEVICE/extensions/sidle/config.xml"
cp -p device/extensions/sidle/menu.json    "$RES_DEVICE/extensions/sidle/menu.json"
cp -p device/extensions/sidle/bin/sidle.sh "$RES_DEVICE/extensions/sidle/bin/sidle.sh"
cp -p device/documents/Sidle.sh            "$RES_DEVICE/documents/Sidle.sh"
# The picker at the path it installs to. The packaged app walks this tree.
cp -p "$KINDLE_TARGET/$DEVICE_TARGET/release/sidle-native" \
    "$RES_DEVICE/extensions/sidle/bin/sidle"
cp -p "$KINDLE_TARGET/$DEVICE_TARGET/release/sidle-native.build-ts" \
    "$RES_DEVICE/extensions/sidle/bin/sidle.build-ts"

# build-bokai.sh builds $BOKAI_BIN, the hardfloat binary, on its own line.
# A cross-build that left one gets staged; a packaged app carrying none
# offers no bokai.
BOKAI_BIN="bokai"
if [ -f "device/extensions/bokai/bin/$BOKAI_BIN" ]; then
    mkdir -p "$RES_DEVICE/extensions/bokai/bin"
    cp -p "device/extensions/bokai/config.xml" "$RES_DEVICE/extensions/bokai/config.xml"
    cp -p "device/extensions/bokai/bin/$BOKAI_BIN" \
        "$RES_DEVICE/extensions/bokai/bin/$BOKAI_BIN"
else
    echo "==> no cross-built bokai in device/extensions/bokai — not staging it"
fi

echo "==> Building sidle desktop app"
# From $APP_DIR: `tauri.conf.json` in the cwd stops the CLI walking up the tree.
# Builds the app in target/ and folds in the staged sidecars and resources.
# Cargo nested inside cargo livelocks on the shared workspace lockfile.
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

# The command line, on a PATH. The binary stays inside the bundle, replaced with
# the app; the symlink is the only thing outside it. First writable candidate
# wins.
CLI="$DST/Contents/MacOS/sidle-cli"
echo "==> Linking sidle-cli"
linked=""
for dir in /usr/local/bin "$HOME/.local/bin"; do
    [ -d "$dir" ] || mkdir -p "$dir" 2>/dev/null || continue
    if ln -sfn "$CLI" "$dir/sidle-cli" 2>/dev/null; then
        linked="$dir/sidle-cli"
        break
    fi
done
if [ -n "$linked" ]; then
    echo "    $linked -> $CLI"
    case ":$PATH:" in
        *":${linked%/*}:"*) ;;
        *) echo "    note: ${linked%/*} is not on this shell's PATH" >&2 ;;
    esac
else
    echo "    no writable directory found; link it by hand:" >&2
    echo "      sudo ln -sfn '$CLI' /usr/local/bin/sidle-cli" >&2
fi

echo "==> Done."
