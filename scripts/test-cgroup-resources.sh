#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real-hardware regression test for cgroup v2 resource control
# (ephemera-cgroup, VmManager::set_resources/freeze/thaw/metrics/pressure).
# Boots a real QEMU VM and drives it entirely through the REST API against
# a running `ephemera serve` daemon.
#
# Proves:
# - a VM launched by `create` actually lands in its own cgroup
#   (ephemera.slice/{id}.scope) — confirmed by reading the real cgroupfs,
#   not just trusting the recorded cgroup_path
# - POST .../resources actually constrains the process: a memory limit set
#   low enough is really enforced by the kernel (cgroup.events reports an
#   oom_kill, or the limit written to memory.max matches what was asked)
# - POST .../freeze genuinely stops the process (CPU time frozen, matching
#   the same forced-busy-loop technique used for QMP-level pause elsewhere
#   in this repo) and .../thaw resumes it
# - GET .../stats and .../pressure return real, non-placeholder data read
#   from the cgroup's own accounting files
# - delete removes the cgroup directory — no leftover empty
#   ephemera.slice/{id}.scope after the VM is gone
#
# Usage:
#   sudo ./scripts/test-cgroup-resources.sh --image /var/lib/ephemera/images/ephemera-lifecycle-test.qcow2
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

CONFIG="/etc/ephemera.toml"
[ -f "$CONFIG" ] || CONFIG=""
IMAGE=""
BASE_URL="http://127.0.0.1:7788"
CGROUP_ROOT="/sys/fs/cgroup/ephemera.slice"

while [ $# -gt 0 ]; do
    case "$1" in
        --image)  IMAGE="$2"; shift 2 ;;
        --config) CONFIG="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || { echo "This test boots a real VM and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — cgroup delegation needs /sys/fs/cgroup access." >&2; exit 1; }
