#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real-hardware regression test for BackendKind::Auto resolution
# (ephemera_scheduler::resolve_backend). Boots actual VMs — it does not just
# assert on the pure function, which is already covered by unit tests in
# ephemera-scheduler/src/lib.rs.
#
# Cases:
#   A. backend=auto, no kernel in request, no default kernel/firmware in
#      config -> resolves to qemu (the only backend that boots from just a
#      disk image). Verified via vsock exec.
#   B. backend=auto, request supplies a kernel path, raw rootfs image
#      -> resolves to firecracker even with no config default. Verified via
#      vsock exec (UDS proxy path, not the QEMU native path A exercised).
#   C. backend=auto, request supplies NO kernel, but config's
#      firecracker_kernel default does -> resolves to firecracker from the
#      config default alone. Temporarily edits /etc/ephemera.toml and
#      restores it afterward (even on failure, via trap).
#
# Usage:
#   sudo ./scripts/test-auto-backend.sh \
#       --qemu-image /var/lib/ephemera/images/ephemera-lifecycle-test.qcow2 \
#       --fc-image /var/lib/ephemera/images/ubuntu-fc-root-agent.raw \
#       --fc-kernel /var/lib/ephemera/kernels/vmlinux-fc
#
# All three paths must already exist (built/extracted by earlier manual
# Firecracker validation — see README's Firecracker section for how to
# produce a compatible kernel + raw rootfs). This script does not build
# them, unlike test-lifecycle.sh's QEMU image bootstrap, since Firecracker
# kernel/rootfs prep is host-specific (see README).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

CONFIG="/etc/ephemera.toml"
QEMU_IMAGE=""
FC_IMAGE=""
FC_KERNEL=""

while [ $# -gt 0 ]; do
    case "$1" in
        --qemu-image) QEMU_IMAGE="$2"; shift 2 ;;
        --fc-image)   FC_IMAGE="$2"; shift 2 ;;
        --fc-kernel)  FC_KERNEL="$2"; shift 2 ;;
        --config)     CONFIG="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || { echo "This test boots real VMs and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — VM creation needs /var/lib/ephemera access." >&2; exit 1; }
[ -n "$QEMU_IMAGE" ] && [ -f "$QEMU_IMAGE" ] || { echo "--qemu-image is required and must exist" >&2; exit 1; }
[ -n "$FC_IMAGE" ] && [ -f "$FC_IMAGE" ] || { echo "--fc-image is required and must exist" >&2; exit 1; }
[ -n "$FC_KERNEL" ] && [ -f "$FC_KERNEL" ] || { echo "--fc-kernel is required and must exist" >&2; exit 1; }

EPH="${EPHEMERA_BIN:-}"
if [ -z "$EPH" ]; then
    if command -v ephemera >/dev/null 2>&1; then
        EPH="$(command -v ephemera)"
    elif [ -x "${PROJECT_DIR}/target/release/ephemera" ]; then
        EPH="${PROJECT_DIR}/target/release/ephemera"
    else
        echo "ephemera binary not found. Build it (cargo build --release -p ephemera-cli) or set EPHEMERA_BIN." >&2
        exit 1
    fi
fi
CFG_ARGS=()
[ -n "$CONFIG" ] && CFG_ARGS=(--config "$CONFIG")
eph() { "$EPH" "${CFG_ARGS[@]}" "$@"; }

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1" >&2; }
section() { echo ""; echo "=== $1 ==="; }

