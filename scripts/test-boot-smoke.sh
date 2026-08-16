#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Minimal real-hardware smoke test: `ephemera serve` comes up, answers
# /healthz, boots a real QEMU VM via the REST API (no agent, no network —
# just the bare create path), and the VM actually reaches a real systemd
# boot sequence (not just "a qemu-system-x86_64 process exists"). Then
# deletes it and confirms the VMM process is gone.
#
# This is the fast, no-agent-image-required check for "is the daemon + REST
# API + QEMU backend fundamentally healthy" — useful after any host-level
# change (a package upgrade, a host that had something go wrong on it) when
# you want a quick real-boot confirmation without the heavier setup
# scripts/test-lifecycle.sh needs (an agent-enabled image, vsock).
#
# Usage:
#   sudo ./scripts/test-boot-smoke.sh --image /var/lib/ephemera/images/ubuntu-noble.qcow2
#
# Env:
#   EPHEMERA_BIN   path to the ephemera binary (default: resolved from PATH or target/release)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

IMAGE=""
BASE_URL="http://127.0.0.1:17801"

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || { echo "This test boots a real VM and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — VM creation needs /var/lib/ephemera access." >&2; exit 1; }
[ -n "$IMAGE" ] && [ -f "$IMAGE" ] || { echo "--image is required and must exist" >&2; exit 1; }

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
SERVE_PID=""
VM_ID=""
VM_PID=""
cleanup() {
    [ -n "$VM_ID" ] && curl -sS -X DELETE "${BASE_URL}/v1/vms/${VM_ID}" -o /dev/null 2>/dev/null || true
    [ -n "$SERVE_PID" ] && { kill "$SERVE_PID" >/dev/null 2>&1 || true; wait "$SERVE_PID" 2>/dev/null || true; }
    rm -rf "$TMP"
}
trap cleanup EXIT

mkdir -p "${TMP}/state" "${TMP}/run"
cat > "${TMP}/ephemera.toml" <<TOML
listen = "127.0.0.1:17801"
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
TOML

section "ephemera serve starts and answers /healthz"
"$EPH" --config "${TMP}/ephemera.toml" serve > "${TMP}/serve.log" 2>&1 &
SERVE_PID=$!
sleep 2
kill -0 "$SERVE_PID" 2>/dev/null || { fail "ephemera serve failed to start"; cat "${TMP}/serve.log" >&2; exit 1; }
pass "ephemera serve is running (pid ${SERVE_PID})"
if curl -sS -m 5 "${BASE_URL}/healthz" | grep -q '"ok":true'; then
    pass "/healthz reports ok"
else
    fail "/healthz did not report ok"
fi

section "Create a real QEMU VM via the REST API"
RESP=$(curl -sS -X POST "${BASE_URL}/v1/vms" -H "Content-Type: application/json" -d "{
  \"name\": \"boot-smoke-test\",
  \"backend\": \"qemu\",
  \"image\": \"${IMAGE}\",
  \"vcpus\": 1,
  \"memory_mib\": 768,
  \"network\": {\"mode\": \"none\"},
  \"ttl_seconds\": 300
}")
VM_ID=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin).get('id',''))" 2>/dev/null || true)
VM_PID=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin).get('pid','') or '')" 2>/dev/null || true)
STATUS=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin).get('status',''))" 2>/dev/null || true)
if [ -n "$VM_ID" ] && [ "$STATUS" = "running" ] && [ -n "$VM_PID" ]; then
    pass "VM created: id=${VM_ID} pid=${VM_PID} status=${STATUS}"
else
    fail "create did not return a running VM: $RESP"
    exit 1
fi

section "The VMM process is real and the guest is actually booting"
sleep 8
if kill -0 "$VM_PID" 2>/dev/null; then
    pass "qemu-system-x86_64 process ${VM_PID} is alive"
else
    fail "qemu-system-x86_64 process ${VM_PID} is not running"
fi
LOG="${TMP}/state/instances/${VM_ID}/console.log"
if [ -f "$LOG" ] && grep -qE 'Linux version|systemd\[1\]|Reached target|Started ' "$LOG" 2>/dev/null; then
    pass "console log shows a real kernel/systemd boot sequence"
else
    fail "console log doesn't show a real boot sequence yet"
    info "last 10 lines of ${LOG}:"
    tail -10 "$LOG" >&2 2>/dev/null || true
fi

section "Delete tears the VM down for real"
DELETE_CODE=$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE "${BASE_URL}/v1/vms/${VM_ID}")
if [ "$DELETE_CODE" = "204" ] || [ "$DELETE_CODE" = "200" ]; then
    pass "delete returned ${DELETE_CODE}"
else
    fail "delete returned unexpected status ${DELETE_CODE}"
fi
sleep 1
if kill -0 "$VM_PID" 2>/dev/null; then
    fail "qemu-system-x86_64 process ${VM_PID} still alive after delete"
else
    pass "qemu-system-x86_64 process is gone after delete"
fi
VM_ID=""

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