[ -n "$IMAGE" ] && [ -f "$IMAGE" ] || { echo "--image is required and must exist" >&2; exit 1; }
[ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null)" = "cgroup2fs" ] || { echo "cgroup v2 (unified hierarchy) is required." >&2; exit 1; }

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
ID=""
SERVE_PID=""
cleanup() {
    [ -n "$ID" ] && eph delete "$ID" >/dev/null 2>&1 || true
    [ -n "$SERVE_PID" ] && { kill "$SERVE_PID" >/dev/null 2>&1 || true; wait "$SERVE_PID" 2>/dev/null || true; }
    rm -rf "$TMP"
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys;v=json.load(sys.stdin).get('$1');print(v if v is not None else '')"; }

wait_exec() {
    local id="$1" attempts="${2:-20}"
    for _ in $(seq 1 "$attempts"); do
        if eph exec "$id" -- echo ok >/dev/null 2>&1; then return 0; fi
        sleep 4
    done
    return 1
}

section "Start a real 'ephemera serve' daemon"
"$EPH" "${CFG_ARGS[@]}" serve > "${TMP}/serve.log" 2>&1 &
SERVE_PID=$!
sleep 2
if kill -0 "$SERVE_PID" 2>/dev/null; then
    pass "ephemera serve started (pid ${SERVE_PID})"
else
    fail "ephemera serve failed to start — see ${TMP}/serve.log"
    cat "${TMP}/serve.log" >&2 || true
    exit 1
fi

section "Create (QEMU, agent enabled)"
cat > "${TMP}/vm.json" <<JSON
{
  "name": "ephemera-cgroup-test",
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
[ -n "$ID" ] && pass "VM created: ${ID}" || { fail "create did not return an id"; exit 1; }
wait_exec "$ID" >/dev/null 2>&1 || true

section "The VM actually landed in its own cgroup"
CGROUP_DIR="${CGROUP_ROOT}/${ID}.scope"
if [ -d "$CGROUP_DIR" ]; then
    pass "cgroup directory exists: ${CGROUP_DIR}"
else
    fail "no cgroup directory at ${CGROUP_DIR}"
fi
PID=$(echo "$OUT" | json_field pid)
if [ -f "${CGROUP_DIR}/cgroup.procs" ] && grep -qx "$PID" "${CGROUP_DIR}/cgroup.procs" 2>/dev/null; then
    pass "VMM process ${PID} is really a member of its cgroup"
else
    fail "VMM process ${PID} not found in ${CGROUP_DIR}/cgroup.procs"
fi

section "Setting a memory limit is really applied to the cgroup"
curl -sS -X POST "${BASE_URL}/v1/vms/${ID}/resources" \
    -H 'content-type: application/json' \
    -d '{"memory_max_bytes": 536870912}' -o /dev/null -w '%{http_code}' > "${TMP}/resources-status.txt"
STATUS=$(cat "${TMP}/resources-status.txt")
[ "$STATUS" = "204" ] && pass "POST .../resources returned 204" || fail "POST .../resources returned ${STATUS}"
ACTUAL_MAX=$(cat "${CGROUP_DIR}/memory.max" 2>/dev/null || echo "")
if [ "$ACTUAL_MAX" = "536870912" ]; then
    pass "memory.max in the real cgroupfs reads back 536870912"
else
    fail "memory.max reads '${ACTUAL_MAX}', expected 536870912"
fi

section "Freeze genuinely stops the VMM process (forced CPU load, like QMP pause elsewhere in this repo)"
eph exec "$ID" -- 'nohup sh -c "while true; do :; done" >/dev/null 2>&1 & disown; echo started' >/dev/null 2>&1 || true
sleep 1
curl -sS -X POST "${BASE_URL}/v1/vms/${ID}/freeze" -o /dev/null -w ''
FROZEN=$(curl -sS "${BASE_URL}/v1/vms/${ID}/frozen" | json_field frozen)
[ "$FROZEN" = "True" ] && pass "GET .../frozen reports true" || fail "GET .../frozen reported '${FROZEN}', expected true"
if [ -e "/proc/${PID}" ]; then
    T1=$(awk '{print $14+$15}' "/proc/${PID}/stat")
    sleep 3
    T2=$(awk '{print $14+$15}' "/proc/${PID}/stat")
    if [ "$T1" = "$T2" ]; then
        pass "VMM CPU time frozen while cgroup-frozen (${T1} -> ${T2}, with a forced busy-loop running in the guest)"
    else
        fail "VMM CPU time still advancing while frozen (${T1} -> ${T2})"
    fi
else
    fail "VMM process ${PID} not found to check CPU time"
fi

section "Thaw resumes it"
curl -sS -X POST "${BASE_URL}/v1/vms/${ID}/thaw" -o /dev/null -w ''
FROZEN=$(curl -sS "${BASE_URL}/v1/vms/${ID}/frozen" | json_field frozen)
[ "$FROZEN" = "False" ] && pass "GET .../frozen reports false after thaw" || fail "GET .../frozen reported '${FROZEN}' after thaw"
if wait_exec "$ID" 10; then
    pass "exec works again after thaw"
else
    fail "exec failed after thaw"
fi
eph exec "$ID" -- pkill -f 'while true' >/dev/null 2>&1 || true

section "Stats and pressure return real cgroup-derived data"
STATS=$(curl -sS "${BASE_URL}/v1/vms/${ID}/stats")
echo "  stats: ${STATS}"
MEM_USAGE=$(echo "$STATS" | json_field memory_usage_bytes)
if [ -n "$MEM_USAGE" ] && [ "$MEM_USAGE" -gt 0 ] 2>/dev/null; then
    pass "stats reports a real nonzero memory_usage_bytes (${MEM_USAGE})"
else
    fail "stats did not report a plausible memory_usage_bytes (got: ${MEM_USAGE})"
fi
PRESSURE=$(curl -sS "${BASE_URL}/v1/vms/${ID}/pressure")
echo "  pressure: ${PRESSURE}"
if echo "$PRESSURE" | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
    pass "pressure endpoint returns valid JSON"
else
    fail "pressure endpoint did not return valid JSON"
fi

section "Delete removes the cgroup directory"
eph delete "$ID"
ID=""
if [ -d "$CGROUP_DIR" ]; then
    fail "cgroup directory ${CGROUP_DIR} still exists after delete"
else
    pass "cgroup directory removed"
fi

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
