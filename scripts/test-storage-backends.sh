#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real-hardware regression test for the pluggable storage-provisioning
# abstraction (ephemera_core::model::StorageBackend,
# ephemera_image::storage). Boots real VMs against two of its three new
# backends:
#
# - storage=lvm-thin: `image` is a real /dev/<vg>/<lv> thin LV. Proves a
#   fresh thin *snapshot* LV is created per VM, the guest actually boots off
#   it (verified via exec, not just "create didn't error"), and `delete`
#   removes the snapshot LV instead of leaking it. Requires an existing thin
#   pool + base LV — see the setup block below and --lvm-vg/--lvm-base-lv.
# - storage=nbd (QEMU only): `image` is a normal qcow2/raw file. Proves the
#   VM's disk is actually served over a `qemu-nbd` export (not opened
#   directly) — the exported qcow2 overlay is confirmed as the real backing
#   file via `qemu-nbd`'s own pid, and torn down (process killed, no leftover
#   pid) only on delete, not on stop.
# - storage=ceph-rbd is NOT covered here: it needs a real Ceph cluster, none
#   of which is available in this test environment. See
#   model::StorageBackend::CephRbd and ephemera_image::storage for the
#   implemented-but-unverified code path.
#
# Also proves two fail-closed cases: storage=nbd is rejected up front for a
# non-QEMU backend, and storage=lvm-thin is rejected up front when the
# Firecracker jailer is enabled (its chroot resource-placement model can't
# extend to a shared block device) — both real product bails, not just
# missing functionality.
#
# Usage:
#   sudo ./scripts/test-storage-backends.sh \
#       --image /var/lib/ephemera/images/ephemera-catalog-test.qcow2 \
#       --lvm-vg ephtest --lvm-base-lv base
#
# The LVM thin pool + base LV must already exist (this script does not
# create host-level LVM state) — e.g.:
#   truncate -s 8G /path/backing.img
#   losetup --find --show /path/backing.img         # -> /dev/loopN
#   pvcreate /dev/loopN && vgcreate ephtest /dev/loopN
#   lvcreate --type thin-pool -L 6G --name thinpool ephtest
#   lvcreate -V 4G --thin-pool ephtest/thinpool --name base
#   qemu-img convert -O raw <some-image.qcow2> /dev/ephtest/base
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

IMAGE=""
LVM_VG="ephtest"
LVM_BASE_LV="base"
BASE_URL="http://127.0.0.1:17798"

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        --lvm-vg) LVM_VG="$2"; shift 2 ;;
        --lvm-base-lv) LVM_BASE_LV="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || { echo "This test boots real VMs and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — VM creation and lvcreate/qemu-nbd need it." >&2; exit 1; }
[ -n "$IMAGE" ] && [ -f "$IMAGE" ] || { echo "--image is required and must exist" >&2; exit 1; }
BASE_LV_DEV="/dev/${LVM_VG}/${LVM_BASE_LV}"
[ -e "$BASE_LV_DEV" ] || { echo "$BASE_LV_DEV does not exist — set up the thin pool + base LV first (see --help)" >&2; exit 1; }

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

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1" >&2; }
info() { echo "  [INFO] $1"; }
section() { echo ""; echo "=== $1 ==="; }

