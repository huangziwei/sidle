#!/bin/sh
# Cross-compile bokai and stage device/extensions/bokai for a USB copy to
# /mnt/us/extensions/bokai/. `armhf` targets armv7-unknown-linux-musleabihf and
# stages bin/bokai; `armsf` targets armv7-unknown-linux-musleabi, bin/bokai-armsf.
#
# POSIX sh: this file runs on a GitHub runner and on a workstation.
set -eu

cd "$(dirname "$0")"

EXT="device/extensions/bokai"

ABI="${1:-armhf}"
case "$ABI" in
armhf)
    TARGET="armv7-unknown-linux-musleabihf"
    NAME="bokai"
    # WANT_FLOAT is the e_flags byte at 0x25: 04 hardfloat, 02 soft-float.
    WANT_FLOAT="04"
    ;;
armsf)
    TARGET="armv7-unknown-linux-musleabi"
    NAME="bokai-armsf"
    WANT_FLOAT="02"
    ;;
*)
    echo "usage: $0 [armhf|armsf]" >&2
    exit 1
    ;;
esac
OUT="$EXT/bin/$NAME"

# `rustup target list --installed` gates the build on $TARGET, ahead of cargo's
# "can't find core for armv7-..." panic.
if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "error: rustup target '$TARGET' is not installed" >&2
    echo "       fix: rustup target add $TARGET" >&2
    exit 1
fi

# VERSION is bokai/Cargo.toml's [package] version, which the binary carries as
# CARGO_PKG_VERSION. $EXT/config.xml is the one install file outside Cargo's
# reach and takes the same value here.
VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' bokai/Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "error: no version in bokai/Cargo.toml [package]" >&2; exit 1; }
# `-i.bak` + rm: BSD and GNU sed disagree about the bare `-i ''`.
sed -i.bak -E "s#<version>[^<]*</version>#<version>${VERSION}</version>#" "$EXT/config.xml"
rm -f "$EXT/config.xml.bak"

# BOKAI_BUILD reaches `bokai --version`, where the release workflow puts its
# tag. An unset value builds `dev`.
BUILD="${BOKAI_BUILD:-dev}"

echo "==> building bokai $VERSION (build $BUILD) for $TARGET"
# `--features native` is KFX<->EPUB without aozora, pdf and validate; `--profile
# device` is release plus fat LTO and panic = "abort".
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

# $OUT's own header decides: e_machine at 0x12 (2800 = EM_ARM) and the e_flags
# float byte at 0x25, against $WANT_FLOAT.
MACHINE="$(od -An -tx1 -j18 -N2 "$OUT" | tr -d ' \n')"
FLOAT="$(od -An -tx1 -j37 -N1 "$OUT" | tr -d ' \n')"
[ "$MACHINE" = "2800" ] || {
    echo "error: $OUT is not an ARM ELF (e_machine 0x$MACHINE) — check .cargo/config.toml" >&2
    exit 1
}
[ "$FLOAT" = "$WANT_FLOAT" ] || {
    echo "error: $OUT is not $ABI (ELF float ABI byte 0x$FLOAT, wanted 0x$WANT_FLOAT)" >&2
    exit 1
}

echo "==> staged $(ls -lh "$OUT" | awk '{print $5}') -> $OUT"
file "$OUT" 2>/dev/null || true

cat <<EOF

==> install — copy this onto the device

    $EXT/  ->  /mnt/us/extensions/bokai/

Then, over SSH:  /mnt/us/extensions/bokai/bin/$NAME --help
EOF
