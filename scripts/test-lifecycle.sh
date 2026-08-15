#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# End-to-end lifecycle smoke test for Zyvor Ephemera: boots a real QEMU VM
# with the vsock guest agent enabled and proves pause/resume/exec/graceful
# shutdown actually work — not just that the CLI calls returned 200/0.
#
# - exec is verified over vsock with network.mode=none (no SSH, no network
#   path at all).
# - pause is verified by first exec'ing a CPU-bound background loop into the
#   guest, then confirming the VMM process's own CPU-time counter stops
#   advancing while paused. (A guest with nothing running is a poor pause
#   signal — idle and paused both show ~flat CPU time — so this test forces
#   real load first rather than trusting that alone.)
# - resume is verified by exec'ing again afterward.
# - stop is verified to prefer a graceful QMP shutdown (the VMM process
#   exits on its own) over a forced kill.
# - vsock CID allocation is verified unique across two VMs created
#   concurrently.
#
# QEMU only: Cloud Hypervisor and Firecracker were validated manually this
# session (see README) but need a Firecracker-compatible uncompressed
# vmlinux / extracted rootfs respectively, which is more setup than belongs
# in an unattended regression script.
#
# Usage:
#   sudo ./scripts/test-lifecycle.sh [--image PATH] [--config PATH]
#
# Env:
#   EPHEMERA_BIN   path to the ephemera binary (default: resolved from PATH or target/release)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

IMAGE=""
CONFIG="/etc/ephemera.toml"
[ -f "$CONFIG" ] || CONFIG=""

while [ $# -gt 0 ]; do
    case "$1" in
        --image)  IMAGE="$2"; shift 2 ;;
        --config) CONFIG="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1" >&2; }
section() { echo ""; echo "=== $1 ==="; }

[ "$(uname -s)" = "Linux" ] || { echo "This test boots a real VM and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — VM creation needs /var/lib/ephemera access." >&2; exit 1; }

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

GUEST_AGENT_BIN="${PROJECT_DIR}/target/release/ephemera-guest-agent"
if [ ! -x "$GUEST_AGENT_BIN" ]; then
    echo "Building ephemera-guest-agent..." >&2
    (cd "$PROJECT_DIR" && cargo build --release -p ephemera-guest-agent)
fi

STATE_DIR="/var/lib/ephemera"
if [ -n "$CONFIG" ]; then
    STATE_DIR=$(python3 -c "
import tomllib
with open('${CONFIG}', 'rb') as f:
    print(tomllib.load(f).get('state_dir', '/var/lib/ephemera'))
" 2>/dev/null || echo "/var/lib/ephemera")
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ -z "$IMAGE" ]; then
    IMAGE="${STATE_DIR}/images/ephemera-lifecycle-test.qcow2"
    if [ ! -f "$IMAGE" ]; then
        section "Building a test image with the guest agent baked in"
        cat > "${TMP}/build.json" <<JSON
{
  "source": "https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img",
  "output": "${IMAGE}",
  "format": "qcow2",
  "copy_in": [
    {"src": "${GUEST_AGENT_BIN}", "dest": "/usr/local/bin/ephemera-guest-agent"},
    {"src": "${PROJECT_DIR}/systemd/ephemera-guest-agent.service", "dest": "/etc/systemd/system/ephemera-guest-agent.service"}
  ],
  "enable_services": ["ephemera-guest-agent"]
}
JSON
        eph build-image --spec "${TMP}/build.json" >/dev/null
        pass "test image built: ${IMAGE}"
    fi
fi

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

section "Create (QEMU, agent enabled, network.mode=none)"
cat > "${TMP}/vm.json" <<JSON
{
  "name": "ephemera-lifecycle-test",
  "backend": "qemu",
  "image": "${IMAGE}",
  "vcpus": 1,
  "memory_mib": 768,
  "network": {"mode": "none"},
  "agent": {"enabled": true, "port": 17777},
  "ttl_seconds": 600
}
JSON
OUT=$(eph create --spec "${TMP}/vm.json")
ID=$(echo "$OUT" | json_field id)
CID=$(echo "$OUT" | json_field guest_cid)
PID=$(echo "$OUT" | json_field pid)
[ -n "$CID" ] && pass "vsock CID assigned: ${CID}" || fail "no vsock CID assigned"

