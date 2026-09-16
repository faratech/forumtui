#!/bin/sh
# install.sh — install Forum Terminal (TUI) (wftui) from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/faratech/forumtui/main/packaging/install.sh | sh
#   sh install.sh                     # latest release, this user
#   WFTUI_EDITION=xf sh install.sh    # Terminal for XenForo (no built-in site)
#   WFTUI_INSTALL_DIR=/opt/bin sh install.sh
#
# Detects OS and CPU (Linux x86_64 / aarch64 today; macOS is not built yet
# and says so), downloads the release's bare binary and SHA256SUMS.txt,
# verifies the digest the release published, and installs with a rename so
# a running wftui is never replaced mid-file (the same contract its own
# self-updater keeps, `common/src/update.rs`).
#
# Windows: use packaging/install.ps1, or the signed MSIX, which is the
# recommended install there.
set -eu

REPO=faratech/forumtui
FEED="${WFTUI_UPDATE_URL:-https://api.github.com/repos/$REPO/releases/latest}"

# Which edition: "" is Forum Terminal (TUI) (the built-in site); "xf" is
# Terminal for XenForo — the same suffix the release assets and the
# self-updater use to keep the two editions' binaries apart.
case "${WFTUI_EDITION:-}" in
    "")  EDITION_SUFFIX="" ;;
    xf)  EDITION_SUFFIX="-xf" ;;
    *)   echo "install.sh: WFTUI_EDITION must be empty or 'xf'" >&2; exit 2 ;;
esac

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# ---- fetch -----------------------------------------------------------------
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" || return 1
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "$1" || return 1
    else
        die "need curl or wget to download a release"
    fi
}
fetch_to() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$2" "$1" || return 1
    else
        wget -qO "$2" "$1" || return 1
    fi
}

# ---- what are we running on ------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS-$ARCH" in
    Linux-x86_64 | Linux-amd64)  TARGET="linux-x86_64" ;;
    Linux-aarch64 | Linux-arm64) TARGET="linux-aarch64" ;;
    Darwin-*) die "macOS is not built yet: releases carry linux-x86_64, \
linux-aarch64 and windows-x64/x86/arm64. A Darwin build is welcome \
to file an issue at https://github.com/$REPO/issues" ;;
    *) die "unsupported platform $OS-$ARCH (releases carry linux-x86_64, \
linux-aarch64, windows-x64/x86/arm64)" ;;
esac

# ---- pick the release ------------------------------------------------------
# The repo has no release yet until the first one ships; the feed 404s and
# both fetch paths fail — say what that means rather than a bare curl error.
feed="$(fetch "$FEED")" || die "cannot reach the release feed ($FEED).
If it answered 404: no release has been published yet. Once one ships,
this script finds it. (Set WFTUI_UPDATE_URL to test against another feed.)"

TAG="$(printf '%s\n' "$feed" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
[ -n "$TAG" ] || die "could not read tag_name from the release feed"
VERSION="${TAG#v}"

ASSET="wftui${EDITION_SUFFIX}-${VERSION}-${TARGET}"
SUMS_ASSET="SHA256SUMS.txt"

BASE="https://github.com/$REPO/releases/download/$TAG"
# Prefer the feed's own download URL for the asset (what the client's
# `pick_assets` does); fall back to the canonical release URL.
URL="$(printf '%s\n' "$feed" |
    sed -n 's/.*"browser_download_url": *"\([^"]*\)".*/\1/p' |
    { grep -E "/$ASSET\$" || true; } | head -n 1)"
URL="${URL:-$BASE/$ASSET}"
SUMS_URL="$(printf '%s\n' "$feed" |
    sed -n 's/.*"browser_download_url": *"\([^"]*\)".*/\1/p' |
    { grep -E "/$SUMS_ASSET\$" || true; } | head -n 1)"
SUMS_URL="${SUMS_URL:-$BASE/$SUMS_ASSET}"

# ---- download and verify ---------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM
say "fetching $ASSET ..."
fetch_to "$URL" "$TMP/$ASSET" || die "download failed: $URL"
fetch_to "$SUMS_URL" "$TMP/$SUMS_ASSET" || die "download failed: $SUMS_ASSET"

# `sha256sum` format: "<digest>  <name>", two spaces. The release writes one
# line per asset; match this asset's line exactly, as the client does.
WANT="$(sed -n "s/^\([0-9a-fA-F]\{64\}\)  $ASSET\$/\1/p" "$TMP/$SUMS_ASSET" | head -n 1)"
[ -n "$WANT" ] || die "$SUMS_ASSET has no line for $ASSET"

if command -v sha256sum >/dev/null 2>&1; then
    GOT="$(sha256sum "$TMP/$ASSET" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
    GOT="$(shasum -a 256 "$TMP/$ASSET" | cut -d' ' -f1)"
elif command -v openssl >/dev/null 2>&1; then
    GOT="$(openssl dgst -sha256 -r "$TMP/$ASSET" | cut -d' ' -f1)"
else
    die "need sha256sum, shasum or openssl to verify the download"
fi
[ "$GOT" = "$(printf '%s' "$WANT" | tr 'A-F' 'a-f')" ] ||
    die "sha256 mismatch for $ASSET: got $GOT, release says $WANT"
say "verified  $GOT"

# ---- install ---------------------------------------------------------------
# Install-then-rename, never a plain copy over a live binary: the rename
# swaps the path atomically and leaves a running wftui on its old inode,
# exactly how its self-update applies itself.
if [ -n "${WFTUI_INSTALL_DIR:-}" ]; then
    INSTALL_DIR="$WFTUI_INSTALL_DIR"
elif [ -w /usr/local/bin ] 2>/dev/null; then
    INSTALL_DIR=/usr/local/bin
elif mkdir -p "$HOME/.local/bin" 2>/dev/null && [ -w "$HOME/.local/bin" ]; then
    INSTALL_DIR="$HOME/.local/bin"
else
    die "cannot write to /usr/local/bin or \$HOME/.local/bin; \
set WFTUI_INSTALL_DIR, or run with sudo: sudo WFTUI_INSTALL_DIR=/usr/local/bin sh \$0"
fi
mkdir -p "$INSTALL_DIR"

NEW="$INSTALL_DIR/.wftui.new.$$"
cp "$TMP/$ASSET" "$NEW"
chmod 755 "$NEW"
mv -f "$NEW" "$INSTALL_DIR/wftui"

say "installed $INSTALL_DIR/wftui ($TAG)"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "note: $INSTALL_DIR is not in your PATH" ;;
esac
say "run it:   wftui"
