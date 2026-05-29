#!/bin/sh
# Build sidle desktop app + on-Kindle native picker, install to /Applications.
#
# Three cargo invocations, run sequentially from the workspace root:
#   1. Cross-compile sidle-native for the Kindle (armv7 musl static).
#   2. Build sidle-server (the LAN daemon the release app spawns as a detached
#      child — the app resolves it at target/release/sidle-server, and unlike the
#      debug build it does NOT build it on demand).
#   3. Build the Tauri desktop app for the host Mac.
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

echo "==> Building sidle desktop app"
cargo tauri build

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
