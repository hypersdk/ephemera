#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Install Firecracker (+ jailer) from upstream GitHub releases, verified
# against the SHA-256 digest GitHub records for the release asset.
#
# Usage:
#   ./scripts/install-firecracker.sh          # latest release
#   ./scripts/install-firecracker.sh v1.16.1  # pinned version
#
# Env:
#   FIRECRACKER_VERSION   Pin a version (overridden by a positional arg)
#   INSTALL_DIR           Where to install binaries (default /usr/local/bin)
set -euo pipefail

REPO="firecracker-microvm/firecracker"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
VERSION="${FIRECRACKER_VERSION:-${1:-}}"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
fi

info() { echo "  [*] $*"; }
ok()   { echo "  [ok] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || fail "Firecracker only runs on Linux/KVM"
case "$(uname -m)" in
    x86_64)  ARCH="x86_64" ;;
    aarch64) ARCH="aarch64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
esac

command -v curl >/dev/null || fail "curl is required"
command -v tar >/dev/null || fail "tar is required"
SHA_CMD=""
command -v sha256sum >/dev/null && SHA_CMD="sha256sum" || { command -v shasum >/dev/null && SHA_CMD="shasum -a 256"; }
[ -n "$SHA_CMD" ] || fail "sha256sum or shasum is required"

SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

api() { curl -fsSL -H "Accept: application/vnd.github+json" "$@"; }

if [ -z "$VERSION" ]; then
    info "Resolving latest firecracker release..."
    VERSION="$(api "https://api.github.com/repos/${REPO}/releases/latest" | python3 -c 'import json,sys;print(json.load(sys.stdin)["tag_name"])')"
fi
[ -n "$VERSION" ] || fail "could not resolve a firecracker version"
info "Target version: ${VERSION}"

if command -v firecracker >/dev/null 2>&1; then
    CURRENT="$(firecracker --version 2>/dev/null | awk '{print $2}')"
    if [ "v${CURRENT}" = "$VERSION" ]; then
        ok "firecracker ${CURRENT} already installed"
        exit 0
    fi
fi

ASSET="firecracker-${VERSION}-${ARCH}.tgz"
RELEASE_JSON="$(api "https://api.github.com/repos/${REPO}/releases/tags/${VERSION}")"
DOWNLOAD_URL="$(echo "$RELEASE_JSON" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '${ASSET}':
        print(a['browser_download_url'])
        break
")"
DIGEST="$(echo "$RELEASE_JSON" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '${ASSET}':
        print((a.get('digest') or '').removeprefix('sha256:'))
        break
")"
[ -n "$DOWNLOAD_URL" ] || fail "asset ${ASSET} not found in release ${VERSION}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
info "Downloading ${ASSET}..."
curl -fsSL -o "${TMP}/${ASSET}" "$DOWNLOAD_URL"

if [ -n "$DIGEST" ]; then
    GOT="$(${SHA_CMD} "${TMP}/${ASSET}" | awk '{print $1}')"
    [ "$GOT" = "$DIGEST" ] || fail "checksum mismatch for ${ASSET}: expected ${DIGEST}, got ${GOT}"
    ok "checksum verified"
else
    echo "  [WARN] GitHub did not report a digest for ${ASSET}; installing unverified" >&2
fi

tar -xzf "${TMP}/${ASSET}" -C "$TMP"
RELDIR="${TMP}/release-${VERSION}-${ARCH}"
[ -d "$RELDIR" ] || RELDIR="$(find "$TMP" -maxdepth 1 -type d -name 'release-*' | head -1)"
[ -d "$RELDIR" ] || fail "unexpected archive layout"

$SUDO install -m755 "${RELDIR}/firecracker-${VERSION}-${ARCH}" "${INSTALL_DIR}/firecracker"
ok "installed ${INSTALL_DIR}/firecracker"

if [ -f "${RELDIR}/jailer-${VERSION}-${ARCH}" ]; then
    $SUDO install -m755 "${RELDIR}/jailer-${VERSION}-${ARCH}" "${INSTALL_DIR}/jailer"
    ok "installed ${INSTALL_DIR}/jailer"
fi

"${INSTALL_DIR}/firecracker" --version 2>/dev/null || true