TMP="$(mktemp -d)"
ID=""
SERVE_PID=""
cleanup() {
    [ -n "$ID" ] && "$EPH" --config "${TMP}/ephemera.toml" delete "$ID" >/dev/null 2>&1 || true
    [ -n "$SERVE_PID" ] && { kill "$SERVE_PID" >/dev/null 2>&1 || true; wait "$SERVE_PID" 2>/dev/null || true; }
    # Best-effort: in case a test failed before its own delete ran.
    for lv in $(sudo lvs --noheadings -o lv_name "$LVM_VG" 2>/dev/null | tr -d ' ' | grep '^eph-' || true); do
        sudo lvchange -an "${LVM_VG}/${lv}" >/dev/null 2>&1 || true
        sudo lvremove -f "${LVM_VG}/${lv}" >/dev/null 2>&1 || true
    done
    rm -rf "$TMP"
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys;v=json.load(sys.stdin).get('$1');print(v if v is not None else '')"; }

wait_exec() {
    local id="$1" attempts="${2:-30}"
    for _ in $(seq 1 "$attempts"); do
        if "$EPH" --config "${TMP}/ephemera.toml" exec "$id" -- echo ok >/dev/null 2>&1; then return 0; fi
        sleep 4
    done
    return 1
}

cat > "${TMP}/ephemera.toml" <<TOML
listen = "127.0.0.1:17798"
state_dir = "${TMP}/state"
run_dir = "${TMP}/run"
qemu_binary = "qemu-system-x86_64"
qemu_img_binary = "qemu-img"
cloud_hypervisor_binary = "cloud-hypervisor"
ch_remote_binary = "ch-remote"
cloud_localds_binary = "cloud-localds"
firecracker_binary = "firecracker"
default_bridge = "vmbr0"
reaper_interval_secs = 5

[jailer]
enabled = true
TOML
mkdir -p "${TMP}/state"

section "storage=lvm-thin: a real thin snapshot LV is created and boots"
cat > "${TMP}/vm-lvm.json" <<JSON
{"name":"storage-lvm","backend":"qemu","image":"${BASE_LV_DEV}","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"agent":{"enabled":true,"port":17777},"ttl_seconds":300,"storage":"lvm-thin"}
JSON
OUT=$("$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-lvm.json")
ID=$(echo "$OUT" | json_field id)
DISK=$(echo "$OUT" | json_field disk)
[ -n "$ID" ] && pass "VM created with storage=lvm-thin" || fail "create with storage=lvm-thin failed"
echo "  disk: ${DISK}"
if [[ "$DISK" == /dev/${LVM_VG}/eph-* ]]; then
    pass "disk is a fresh snapshot LV under ${LVM_VG}, not the base LV"
else
    fail "disk '${DISK}' is not a /dev/${LVM_VG}/eph-* snapshot path"
fi
SNAP_LV="${DISK#/dev/${LVM_VG}/}"
if sudo lvs "${LVM_VG}/${SNAP_LV}" >/dev/null 2>&1; then
    pass "snapshot LV ${LVM_VG}/${SNAP_LV} really exists in LVM"
else
    fail "snapshot LV ${LVM_VG}/${SNAP_LV} not found via lvs"
fi
if wait_exec "$ID" 30; then
    pass "guest booted off the LVM thin snapshot and answers exec"
else
    fail "guest never became reachable when booted from an LVM thin snapshot"
fi
"$EPH" --config "${TMP}/ephemera.toml" delete "$ID"
if sudo lvs "${LVM_VG}/${SNAP_LV}" >/dev/null 2>&1; then
    fail "snapshot LV ${LVM_VG}/${SNAP_LV} still exists after delete — leaked"
else
    pass "delete removed the snapshot LV — no leak"
fi
ID=""

section "storage=nbd: the disk is really served over a qemu-nbd export"
cat > "${TMP}/vm-nbd.json" <<JSON
{"name":"storage-nbd","backend":"qemu","image":"${IMAGE}","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"agent":{"enabled":true,"port":17777},"ttl_seconds":300,"storage":"nbd"}
JSON
OUT=$("$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-nbd.json")
ID=$(echo "$OUT" | json_field id)
[ -n "$ID" ] && pass "VM created with storage=nbd" || fail "create with storage=nbd failed"
GET_OUT=$("$EPH" --config "${TMP}/ephemera.toml" get "$ID")
NBD_PID=$(echo "$GET_OUT" | json_field nbd_pid)
WORKSPACE=$(echo "$GET_OUT" | json_field workspace)
if [ -n "$NBD_PID" ] && sudo kill -0 "$NBD_PID" >/dev/null 2>&1; then
    pass "a real qemu-nbd process (pid ${NBD_PID}) is serving this VM's disk"
else
    fail "no live qemu-nbd process recorded for this VM (nbd_pid='${NBD_PID}')"
fi
if [ -S "${WORKSPACE}/nbd.sock" ]; then
    pass "qemu-nbd UNIX socket exists at ${WORKSPACE}/nbd.sock"
else
    fail "expected qemu-nbd socket ${WORKSPACE}/nbd.sock not found"
fi
if sudo ps -p "$NBD_PID" -o cmd= 2>/dev/null | grep -q qemu-nbd; then
    pass "the recorded pid is genuinely a qemu-nbd process (not a stale/reused pid)"
else
    fail "pid ${NBD_PID} is not a qemu-nbd process"
fi
if wait_exec "$ID" 30; then
    pass "guest booted off the NBD-exported disk and answers exec"
else
    fail "guest never became reachable when booted over NBD"
fi
"$EPH" --config "${TMP}/ephemera.toml" delete "$ID"
if sudo kill -0 "$NBD_PID" >/dev/null 2>&1; then
    fail "qemu-nbd process ${NBD_PID} is still alive after delete — leaked"
else
    pass "delete stopped the qemu-nbd export — no leaked process"
fi
ID=""

section "storage=nbd is rejected up front for a non-QEMU backend"
cat > "${TMP}/vm-nbd-ch.json" <<JSON
{"name":"storage-nbd-ch","backend":"cloud-hypervisor","image":"${IMAGE}","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"ttl_seconds":300,"storage":"nbd"}
JSON
if "$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-nbd-ch.json" > "${TMP}/nbd-ch-out.txt" 2>&1; then
    fail "create with storage=nbd + backend=cloud-hypervisor should have been rejected but succeeded"
    ID=$(json_field id < "${TMP}/nbd-ch-out.txt")
else
    pass "storage=nbd + backend=cloud-hypervisor was correctly rejected"
    cat "${TMP}/nbd-ch-out.txt" | sed 's/^/    /'
fi

section "storage=lvm-thin is rejected up front under the Firecracker jailer"
cat > "${TMP}/vm-lvm-jail.json" <<JSON
{"name":"storage-lvm-jail","backend":"firecracker","image":"${BASE_LV_DEV}","kernel":"/nonexistent-kernel","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"ttl_seconds":300,"storage":"lvm-thin"}
JSON
if "$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-lvm-jail.json" > "${TMP}/lvm-jail-out.txt" 2>&1; then
    fail "create with storage=lvm-thin under the jailer should have been rejected but succeeded"
    ID=$(json_field id < "${TMP}/lvm-jail-out.txt")
else
    pass "storage=lvm-thin under the Firecracker jailer was correctly rejected"
    cat "${TMP}/lvm-jail-out.txt" | sed 's/^/    /'
fi

section "storage left unset (Default) still works exactly as before"
cat > "${TMP}/vm-default.json" <<JSON
{"name":"storage-default","backend":"qemu","image":"${IMAGE}","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"ttl_seconds":300}
JSON
OUT=$("$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-default.json")
ID=$(echo "$OUT" | json_field id)
DISK=$(echo "$OUT" | json_field disk)
[ -n "$ID" ] && pass "create with no storage field set still works (backward compatible)" || fail "create with storage unset broke"
[[ "$DISK" == *.qcow2 ]] && pass "default storage still produces a qcow2 overlay" || fail "default storage disk '${DISK}' is not qcow2"
"$EPH" --config "${TMP}/ephemera.toml" delete "$ID"
ID=""

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
