#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Install Cloud Hypervisor (+ optional Rust Hypervisor Firmware) from upstream
# GitHub releases, verified against the SHA-256 digest GitHub records for the
# release asset.
#
# Usage:
#   ./scripts/install-cloud-hypervisor.sh              # latest release
#   ./scripts/install-cloud-hypervisor.sh v53.0         # pinned version
#   ./scripts/install-cloud-hypervisor.sh --no-firmware # skip hypervisor-fw
#
# Env:
#   CLOUD_HYPERVISOR_VERSION   Pin a version (overridden by a positional arg)
#   INSTALL_DIR                Where to install the binary (default /usr/local/bin)
#   FIRMWARE_DIR                Where to install hypervisor-fw (default /usr/local/share/ephemera)
set -euo pipefail

REPO="cloud-hypervisor/cloud-hypervisor"
FW_REPO="cloud-hypervisor/rust-hypervisor-firmware"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
FIRMWARE_DIR="${FIRMWARE_DIR:-/usr/local/share/ephemera}"
VERSION="${CLOUD_HYPERVISOR_VERSION:-}"
WITH_FIRMWARE=true

for arg in "$@"; do
    case "$arg" in
        --no-firmware) WITH_FIRMWARE=false ;;
        -h|--help)
            sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) VERSION="$arg" ;;
    esac
done

info() { echo "  [*] $*"; }
ok()   { echo "  [ok] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || fail "Cloud Hypervisor only runs on Linux/KVM"
case "$(uname -m)" in
    x86_64)  ARCH="" ;;
    aarch64) ARCH="-aarch64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
esac

command -v curl >/dev/null || fail "curl is required"
SHA_CMD=""
command -v sha256sum >/dev/null && SHA_CMD="sha256sum" || { command -v shasum >/dev/null && SHA_CMD="shasum -a 256"; }
[ -n "$SHA_CMD" ] || fail "sha256sum or shasum is required"

SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

api() { curl -fsSL -H "Accept: application/vnd.github+json" "$@"; }

if [ -z "$VERSION" ]; then
    info "Resolving latest cloud-hypervisor release..."
    VERSION="$(api "https://api.github.com/repos/${REPO}/releases/latest" | python3 -c 'import json,sys;print(json.load(sys.stdin)["tag_name"])')"
fi
[ -n "$VERSION" ] || fail "could not resolve a cloud-hypervisor version"
info "Target version: ${VERSION}"

SKIP_CH_BINARY=false
if command -v cloud-hypervisor >/dev/null 2>&1; then
    CURRENT="$(cloud-hypervisor --version 2>/dev/null | awk '{print $2}')"
    if [ "$CURRENT" = "${VERSION#v}" ] || [ "v${CURRENT}" = "$VERSION" ]; then
        ok "cloud-hypervisor ${CURRENT} already installed"
        SKIP_CH_BINARY=true
    fi
fi

ASSET="cloud-hypervisor-static${ARCH}"
RELEASE_JSON="$(api "https://api.github.com/repos/${REPO}/releases/tags/${VERSION}")"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ "$SKIP_CH_BINARY" != true ]; then
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

    info "Downloading ${ASSET} (${VERSION})..."
    curl -fsSL -o "${TMP}/cloud-hypervisor" "$DOWNLOAD_URL"

    if [ -n "$DIGEST" ]; then
        GOT="$(${SHA_CMD} "${TMP}/cloud-hypervisor" | awk '{print $1}')"
        [ "$GOT" = "$DIGEST" ] || fail "checksum mismatch for ${ASSET}: expected ${DIGEST}, got ${GOT}"
        ok "checksum verified"
    else
        echo "  [WARN] GitHub did not report a digest for ${ASSET}; installing unverified" >&2
    fi

    chmod +x "${TMP}/cloud-hypervisor"
    $SUDO install -m755 "${TMP}/cloud-hypervisor" "${INSTALL_DIR}/cloud-hypervisor"
    ok "installed ${INSTALL_DIR}/cloud-hypervisor ($(${INSTALL_DIR}/cloud-hypervisor --version 2>/dev/null || echo "$VERSION"))"
