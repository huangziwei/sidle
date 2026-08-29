#!/bin/sh

# Which products a release covers, and whether a tag's claim holds. README.md
# documents the grammar; $GITHUB_OUTPUT, when set, takes the scope.
#   ./release-check.sh [<tag>]
set -eu

cd "$(dirname "$0")"

# device/extensions/bokai is bokai's, and SIDLE_PATHS excludes it. Cargo.lock,
# rust-toolchain.toml and .github belong to neither.
SIDLE_PATHS="sidle device build.sh :(exclude)device/extensions/bokai"
BOKAI_PATHS="bokai jxr build-bokai.sh device/extensions/bokai"

SIDLE_VERSION="$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0}
                      f && /^version *=/{gsub(/[" ]/,""); sub(/^version=/,""); print; exit}' Cargo.toml)"
BOKAI_VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' bokai/Cargo.toml | head -1)"
[ -n "$SIDLE_VERSION" ] || { echo "error: no version in Cargo.toml [workspace.package]" >&2; exit 1; }
[ -n "$BOKAI_VERSION" ] || { echo "error: no version in bokai/Cargo.toml [package]" >&2; exit 1; }

version_of() {
    case "$1" in
    sidle) echo "$SIDLE_VERSION" ;;
    bokai) echo "$BOKAI_VERSION" ;;
    esac
}

paths_of() {
    case "$1" in
    sidle) echo "$SIDLE_PATHS" ;;
    bokai) echo "$BOKAI_PATHS" ;;
    esac
}

manifest_of() {
    case "$1" in
    sidle) echo "Cargo.toml [workspace.package]" ;;
    bokai) echo "bokai/Cargo.toml [package]" ;;
    esac
}

# build.sh and build-bokai.sh stamp config.xml from the manifest at build time.
config_drift() {
    case "$1" in
    sidle) file="device/extensions/sidle/config.xml" ;;
    bokai) file="device/extensions/bokai/config.xml" ;;
    esac
    [ -f "$file" ] || return 0
    stamped="$(sed -n 's#.*<version>\([^<]*\)</version>.*#\1#p' "$file" | head -1)"
    want="$(version_of "$1")"
    if [ "$stamped" != "$want" ]; then
        echo "note: $file says $stamped, $(manifest_of "$1") says $want"
    fi
}

# Prints "<product> <version>" for each product $1 names; `+` joins two.
tag_claims() {
    printf '%s\n' "$1" | tr '+' '\n' | while IFS= read -r part; do
        case "$part" in
        bokai-v[0-9]*) echo "bokai ${part#bokai-v}" ;;
        sidle-v[0-9]*) echo "sidle ${part#sidle-v}" ;;
        v[0-9]*) echo "sidle ${part#v}" ;;
        *) echo "unknown $part" ;;
        esac
    done
}

# Prints "<version> <tag>" for the highest version any tag claims for $1.
last_tag() {
    product="$1"
    git tag --list 2>/dev/null | while IFS= read -r t; do
        tag_claims "$t" | while read -r p v; do
            if [ "$p" = "$product" ]; then
                printf '%s %s\n' "$v" "$t"
            fi
        done
    done | sort -V | tail -1
}

# Counts files under $2's paths between tag $1 and HEAD; `?` for an absent tag.
# paths_of splits unquoted, one word per path.
changed_files() {
    if ! git rev-parse -q --verify "refs/tags/$1^{commit}" >/dev/null 2>&1; then
        echo "?"
        return
    fi
    # shellcheck disable=SC2046
    git diff --name-only "$1..HEAD" -- $(paths_of "$2") | wc -l | tr -d ' '
}

# One line saying whether $1 needs a version bump, a tag, or neither.
verdict() {
    product="$1"
    version="$(version_of "$product")"
    last="$(last_tag "$product")"
    if [ -z "$last" ]; then
        echo "$product has never been tagged — tag $product-v$version to release it"
        return
    fi
    last_version="${last%% *}"
    last_name="${last#* }"
    n="$(changed_files "$last_name" "$product")"
    case "$n" in
    '?') echo "$product: $last_name is not in this clone, so nothing can be compared" ;;
    0) echo "$product has no change since $last_name — no release needed" ;;
    *)
        if [ "$version" = "$last_version" ]; then
            echo "$product has $n changed files since $last_name and is still at $version — bump it, then tag"
        else
            echo "$product has $n changed files since $last_name — release it as $product-v$version"
        fi
        ;;
    esac
}

