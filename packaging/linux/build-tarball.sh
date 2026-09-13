#!/usr/bin/env bash
# Build a plain release tarball of wftui for Linux.
#
# Usage:
#   packaging/linux/build-tarball.sh                # WindowsForum Terminal, native target
#   packaging/linux/build-tarball.sh --no-images     # cargo test's other supported build
#   packaging/linux/build-tarball.sh --generic       # Terminal for XenForo (no built-in site)
#
# Output: dist/wftui-<version>-linux-<target>.tar.gz next to a .sha256 file,
# both under the repo root — plus, for the default build, the bare binary
# dist/wftui-<version>-linux-<target> that the self-updater fetches. No cross-compilation is attempted — this builds
# for whatever target the host running the script is (matches how bin/wftui
# is already built and committed).
set -euo pipefail

FEATURES_FLAG=()
SUFFIX=""
# The generic edition's assets carry `-xf` in their *prefix*
# (`wftui-xf-<ver>-…`), which is what the self-updater looks for
# (`common::site::EDITION_SUFFIX`); the no-images tier is a suffix.
EDITION=""
PRODUCT="WindowsForum Terminal"
BLURB="Terminal client for windowsforum.com."
HOMEPAGE="https://windowsforum.com"
case "${1:-}" in
    --no-images)
        FEATURES_FLAG=(--no-default-features --features builtin-windowsforum)
        SUFFIX="-no-images"
        ;;
    --generic)
        FEATURES_FLAG=(--no-default-features --features images)
        EDITION="-xf"
        PRODUCT="Terminal for XenForo"
        BLURB="Terminal client for XenForo 2.3 forums. On first run it asks for the forum's address and OAuth client ID; see docs/CONFIG.md in the project repository."
        HOMEPAGE="https://github.com/faratech/wftui"
        ;;
esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(grep -m1 '^version = ' wftui/Cargo.toml | sed -E 's/version = "(.*)"/\1/')"
TARGET_TRIPLE="$(rustc -vV | sed -n 's/host: //p')"
ARCH_TAG="${TARGET_TRIPLE%%-*}"

echo "wftui version: $VERSION"
echo "host target:   $TARGET_TRIPLE"

cargo build --release -p wftui "${FEATURES_FLAG[@]}"

PKG_NAME="wftui${EDITION}-${VERSION}-linux-${ARCH_TAG}${SUFFIX}"
STAGE_DIR="dist/${PKG_NAME}"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

cp "target/release/wftui" "$STAGE_DIR/wftui"
chmod 755 "$STAGE_DIR/wftui"

cat > "$STAGE_DIR/README.txt" <<EOF
${PRODUCT} (wftui) ${VERSION} — linux-${ARCH_TAG}${SUFFIX}

${BLURB} Run ./wftui, or copy it onto your
PATH (e.g. /usr/local/bin/wftui).

Proprietary software — in development. Not for redistribution.
${HOMEPAGE}
EOF

TARBALL="dist/${PKG_NAME}.tar.gz"
tar czf "$TARBALL" -C dist "$PKG_NAME"
( cd dist && sha256sum "$(basename "$TARBALL")" > "$(basename "$TARBALL").sha256" )

rm -rf "$STAGE_DIR"

echo "Built: $TARBALL"
echo "       ${TARBALL}.sha256"

# The bare binary is a release asset too: the client's self-updater
# (`common::update`) downloads exactly this file, by exactly this name
# (`Target::asset_name`), and verifies it against the release's
# SHA256SUMS.txt. Only the default-features build — the no-images tier is
# a deliberate choice, not something an update should swap someone into.
if [[ -z "$SUFFIX" ]]; then
    BARE="dist/wftui${EDITION}-${VERSION}-linux-${ARCH_TAG}"
    install -m755 "target/release/wftui" "$BARE"
    ( cd dist && sha256sum "$(basename "$BARE")" > "$(basename "$BARE").sha256" )
    echo "       $BARE"
fi