fi

# ch-remote is a separate asset in the same release — needed for
# pause/resume/shutdown (ephemera shells out to it rather than talking to
# the API socket directly).
if command -v ch-remote >/dev/null 2>&1; then
    ok "ch-remote already installed"
else
    CH_REMOTE_ASSET="ch-remote-static${ARCH}"
    CH_REMOTE_URL="$(echo "$RELEASE_JSON" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '${CH_REMOTE_ASSET}':
        print(a['browser_download_url'])
        break
")"
    CH_REMOTE_DIGEST="$(echo "$RELEASE_JSON" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '${CH_REMOTE_ASSET}':
        print((a.get('digest') or '').removeprefix('sha256:'))
        break
")"
    if [ -n "$CH_REMOTE_URL" ]; then
        curl -fsSL -o "${TMP}/ch-remote" "$CH_REMOTE_URL"
        if [ -n "$CH_REMOTE_DIGEST" ]; then
            GOT="$(${SHA_CMD} "${TMP}/ch-remote" | awk '{print $1}')"
            [ "$GOT" = "$CH_REMOTE_DIGEST" ] || fail "checksum mismatch for ${CH_REMOTE_ASSET}: expected ${CH_REMOTE_DIGEST}, got ${GOT}"
            ok "ch-remote checksum verified"
        else
            echo "  [WARN] GitHub did not report a digest for ${CH_REMOTE_ASSET}; installing unverified" >&2
        fi
        chmod +x "${TMP}/ch-remote"
        $SUDO install -m755 "${TMP}/ch-remote" "${INSTALL_DIR}/ch-remote"
        ok "installed ${INSTALL_DIR}/ch-remote"
    else
        echo "  [WARN] could not find ${CH_REMOTE_ASSET} asset; pause/resume/shutdown will not work" >&2
    fi
fi

if [ "$WITH_FIRMWARE" = true ]; then
    info "Resolving latest rust-hypervisor-firmware release..."
    FW_JSON="$(api "https://api.github.com/repos/${FW_REPO}/releases/latest")"
    FW_ASSET="hypervisor-fw"
    [ "$ARCH" = "-aarch64" ] && FW_ASSET="hypervisor-fw-aarch64"
    FW_URL="$(echo "$FW_JSON" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '${FW_ASSET}':
        print(a['browser_download_url'])
        break
")"
    FW_DIGEST="$(echo "$FW_JSON" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '${FW_ASSET}':
        print((a.get('digest') or '').removeprefix('sha256:'))
        break
")"
    if [ -n "$FW_URL" ]; then
        curl -fsSL -o "${TMP}/hypervisor-fw" "$FW_URL"
        if [ -n "$FW_DIGEST" ]; then
            GOT="$(${SHA_CMD} "${TMP}/hypervisor-fw" | awk '{print $1}')"
            [ "$GOT" = "$FW_DIGEST" ] || fail "checksum mismatch for ${FW_ASSET}: expected ${FW_DIGEST}, got ${GOT}"
            ok "firmware checksum verified"
        else
            echo "  [WARN] GitHub did not report a digest for ${FW_ASSET}; installing unverified" >&2
        fi
        $SUDO install -d -m755 "${FIRMWARE_DIR}"
        $SUDO install -m644 "${TMP}/hypervisor-fw" "${FIRMWARE_DIR}/hypervisor-fw"
        ok "installed ${FIRMWARE_DIR}/hypervisor-fw"
        echo "  Set cloud_hypervisor_firmware = \"${FIRMWARE_DIR}/hypervisor-fw\" in your ephemera config to use firmware boot."
    else
        echo "  [WARN] could not find ${FW_ASSET} asset; skipping firmware install" >&2
    fi
fi
