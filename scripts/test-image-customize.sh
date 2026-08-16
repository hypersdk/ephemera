#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real-hardware regression test for `ephemera build-image`'s guestkit-based
# customization path (ephemera_image::customize_image) — every field of
# BuildImageRequest that mutates the guest filesystem: hostname, packages,
# commands, ssh_key, copy_in, enable_services. No libguestfs/virt-customize
# involved; this mounts the base image via qemu-nbd + chroot, exactly what
# `ephemera build-image` itself does.
#
# Proves:
# - `ephemera build-image` accepts a spec exercising every customization
#   field at once and exits 0
# - hostname is written into /etc/hostname
# - packages actually installs a real package via the guest's own package
#   manager (apt/dnf/tdnf/yum/pacman, auto-detected) — this needs working
#   DNS resolution from inside the chroot, which is not automatic (a stock
#   cloud image's /etc/resolv.conf is normally a dangling symlink); this
#   test would fail loudly if that staging/restore logic regresses
# - commands ran for real (a file it creates is verified to exist)
# - ssh_key lands in /root/.ssh/authorized_keys with 0600 perms
# - copy_in places the exact file contents at the requested destination
# - enable_services actually creates the systemd enablement symlink
# - the resolv.conf staged for package installs is cleaned up afterward,
#   not left behind in the output image
#
# Requires root (qemu-nbd mount) and a base image with a real package
# manager + systemd (the sample cloud images this project already ships
# with all qualify).
#
# Usage:
#   sudo ./scripts/test-image-customize.sh --image /var/lib/ephemera/images/ubuntu-noble.qcow2
#
# The base image's distro determines which package-manager branch of
# install_packages() actually runs (apt-get/dnf/tdnf/yum/pacman, detected
# live from the image). TEST_PACKAGE/TEST_SERVICE default to Debian/Ubuntu
# names (tree/cron) — override them for other families, e.g.:
#   TEST_SERVICE=crond sudo -E ./scripts/test-image-customize.sh --image rocky9.qcow2   # dnf
#   TEST_SERVICE=cronie sudo -E ./scripts/test-image-customize.sh --image arch.qcow2    # pacman
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

IMAGE=""
TEST_PACKAGE="${TEST_PACKAGE:-tree}"
TEST_SERVICE="${TEST_SERVICE:-cron}"

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || { echo "This test mounts a real disk image and requires Linux." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — qemu-nbd mount needs it." >&2; exit 1; }
[ -n "$IMAGE" ] && [ -f "$IMAGE" ] || { echo "--image is required and must exist" >&2; exit 1; }
command -v qemu-nbd >/dev/null 2>&1 || { echo "qemu-nbd not found on PATH" >&2; exit 1; }
modprobe nbd max_part=16 2>/dev/null || true

EPH="${EPHEMERA_BIN:-}"
if [ -z "$EPH" ]; then
    if [ -x "${PROJECT_DIR}/target/release/ephemera" ]; then
        EPH="${PROJECT_DIR}/target/release/ephemera"
    elif command -v ephemera >/dev/null 2>&1; then
        EPH="$(command -v ephemera)"
    else
        echo "ephemera binary not found. Build it (cargo build --release -p ephemera-cli) or set EPHEMERA_BIN." >&2
        exit 1
    fi
fi

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1" >&2; }
info() { echo "  [INFO] $1"; }
section() { echo ""; echo "=== $1 ==="; }

TMP="$(mktemp -d)"
NBD_DEV=""
MOUNT_DIR="${TMP}/mnt"
mkdir -p "$MOUNT_DIR"
cleanup() {
    mountpoint -q "$MOUNT_DIR" 2>/dev/null && umount "$MOUNT_DIR" 2>/dev/null || true
    [ -n "$NBD_DEV" ] && qemu-nbd -d "$NBD_DEV" >/dev/null 2>&1 || true
    rm -rf "$TMP"
}
trap cleanup EXIT

BASE_COPY="${TMP}/base.qcow2"
OUT_IMAGE="${TMP}/out.qcow2"
cp "$IMAGE" "$BASE_COPY"

SSH_KEY_PUB="ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINtestNotARealKeyUsedOnlyByThisRegressionTestXX test@ephemera-regression"
COPY_IN_SRC="${TMP}/copyfile.txt"
echo "ephemera build-image regression marker" > "$COPY_IN_SRC"

cat > "${TMP}/spec.json" <<JSON
{
  "source": "${BASE_COPY}",
  "output": "${OUT_IMAGE}",
  "format": "qcow2",
  "hostname": "ephemera-customize-test",
  "packages": ["${TEST_PACKAGE}"],
  "commands": ["touch /etc/ephemera-customize-test-marker"],
  "ssh_key": "${SSH_KEY_PUB}",
  "copy_in": [{"src": "${COPY_IN_SRC}", "dest": "/etc/ephemera-customize-test-copyfile.txt"}],
  "enable_services": ["${TEST_SERVICE}"]
}
JSON

section "build-image applies every customization field in one pass"
if "$EPH" build-image --spec "${TMP}/spec.json" > "${TMP}/build.log" 2>&1; then
    pass "ephemera build-image exited 0"
else
    fail "ephemera build-image failed"
    cat "${TMP}/build.log" >&2
    exit 1
fi
[ -f "$OUT_IMAGE" ] && pass "output image was created" || { fail "output image missing"; exit 1; }

section "mounting the output image to verify what actually landed on disk"
NBD_DEV="$(for i in 0 1 2 3 4 5 6 7; do d="/dev/nbd${i}"; [ -e "$d" ] && ! fuser "$d" >/dev/null 2>&1 && echo "$d" && break; done)"
[ -n "$NBD_DEV" ] || { echo "no free /dev/nbdN device found" >&2; exit 1; }
qemu-nbd -c "$NBD_DEV" "$OUT_IMAGE"
partprobe "$NBD_DEV" 2>/dev/null || true
udevadm settle --timeout=10 2>/dev/null || sleep 3
ROOT_PART="$(lsblk -no NAME -l "$NBD_DEV" | grep -v "^$(basename "$NBD_DEV")$" | while read -r p; do
    if mount -o ro "/dev/${p}" "$MOUNT_DIR" 2>/dev/null; then
        if [ -d "${MOUNT_DIR}/etc" ]; then echo "$p"; break; fi
        umount "$MOUNT_DIR" 2>/dev/null || true
    fi
done)"
mountpoint -q "$MOUNT_DIR" || { echo "could not find/mount the root partition on ${NBD_DEV}" >&2; exit 1; }
pass "mounted root partition /dev/${ROOT_PART}"

