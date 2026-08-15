#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real-hardware regression test for warm VM pools (VmManager::create_pool /
# claim_from_pool / delete_pool). Boots real QEMU VMs.
#
# Proves:
# - a pool backfills to its target size on its own (create + pause per
#   member, done asynchronously by a background task — see
#   VmManager::backfill_pool)
# - claiming pops a ready member and resumes it MUCH faster than a full
#   create would take (timed against a plain create for comparison)
# - the pool backfills again after a claim, without the caller asking for it
# - two claims in a row hand out two different VMs
# - deleting a pool cleans up every member VM it still owns
#
# Usage:
#   sudo ./scripts/test-warm-pool.sh --image /var/lib/ephemera/images/ephemera-lifecycle-test.qcow2
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

CONFIG="/etc/ephemera.toml"
[ -f "$CONFIG" ] || CONFIG=""
IMAGE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --image)  IMAGE="$2"; shift 2 ;;
        --config) CONFIG="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || { echo "This test boots real VMs and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — VM creation needs /var/lib/ephemera access." >&2; exit 1; }
[ -n "$IMAGE" ] && [ -f "$IMAGE" ] || { echo "--image is required and must exist" >&2; exit 1; }

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
POOL="ephemera-pool-test"
cleanup() {
    eph pool delete "$POOL" >/dev/null 2>&1 || true
    rm -rf "$TMP"
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys;v=json.load(sys.stdin).get('$1');print(v if v is not None else '')"; }

wait_for_pool_size() {
    local target="$1" attempts="${2:-30}"
    for _ in $(seq 1 "$attempts"); do
        local n
        n=$(eph pool get "$POOL" | python3 -c "import json,sys;print(len(json.load(sys.stdin)['members']))")
        [ "$n" -ge "$target" ] && return 0
        sleep 2
    done
    return 1
}

section "Create pool (size=2, QEMU, agent enabled)"
cat > "${TMP}/pool.json" <<JSON
{
  "name": "${POOL}",
  "size": 2,
  "template": {
    "name": "ignored",
    "backend": "qemu",
    "image": "${IMAGE}",
    "vcpus": 1,
    "memory_mib": 768,
    "network": {"mode": "none"},
    "agent": {"enabled": true, "port": 17777}
  }
}
JSON
eph pool create --spec "${TMP}/pool.json" >/dev/null
pass "pool created"

section "Pool backfills to its target size on its own"
if wait_for_pool_size 2; then
    pass "pool reached 2 ready members without further action"
else
    fail "pool never reached 2 members"
fi
# Sanity: both members are actually Paused VMs in the main VM store.
BAD=0
for id in $(eph pool get "$POOL" | python3 -c "import json,sys;[print(m) for m in json.load(sys.stdin)['members']]"); do
    status=$(eph get "$id" | json_field status)
    [ "$status" = "paused" ] || BAD=1
done
[ "$BAD" -eq 0 ] && pass "every pool member is a real Paused VM" || fail "a pool member was not actually Paused"

section "Baseline: how long a plain create takes"
cat > "${TMP}/plain.json" <<JSON
{"name":"ephemera-plain-baseline","backend":"qemu","image":"${IMAGE}","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"agent":{"enabled":true,"port":17777}}
JSON
T0=$(date +%s.%N)
PLAIN_ID=$(eph create --spec "${TMP}/plain.json" | json_field id)
CREATE_SECS=$(python3 -c "print(f'{$(date +%s.%N) - $T0:.1f}')")
echo "  plain create took ${CREATE_SECS}s"
eph delete "$PLAIN_ID" >/dev/null 2>&1 || true

section "Claim is much faster than create (resume, not boot)"
T0=$(date +%s.%N)
CLAIMED=$(eph pool claim "$POOL" --vm-name claimed-1)
CLAIM_SECS=$(python3 -c "print(f'{$(date +%s.%N) - $T0:.1f}')")
CLAIMED_ID=$(echo "$CLAIMED" | json_field id)
CLAIMED_STATUS=$(echo "$CLAIMED" | json_field status)
echo "  claim took ${CLAIM_SECS}s (create took ${CREATE_SECS}s)"
[ "$CLAIMED_STATUS" = "running" ] && pass "claimed VM is Running" || fail "claimed VM status was '${CLAIMED_STATUS}', not running"
if python3 -c "import sys; sys.exit(0 if float('$CLAIM_SECS') < float('$CREATE_SECS') else 1)"; then
    pass "claim (${CLAIM_SECS}s) was faster than a plain create (${CREATE_SECS}s)"
else
    fail "claim (${CLAIM_SECS}s) was NOT faster than a plain create (${CREATE_SECS}s)"
fi

section "Claimed VM actually works (exec over vsock)"
if eph exec "$CLAIMED_ID" -- echo claimed-and-working >/dev/null 2>&1; then
    pass "exec succeeded on the claimed VM immediately after claim"
else
    fail "exec failed on the claimed VM"
fi

section "Pool backfills again after the claim, without being asked"
if wait_for_pool_size 2; then
    pass "pool refilled back to 2 members after the claim"
else
    fail "pool did not refill after the claim"
fi

section "A second claim hands out a different VM"
CLAIMED2=$(eph pool claim "$POOL" --vm-name claimed-2)
CLAIMED2_ID=$(echo "$CLAIMED2" | json_field id)
[ "$CLAIMED2_ID" != "$CLAIMED_ID" ] && pass "second claim returned a different VM ($CLAIMED2_ID != $CLAIMED_ID)" || fail "second claim returned the SAME VM"

eph delete "$CLAIMED_ID" >/dev/null 2>&1 || true
eph delete "$CLAIMED2_ID" >/dev/null 2>&1 || true

section "Delete pool cleans up remaining members"
wait_for_pool_size 2 >/dev/null 2>&1 || true
REMAINING=$(eph pool get "$POOL" | python3 -c "import json,sys;print(json.load(sys.stdin)['members'])" | python3 -c "import ast,sys;print(len(ast.literal_eval(sys.stdin.read())))" 2>/dev/null || echo 0)
eph pool delete "$POOL"
LIST_AFTER=$(eph list)
LEFTOVER=$(echo "$LIST_AFTER" | python3 -c "
import json,sys
items = json.load(sys.stdin)
print(sum(1 for v in items if v['name'].startswith('${POOL}-pool-')))
")
if [ "$LEFTOVER" -eq 0 ]; then
    pass "no leftover pool-member VMs after pool delete (had ${REMAINING} member(s))"
else
    fail "${LEFTOVER} pool-member VM(s) leaked after pool delete"
fi
if eph pool get "$POOL" >/dev/null 2>&1; then
    fail "pool record still exists after delete"
else
    pass "pool record removed"
fi

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
