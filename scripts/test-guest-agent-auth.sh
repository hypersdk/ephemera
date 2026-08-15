#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real-hardware regression test for the vsock guest-agent's shared-secret
# auth (ephemera_guest_protocol::Envelope / TOKEN_FILE_PATH). Boots a real
# QEMU VM and talks to its guest agent over a *raw* AF_VSOCK socket — not
# through `eph exec` — because the point of this test is to prove the guest
# itself rejects an unauthenticated/wrong-token caller, independent of
# whether the host's own ephemera-vsock-client behaves correctly (that part
# is already covered by test-lifecycle.sh/test-auto-backend.sh, which always
# go through the host client and therefore always send the right token).
#
# This is exactly the threat model the token defends against: some other
# process on the host — not ephemera itself — opening a raw vsock socket to
# the VM's CID.
#
# Requires a test image with the current ephemera-guest-agent baked in (see
# README's "Pause, resume, and exec" section for how to build one via
# build-image's copy_in/enable_services).
#
# Usage:
#   sudo ./scripts/test-guest-agent-auth.sh --image /var/lib/ephemera/images/ephemera-auth-test.qcow2
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
            sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || { echo "This test boots a real VM and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — VM creation needs /var/lib/ephemera access." >&2; exit 1; }
[ -n "$IMAGE" ] && [ -f "$IMAGE" ] || { echo "--image is required and must exist (build one with the current ephemera-guest-agent baked in — see README)" >&2; exit 1; }

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
cleanup() {
    [ -n "$ID" ] && eph delete "$ID" >/dev/null 2>&1 || true
    rm -rf "$TMP"
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys;v=json.load(sys.stdin).get('$1');print(v if v is not None else '')"; }

# Speaks the raw wire protocol directly (Envelope + newline-delimited JSON)
# over a native AF_VSOCK socket, bypassing ephemera-vsock-client entirely.
raw_vsock_request() {
    local cid="$1" port="$2" token_json="$3" op_json="$4"
    python3 - "$cid" "$port" "$token_json" "$op_json" <<'PYEOF'
import json, socket, sys
cid, port, token_json, op_json = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], sys.argv[4]
req = json.loads(op_json)
req["token"] = json.loads(token_json)
s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
s.settimeout(10)
s.connect((cid, port))
s.sendall((json.dumps(req) + "\n").encode())
buf = b""
while not buf.endswith(b"\n"):
    chunk = s.recv(1)
    if not chunk:
        break
    buf += chunk
print(buf.decode().strip())
PYEOF
}

section "Create (QEMU, agent enabled — token auto-generated)"
cat > "${TMP}/vm.json" <<JSON
{
  "name": "ephemera-auth-test",
  "backend": "qemu",
  "image": "${IMAGE}",
  "vcpus": 1,
  "memory_mib": 768,
  "network": {"mode": "none"},
  "agent": {"enabled": true, "port": 17777},
  "ttl_seconds": 300
}
JSON
OUT=$(eph create --spec "${TMP}/vm.json")
ID=$(echo "$OUT" | json_field id)
CID=$(echo "$OUT" | python3 -c "import json,sys;print(json.load(sys.stdin)['guest_cid'])")
TOKEN=$(echo "$OUT" | python3 -c "import json,sys;print(json.load(sys.stdin)['request']['agent']['token'])")
[ -n "$TOKEN" ] && pass "a token was auto-generated for this agent-enabled VM" || fail "no token was generated"

echo "waiting for guest agent to become reachable..."
READY=0
for _ in $(seq 1 20); do
    if eph exec "$ID" -- echo ok >/dev/null 2>&1; then READY=1; break; fi
    sleep 4
done
[ "$READY" = "1" ] || { fail "guest agent never became reachable at all"; echo "pass: ${PASS}  fail: ${FAIL}"; exit 1; }

section "Raw vsock, no token -> rejected"
RESP=$(raw_vsock_request "$CID" 17777 'null' '{"op":"ping"}')
echo "  response: $RESP"
if echo "$RESP" | python3 -c 'import json,sys;d=json.load(sys.stdin);sys.exit(0 if d.get("result")=="error" and "unauthor" in d.get("message","").lower() else 1)' 2>/dev/null; then
    pass "no-token request was rejected as unauthorized"
else
    fail "no-token request was NOT rejected (got: $RESP)"
fi

section "Raw vsock, wrong token -> rejected"
RESP=$(raw_vsock_request "$CID" 17777 '"not-the-real-token"' '{"op":"ping"}')
echo "  response: $RESP"
if echo "$RESP" | python3 -c 'import json,sys;d=json.load(sys.stdin);sys.exit(0 if d.get("result")=="error" and "unauthor" in d.get("message","").lower() else 1)' 2>/dev/null; then
    pass "wrong-token request was rejected as unauthorized"
else
    fail "wrong-token request was NOT rejected (got: $RESP)"
fi

section "Raw vsock, correct token -> succeeds"
RESP=$(raw_vsock_request "$CID" 17777 "\"${TOKEN}\"" '{"op":"ping"}')
echo "  response: $RESP"
if echo "$RESP" | python3 -c 'import json,sys;sys.exit(0 if json.load(sys.stdin).get("result")=="pong" else 1)' 2>/dev/null; then
    pass "correct-token request succeeded"
else
    fail "correct-token request did NOT succeed (got: $RESP)"
fi

section "Host client (ephemera-vsock-client) still works end to end"
if eph exec "$ID" -- echo hello-authed >/dev/null 2>&1; then
    pass "eph exec (which auto-supplies the stored token) still works"
else
    fail "eph exec stopped working after auth was added"
fi

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