section "hostname"
if [ "$(cat "${MOUNT_DIR}/etc/hostname" 2>/dev/null)" = "ephemera-customize-test" ]; then
    pass "/etc/hostname was rewritten"
else
    fail "/etc/hostname does not match (got: $(cat "${MOUNT_DIR}/etc/hostname" 2>/dev/null || echo '<missing>'))"
fi

section "packages"
if [ -e "${MOUNT_DIR}/usr/bin/${TEST_PACKAGE}" ] || [ -e "${MOUNT_DIR}/usr/sbin/${TEST_PACKAGE}" ]; then
    pass "package '${TEST_PACKAGE}' was installed (this needed real DNS resolution to work)"
else
    fail "package '${TEST_PACKAGE}' was not found in the image"
fi

section "commands"
if [ -e "${MOUNT_DIR}/etc/ephemera-customize-test-marker" ]; then
    pass "commands entry actually ran"
else
    fail "command-created marker file is missing"
fi

section "copy_in"
if [ "$(cat "${MOUNT_DIR}/etc/ephemera-customize-test-copyfile.txt" 2>/dev/null)" = "ephemera build-image regression marker" ]; then
    pass "copy_in placed the exact file contents"
else
    fail "copy_in destination file missing or wrong content"
fi

section "ssh_key"
AUTH_KEYS="${MOUNT_DIR}/root/.ssh/authorized_keys"
if [ -f "$AUTH_KEYS" ] && grep -qF "$SSH_KEY_PUB" "$AUTH_KEYS"; then
    PERMS="$(stat -c '%a' "$AUTH_KEYS")"
    if [ "$PERMS" = "600" ]; then
        pass "ssh_key landed in authorized_keys with 0600 perms"
    else
        fail "authorized_keys has wrong perms (got ${PERMS}, want 600)"
    fi
else
    fail "ssh key not found in authorized_keys"
fi

section "enable_services"
if [ -L "${MOUNT_DIR}/etc/systemd/system/multi-user.target.wants/${TEST_SERVICE}.service" ]; then
    pass "${TEST_SERVICE}.service was enabled (multi-user.target.wants symlink present)"
else
    fail "${TEST_SERVICE}.service enablement symlink is missing"
fi

section "resolv.conf staging didn't leak into the output image"
if [ -e "${MOUNT_DIR}/etc/resolv.conf" ] && [ ! -L "${MOUNT_DIR}/etc/resolv.conf" ]; then
    fail "a real (non-symlink) /etc/resolv.conf was left behind — DNS staging cleanup regressed"
else
    pass "no leaked real /etc/resolv.conf in the output image"
fi

umount "$MOUNT_DIR"
qemu-nbd -d "$NBD_DEV"
NBD_DEV=""

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