TMP="$(mktemp -d)"
CONFIG_BACKUP=""
cleanup() {
    rm -rf "$TMP"
    if [ -n "$CONFIG_BACKUP" ] && [ -f "$CONFIG_BACKUP" ]; then
        cp "$CONFIG_BACKUP" "$CONFIG"
    fi
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys;v=json.load(sys.stdin).get('$1');print(v if v is not None else '')"; }

wait_exec() {
    local id="$1" attempts="${2:-20}"
    for _ in $(seq 1 "$attempts"); do
        if eph exec "$id" -- echo ok >/dev/null 2>&1; then
            return 0
        fi
        sleep 4
    done
    return 1
}

create_and_check() {
    local label="$1" spec="$2" want_backend="$3"
    local out id backend attempts=20
    if ! out=$(eph create --spec "$spec" 2>&1); then
        fail "$label: create failed: $out"
        return 1
    fi
    id=$(echo "$out" | json_field id)
    backend=$(echo "$out" | json_field backend)
    if [ "$backend" != "$want_backend" ]; then
        fail "$label: resolved backend was '${backend}', expected '${want_backend}'"
        eph delete "$id" >/dev/null 2>&1 || true
        return 1
    fi
    pass "$label: auto resolved to ${backend}"
    # A raw rootfs extracted from a single GPT partition (see README's
    # Firecracker section) has no /boot/efi or other by-label fstab entries,
    # so systemd burns its ~90s local-fs.target timeout on every boot before
    # falling through to emergency mode — where the guest agent (no local-fs
    # dependency) is already up. Firecracker boots need a longer budget than
    # QEMU's for exactly this reason, not because anything is actually stuck.
    # Observed 93-140s from create-return to agent-reachable on the real
    # test host (raw-disk clone isn't reflink-able on this filesystem, so
    # the ~2.6G copy plus the ~90s local-fs.target timeout above both land
    # in this window) — 60 attempts gives real headroom over that.
    [ "$backend" = "firecracker" ] && attempts=60
    if wait_exec "$id" "$attempts"; then
        pass "$label: vsock exec reachable on the resolved backend"
    else
        fail "$label: guest agent never became reachable over vsock"
    fi
    eph stop "$id" >/dev/null 2>&1 || true
    eph delete "$id" >/dev/null 2>&1 || true
}

section "Case A: auto with no kernel/firmware anywhere -> qemu"
cat > "${TMP}/a.json" <<JSON
{
  "name": "ephemera-auto-a",
  "backend": "auto",
  "image": "${QEMU_IMAGE}",
  "vcpus": 1,
  "memory_mib": 768,
  "network": {"mode": "none"},
  "agent": {"enabled": true, "port": 17777},
  "ttl_seconds": 300
}
JSON
create_and_check "Case A" "${TMP}/a.json" "qemu"

section "Case B: auto with a request-level kernel -> firecracker"
cat > "${TMP}/b.json" <<JSON
{
  "name": "ephemera-auto-b",
  "backend": "auto",
  "image": "${FC_IMAGE}",
  "kernel": "${FC_KERNEL}",
  "vcpus": 1,
  "memory_mib": 768,
  "network": {"mode": "none"},
  "agent": {"enabled": true, "port": 17777},
  "ttl_seconds": 300
}
JSON
create_and_check "Case B" "${TMP}/b.json" "firecracker"

section "Case C: auto with only a config-level default kernel -> firecracker"
CONFIG_BACKUP="${TMP}/ephemera.toml.bak"
cp "$CONFIG" "$CONFIG_BACKUP"
if grep -q '^firecracker_kernel' "$CONFIG"; then
    sed -i "s@^firecracker_kernel.*@firecracker_kernel = \"${FC_KERNEL}\"@" "$CONFIG"
elif grep -q '^# *firecracker_kernel' "$CONFIG"; then
    sed -i "s@^# *firecracker_kernel.*@firecracker_kernel = \"${FC_KERNEL}\"@" "$CONFIG"
else
    echo "firecracker_kernel = \"${FC_KERNEL}\"" >> "$CONFIG"
fi
cat > "${TMP}/c.json" <<JSON
{
  "name": "ephemera-auto-c",
  "backend": "auto",
  "image": "${FC_IMAGE}",
  "vcpus": 1,
  "memory_mib": 768,
  "network": {"mode": "none"},
  "agent": {"enabled": true, "port": 17777},
  "ttl_seconds": 300
}
JSON
create_and_check "Case C" "${TMP}/c.json" "firecracker"
cp "$CONFIG_BACKUP" "$CONFIG"
CONFIG_BACKUP=""
pass "Case C: /etc/ephemera.toml restored"

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