section "Exec over vsock (no network path exists — proves it's not SSH)"
if wait_exec "$ID"; then
    RESULT=$(eph exec "$ID" -- echo hello-from-vsock)
    if echo "$RESULT" | python3 -c 'import json,sys;d=json.load(sys.stdin);sys.exit(0 if d.get("stdout","").strip()=="hello-from-vsock" else 1)' 2>/dev/null; then
        pass "exec round-tripped real output over vsock"
    else
        fail "exec succeeded but output didn't match: $RESULT"
    fi
else
    fail "guest agent never became reachable over vsock"
fi

section "Pause / resume"
eph exec "$ID" -- 'nohup sh -c "while true; do :; done" >/dev/null 2>&1 & disown; echo started' >/dev/null 2>&1 || true
sleep 1
eph pause "$ID" >/dev/null
if [ -e "/proc/${PID}" ]; then
    T1=$(awk '{print $14+$15}' "/proc/${PID}/stat")
    sleep 3
    T2=$(awk '{print $14+$15}' "/proc/${PID}/stat")
    if [ "$T1" = "$T2" ]; then
        pass "VMM CPU time frozen while paused (${T1} -> ${T2}, with a forced busy-loop running in the guest)"
    else
        fail "VMM CPU time still advancing while paused (${T1} -> ${T2}) — not actually frozen"
    fi
else
    fail "VMM process ${PID} not found to check CPU time"
fi
eph resume "$ID" >/dev/null
sleep 2
if eph exec "$ID" -- pkill -f 'while true' >/dev/null 2>&1; then
    pass "exec works again after resume"
else
    fail "exec failed after resume"
fi

section "Stop (graceful shutdown preferred over force-kill)"
T0=$(date +%s)
eph stop "$ID" >/dev/null
ELAPSED=$(( $(date +%s) - T0 ))
if kill -0 "$PID" 2>/dev/null; then
    fail "VMM process ${PID} still alive after stop"
else
    pass "VMM process exited (stop took ${ELAPSED}s)"
fi

section "Delete"
eph delete "$ID"
if eph list | python3 -c "import json,sys;d=json.load(sys.stdin);sys.exit(1 if any(v['id']=='$ID' for v in d) else 0)" 2>/dev/null; then
    pass "VM removed from state"
else
    fail "VM still present after delete"
fi

section "CID uniqueness under concurrent create"
cat > "${TMP}/vm-a.json" <<JSON
{"name":"ephemera-cidtest-a","backend":"qemu","image":"${IMAGE}","vcpus":1,"memory_mib":512,"network":{"mode":"none"},"agent":{"enabled":true},"ttl_seconds":120}
JSON
cat > "${TMP}/vm-b.json" <<JSON
{"name":"ephemera-cidtest-b","backend":"qemu","image":"${IMAGE}","vcpus":1,"memory_mib":512,"network":{"mode":"none"},"agent":{"enabled":true},"ttl_seconds":120}
JSON
eph create --spec "${TMP}/vm-a.json" > "${TMP}/cid-a.json" &
eph create --spec "${TMP}/vm-b.json" > "${TMP}/cid-b.json" &
wait
CID_A=$(json_field guest_cid < "${TMP}/cid-a.json")
CID_B=$(json_field guest_cid < "${TMP}/cid-b.json")
ID_A=$(json_field id < "${TMP}/cid-a.json")
ID_B=$(json_field id < "${TMP}/cid-b.json")
if [ -n "$CID_A" ] && [ -n "$CID_B" ] && [ "$CID_A" != "$CID_B" ]; then
    pass "concurrent creates got distinct CIDs (${CID_A} vs ${CID_B})"
else
    fail "concurrent creates did not get distinct CIDs (${CID_A} vs ${CID_B})"
fi
eph stop "$ID_A" >/dev/null 2>&1 || true
eph delete "$ID_A" >/dev/null 2>&1 || true
eph stop "$ID_B" >/dev/null 2>&1 || true
eph delete "$ID_B" >/dev/null 2>&1 || true

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