# Counts files under $1's paths that no commit carries.
uncommitted() {
    # shellcheck disable=SC2046
    git status --porcelain -- $(paths_of "$1") 2>/dev/null | wc -l | tr -d ' '
}

# Prints $1's table line: manifest version, last tag, distance from it.
row() {
    product="$1"
    last="$(last_tag "$product")"
    if [ -z "$last" ]; then
        printf '%-6s %-8s %-16s %s\n' "$product" "$(version_of "$product")" "(none)" "everything"
        return
    fi
    last_name="${last#* }"
    n="$(changed_files "$last_name" "$product")"
    case "$n" in
    0) since="unchanged" ;;
    '?') since="unknown" ;;
    *) since="$n files" ;;
    esac
    dirty="$(uncommitted "$product")"
    [ "$dirty" = 0 ] || since="$since, $dirty uncommitted"
    printf '%-6s %-8s %-16s %s\n' "$product" "$(version_of "$product")" "$last_name" "$since"
}

TAG="${1:-}"

printf '%-6s %-8s %-16s %s\n' product version "last tag" "changed since"
row sidle
row bokai
config_drift sidle
config_drift bokai
echo

if [ -z "$TAG" ]; then
    verdict sidle
    verdict bokai
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
        {
            echo "products="
            echo "sidle_version=$SIDLE_VERSION"
            echo "bokai_version=$BOKAI_VERSION"
            echo "bokai_build=true"
            echo "bokai_check=true"
            echo "asset=bokai-v$BOKAI_VERSION-kindle"
        } >>"$GITHUB_OUTPUT"
    fi
    exit 0
fi

CLAIMS="$(tag_claims "$TAG")"
PRODUCTS=""
FAILED=0
OLD_IFS="$IFS"
IFS='
'
for claim in $CLAIMS; do
    product="${claim%% *}"
    claimed="${claim#* }"
    if [ "$product" = unknown ]; then
        echo "error: '$claimed' in tag '$TAG' names no product." >&2
        echo "       expected sidle-vX.Y.Z, bokai-vX.Y.Z, or the two joined by '+'." >&2
        FAILED=1
        continue
    fi
    want="$(version_of "$product")"
    if [ "$claimed" != "$want" ]; then
        echo "error: $TAG claims $product $claimed, and $(manifest_of "$product") holds $want" >&2
        FAILED=1
        continue
    fi
    PRODUCTS="$PRODUCTS $product"
    echo "$TAG releases $product $claimed"
done
IFS="$OLD_IFS"
[ "$FAILED" = 0 ] || exit 1

# verdict reports the product $TAG leaves out.
case " $PRODUCTS " in
*" sidle "*) ;;
*) verdict sidle ;;
esac
case " $PRODUCTS " in
*" bokai "*) ;;
*) verdict bokai ;;
esac

PRODUCTS="${PRODUCTS# }"
case " $PRODUCTS " in
*" bokai "*) BOKAI_BUILD=true ;;
*) BOKAI_BUILD=false ;;
esac
# BOKAI_CHECK covers a sidle-only release whose tree holds a changed bokai.
BOKAI_CHECK="$BOKAI_BUILD"
if [ "$BOKAI_CHECK" = false ]; then
    last="$(last_tag bokai)"
    if [ -n "$last" ] && [ "$(changed_files "${last#* }" bokai)" != 0 ]; then
        BOKAI_CHECK=true
    fi
fi

if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
        echo "products=$PRODUCTS"
        echo "sidle_version=$SIDLE_VERSION"
        echo "bokai_version=$BOKAI_VERSION"
        echo "bokai_build=$BOKAI_BUILD"
        echo "bokai_check=$BOKAI_CHECK"
        echo "asset=bokai-v$BOKAI_VERSION-kindle"
    } >>"$GITHUB_OUTPUT"
fi
