#!/bin/sh
# Cross-compile bokai for the Kindle and stage the extension a USB copy installs.
#
#   ./build-bokai.sh
#
# Output, ready to drag onto the Kindle's USB volume:
#
#   device/extensions/bokai/  ->  /mnt/us/extensions/bokai/
#
# **This is bokai's build, not sidle's.** The desktop app links bokai as a
# library and wants nothing from here, so `build.sh` next door never calls this
# one and a desktop build does not pay for a cross-compile it cannot use. The
# two run independently, which is also what lets the two ship independently.
#
# The Kindle build is the `native` feature: KFX<->EPUB and the subcommands over
# it, with `aozora`, `pdf` and `validate` absent — bokai/Cargo.toml says why for
# each. `--profile device` is release plus fat LTO and `panic = "abort"`.
#
# One armv7 musl binary covers the fleet: the KOA2, Colorsoft and Scribe all run
# a 32-bit armv7 userspace, and static linking makes it firmware-agnostic. No C
# toolchain is needed, because the `native` feature graph is pure Rust — the C
# dependencies (resvg, lopdf, rusqlite) all sit behind features it does not
# take. rustc's bundled rust-lld links it; see .cargo/config.toml, which also
# names the one-time symlink that puts rust-lld on PATH.
#
# Portable sh, unlike build.sh: a GitHub runner runs this file too, and the
# release artifact has to be the same tree a workstation assembles.
set -eu

cd "$(dirname "$0")"

TARGET="armv7-unknown-linux-musleabihf"
EXT="device/extensions/bokai"
OUT="$EXT/bin/bokai"

# Name the missing cross target in one line, ahead of cargo's opaque
# "can't find core for armv7-..." panic.
if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "error: rustup target '$TARGET' is not installed" >&2
    echo "       fix: rustup target add $TARGET" >&2
    exit 1
fi

# bokai versions on its own line in bokai/Cargo.toml, deliberately outside the
# root [workspace.package] the sidle-* crates share: the engine and the product
# move at different rates, and this script exists so the engine can move alone.
# The binary takes it through CARGO_PKG_VERSION; the copy here stamps the KUAL
# metadata beside it, which is the one install file outside Cargo's reach.
VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' bokai/Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "error: no version in bokai/Cargo.toml [package]" >&2; exit 1; }
# `-i.bak` + rm, not `-i ''`: BSD and GNU sed disagree about the bare form and
# this file has to run on both.
sed -i.bak -E "s#<version>[^<]*</version>#<version>${VERSION}</version>#" "$EXT/config.xml"
rm -f "$EXT/config.xml.bak"
# The other install file outside Cargo's reach: what a desktop installer reads
# to name the version it is about to push. bokai has no tile and no menu, so
# this is the only place the device says which build it carries.
sed -i.bak -E "s#\"version\": \"[^\"]*\"#\"version\": \"${VERSION}\"#" "$EXT/app.json"
rm -f "$EXT/app.json.bak"

# The stamp that tells two builds of one version apart. bokai's version stands
# still across most sidle releases — that is the point of releasing it attached
# to them — so `bokai 0.1.2` on its own does not name what a device is carrying.
# The release workflow passes the release tag in here and `--version` prints it.
# An inherited value wins; a local build says `dev`.
BUILD="${BOKAI_BUILD:-dev}"

echo "==> building bokai $VERSION (build $BUILD) for $TARGET"
BOKAI_BUILD="$BUILD" cargo build \
    --profile device \
    --target "$TARGET" \
    -p bokai \
    --no-default-features \
    --features native \
    --bin bokai

mkdir -p "$(dirname "$OUT")"
cp "target/$TARGET/device/bokai" "$OUT"
chmod +x "$OUT"

# Catch a cross path that quietly produced a host binary. It would run here and
# do nothing on the device, and nothing else in the build would notice.
if command -v file >/dev/null 2>&1; then
    case "$(file -b "$OUT")" in
    *ARM*) ;;
    *) echo "warning: $OUT is not an ARM binary — check .cargo/config.toml" >&2 ;;
    esac
fi

echo "==> staged $(ls -lh "$OUT" | awk '{print $5}') -> $OUT"
file "$OUT" 2>/dev/null || true

cat <<'EOF'

==> install — copy this onto the device

    device/extensions/bokai/  ->  /mnt/us/extensions/bokai/

Then, over SSH:  /mnt/us/extensions/bokai/bin/bokai --help
EOF
