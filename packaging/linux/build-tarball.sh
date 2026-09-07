#!/usr/bin/env bash
# Build a plain release tarball of wftui for Linux.
#
# Usage:
#   packaging/linux/build-tarball.sh                # native target, release profile
#   packaging/linux/build-tarball.sh --no-images     # cargo test's other supported build
#
# Output: dist/wftui-<version>-linux-<target>.tar.gz next to a .sha256 file,
# both under the repo root. No cross-compilation is attempted — this builds
# for whatever target the host running the script is (matches how bin/wftui
# is already built and committed).
set -euo pipefail

FEATURES_FLAG=()
SUFFIX=""
if [[ "${1:-}" == "--no-images" ]]; then
    FEATURES_FLAG=(--no-default-features)
    SUFFIX="-no-images"
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(grep -m1 '^version = ' wftui/Cargo.toml | sed -E 's/version = "(.*)"/\1/')"
TARGET_TRIPLE="$(rustc -vV | sed -n 's/host: //p')"
ARCH_TAG="${TARGET_TRIPLE%%-*}"

echo "wftui version: $VERSION"
echo "host target:   $TARGET_TRIPLE"

cargo build --release -p wftui "${FEATURES_FLAG[@]}"

PKG_NAME="wftui-${VERSION}-linux-${ARCH_TAG}${SUFFIX}"
STAGE_DIR="dist/${PKG_NAME}"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

cp "target/release/wftui" "$STAGE_DIR/wftui"
chmod 755 "$STAGE_DIR/wftui"

cat > "$STAGE_DIR/README.txt" <<EOF
WindowsForum TUI (wftui) ${VERSION} — linux-${ARCH_TAG}${SUFFIX}

Terminal client for windowsforum.com. Run ./wftui, or copy it onto your
PATH (e.g. /usr/local/bin/wftui).

Proprietary software — in development. Not for redistribution.
https://windowsforum.com
EOF

TARBALL="dist/${PKG_NAME}.tar.gz"
tar czf "$TARBALL" -C dist "$PKG_NAME"
( cd dist && sha256sum "$(basename "$TARBALL")" > "$(basename "$TARBALL").sha256" )

rm -rf "$STAGE_DIR"

echo "Built: $TARBALL"
echo "       ${TARBALL}.sha256"
