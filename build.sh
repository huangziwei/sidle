#!/bin/sh
# Build sidle desktop app + on-Kindle native picker, install to /Applications.
#
# Three cargo invocations, run sequentially from the workspace root:
#   1. Cross-compile sidle-native for the Kindle (armv7 musl static).
#   2. Build sidle-server (the LAN daemon the release app spawns as a detached
#      child). Unlike the debug build, the release app does NOT build it on
#      demand — it loads the sidecar from inside its own bundle.
#   3. Build the Tauri desktop app for the host Mac.
# Between 2 and 3 we stage two things under src-tauri so `cargo tauri build`
# folds them into the bundle and the installed .app is fully self-contained (no
# reach-back into this checkout at runtime):
#   - the sidle-server binary as a Tauri sidecar (-> Contents/MacOS/sidle-server)
#   - the KUAL extension files + armv7 picker as resources (-> Contents/Resources)
# Then ditto the bundle into /Applications, replacing any prior copy.
#
# Why a script and not `cargo tauri build`'s build.rs: nesting cargo
# inside cargo livelocks on the shared workspace lockfile. See
# .claude/plans/build-sh-script.md for the history.

set -eu

cd "$(dirname "$0")"

KUAL_TARGET="armv7-unknown-linux-musleabihf"

# Precheck: surface a one-liner if the cross target isn't installed,
# instead of cargo's opaque "can't find core for armv7-..." panic.
if ! rustup target list --installed | grep -qx "$KUAL_TARGET"; then
    echo "error: rustup target '$KUAL_TARGET' is not installed" >&2
    echo "       fix: rustup target add $KUAL_TARGET" >&2
    exit 1
fi

# Stamp the unified workspace version into the KUAL extension's config.xml —
# the one release artifact outside Cargo's reach. The desktop app pushes this
# file verbatim to the Kindle, and KUAL shows <version> on its info screen.
# Source of truth is [workspace.package].version in the root Cargo.toml; the
# sidle-* crates (incl. the on-device binary) inherit it via
# `version.workspace = true`, so this keeps the cosmetic XML in lockstep.
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/^version *= *"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "error: no version found in root Cargo.toml [workspace.package]" >&2; exit 1; }
echo "==> Stamping KUAL config.xml version ($VERSION)"
sed -i '' -E "s#<version>[^<]*</version>#<version>${VERSION}</version>#" kual/sidle/config.xml

echo "==> Cross-compiling sidle-native for Kindle ($KUAL_TARGET)"
cargo build --release --target "$KUAL_TARGET" -p sidle-native

echo "==> Building sidle-server (LAN daemon: app spawns it; sakabar + Kindle reach it)"
cargo build --release -p sidle-server

# Stage everything the bundle must carry so the installed .app runs standalone.
# Tauri names sidecars `<path>-<target-triple>` and strips the suffix when copying
# into Contents/MacOS, so host-native builds use the host triple. The KUAL resources
# mirror the `kual/sidle` layout KualSource::from_resource_root() expects under
# Contents/Resources/resources/kual — only the files pushed to the device, NOT the
# gitignored etc/server.conf (rendered per-device at install time).
HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$HOST_TRIPLE" ] || { echo "error: could not read host target triple from rustc -vV" >&2; exit 1; }

# Fetch the host-arch libpdfium for the PDF→KFX cover renderer. The worker
# renders page 1 via pdfium (boko `render`), which dlopen's libpdfium at runtime;
# we bundle it beside the app binary below so the installed app finds it (else
# PDF imports produce a cover-less KFX). pdfium is Chrome's engine (no pure-Rust
# renderer); the binary is from bblanchon/pdfium-binaries (Apache/BSD, loaded
# dynamically). Cached per-machine under vendor/ (gitignored); `latest` is always
# >= our `pdfium_7543` binding ABI (pdfium only adds symbols).
PDFIUM_CACHE="vendor/pdfium/$HOST_TRIPLE"
PDFIUM_LIB="$PDFIUM_CACHE/libpdfium.dylib"
if [ ! -f "$PDFIUM_LIB" ]; then
    case "$HOST_TRIPLE" in
        aarch64-apple-darwin) PDFIUM_ASSET="pdfium-mac-arm64.tgz" ;;
        x86_64-apple-darwin)  PDFIUM_ASSET="pdfium-mac-x64.tgz" ;;
        *) echo "error: no pdfium binary mapping for host '$HOST_TRIPLE'" >&2; exit 1 ;;
    esac
    echo "==> Fetching libpdfium ($PDFIUM_ASSET) for the cover renderer"
    mkdir -p "$PDFIUM_CACHE"
    curl -fsSL "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/$PDFIUM_ASSET" \
        | tar -xzf - -C "$PDFIUM_CACHE" --strip-components=1 lib/libpdfium.dylib
    [ -f "$PDFIUM_LIB" ] || { echo "error: libpdfium extract failed ($PDFIUM_LIB missing)" >&2; exit 1; }
fi

echo "==> Staging sidle-server sidecar ($HOST_TRIPLE) + KUAL resources for bundling"
SIDECAR_DIR="sidle/src-tauri/binaries"
RES_KUAL="sidle/src-tauri/resources/kual"
rm -rf "$SIDECAR_DIR" "$RES_KUAL"
mkdir -p "$SIDECAR_DIR" "$RES_KUAL/sidle/bin" "$RES_KUAL/native"
cp target/release/sidle-server "$SIDECAR_DIR/sidle-server-$HOST_TRIPLE"
cp kual/sidle/config.xml    "$RES_KUAL/sidle/config.xml"
cp kual/sidle/menu.json     "$RES_KUAL/sidle/menu.json"
cp kual/sidle/bin/sidle.sh  "$RES_KUAL/sidle/bin/sidle.sh"
cp kual/sidle/bin/update.sh "$RES_KUAL/sidle/bin/update.sh"
cp "target/$KUAL_TARGET/release/sidle" "$RES_KUAL/native/sidle"

echo "==> Building sidle desktop app"
cargo tauri build

echo "==> Installing to /Applications/Sidle.app"
SRC="target/release/bundle/macos/Sidle.app"
DST="/Applications/Sidle.app"
if [ ! -d "$SRC" ]; then
    echo "error: expected bundle not found at $SRC" >&2
    exit 1
fi

# Bundle libpdfium beside the app binary (Contents/MacOS) so boko's cover
# renderer finds it via its beside-exe search at runtime. Tauri has already
# signed the bundle, so ad-hoc-sign the added dylib and re-seal the bundle —
# otherwise dyld refuses to load an unsigned library into the signed process.
echo "==> Bundling libpdfium into the app + re-signing (ad-hoc)"
cp "$PDFIUM_LIB" "$SRC/Contents/MacOS/libpdfium.dylib"
codesign --force --sign - "$SRC/Contents/MacOS/libpdfium.dylib"
codesign --force --deep --sign - "$SRC"

rm -rf "$DST"
ditto "$SRC" "$DST"

echo "==> Done."
