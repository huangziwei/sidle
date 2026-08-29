#!/bin/sh
# Build sidle desktop app + on-Kindle native picker, install to /Applications.
#
# Two cargo invocations, run sequentially from the workspace root:
#   1. Cross-compile sidle-native for the Kindle (armv7 musl static).
#   2. Build every host package: sidle-server, sidle-cli, sidle. The release app
#      loads sidle-server from inside its own bundle; the debug build is the one
#      that builds it on demand.
# The bundling step then wraps the sidle binary from 2 into the .app.
# Two things are staged under sidle/desktop before the bundling step, which
# folds them into the bundle. The installed .app then reaches back into this
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

# config.xml is the one release artifact outside Cargo's reach, and the app
# pushes it verbatim to the Kindle. $VERSION comes from
# [workspace.package].version, which every sidle-* crate takes as well.
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/^version *= *"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "error: no version found in root Cargo.toml [workspace.package]" >&2; exit 1; }
echo "==> Stamping config.xml version ($VERSION)"
sed -i '' -E "s#<version>[^<]*</version>#<version>${VERSION}</version>#" device/extensions/sidle/config.xml

# One $BUILD_TS per run reaches the picker through SIDLE_BUILD_TS and the
# server through the sidle.build-ts sidecar, which the LAN-update manifest
# carries as `built_at` — the value a device self-update compares against.
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

# One invocation for every host package: cargo resolves features once per
# invocation, and a dependency resolved two ways is a separate unit with its own
# hash. sidle-server is the LAN daemon; sidle-cli is bundled and symlinked.
echo "==> Building sidle-server, sidle-cli and the desktop binary"
cargo build --release -p sidle-server -p sidle-cli -p sidle

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
# sidle-cli is spawned by nobody and rides in so `cargo tauri build` signs it
# with everything else — the symlink at the end is what puts it on a PATH.
for host_bin in sidle-server sidle-cli; do
    cp "target/release/$host_bin" "$SIDECAR_DIR/$host_bin-$HOST_TRIPLE"
done
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

# build-bokai.sh builds $BOKAI_BIN, the hardfloat binary, on its own line.
# A cross-build that left one gets staged; a packaged app carrying none
# offers no bokai.
BOKAI_BIN="bokai"
if [ -f "device/extensions/bokai/bin/$BOKAI_BIN" ]; then
    mkdir -p "$RES_DEVICE/extensions/bokai/bin"
    cp "device/extensions/bokai/config.xml" "$RES_DEVICE/extensions/bokai/config.xml"
    cp "device/extensions/bokai/bin/$BOKAI_BIN" \
        "$RES_DEVICE/extensions/bokai/bin/$BOKAI_BIN"
else
    echo "==> no cross-built bokai in device/extensions/bokai — not staging it"
fi

echo "==> Building sidle desktop app"
# From $APP_DIR: `tauri.conf.json` in the cwd stops the CLI walking up the
# tree, and its own paths resolve against itself.
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

# The command line, on a PATH. The binary stays inside the bundle so it is
# replaced with the app and never drifts from the library code it opens; the
# symlink is the only thing outside it. First writable candidate wins.
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
