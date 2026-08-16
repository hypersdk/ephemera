#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real-hardware regression test for the image catalog + Ed25519 signing
# (ephemera_image::catalog, `ephemera catalog keygen/sign`,
# GET /v1/images/catalog). Boots a real QEMU VM created by referencing a
# catalog alias instead of a raw image path.
#
# Proves:
# - `ephemera catalog keygen` produces a usable keypair, and `catalog sign`
#   produces a verifiable signed entry
# - creating a VM with `"image": "<catalog-name>"` actually resolves through
#   the catalog and boots the real underlying image (verified via exec)
# - with trusted_signers configured, an unsigned catalog entry is rejected
#   at create time (fails closed) — not silently allowed through
# - a validly signed entry is accepted and boots
# - GET /v1/images/catalog reports signature_valid: true for the signed
#   entry and includes it in the listing
# - a plain literal path (not in the catalog) still works unchanged —
#   backward compatible
#
# Usage:
#   sudo ./scripts/test-image-catalog.sh --image /var/lib/ephemera/images/ephemera-lifecycle-test.qcow2
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

IMAGE=""
BASE_URL="http://127.0.0.1:17799"

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
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
section() { echo ""; echo "=== $1 ==="; }

TMP="$(mktemp -d)"
ID=""
SERVE_PID=""
cleanup() {
    [ -n "$ID" ] && "$EPH" --config "${TMP}/ephemera.toml" delete "$ID" >/dev/null 2>&1 || true
    [ -n "$SERVE_PID" ] && { kill "$SERVE_PID" >/dev/null 2>&1 || true; wait "$SERVE_PID" 2>/dev/null || true; }
    rm -rf "$TMP"
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys;v=json.load(sys.stdin).get('$1');print(v if v is not None else '')"; }

wait_exec() {
    local id="$1" attempts="${2:-20}"
    for _ in $(seq 1 "$attempts"); do
        if "$EPH" --config "${TMP}/ephemera.toml" exec "$id" -- echo ok >/dev/null 2>&1; then return 0; fi
        sleep 4
    done
    return 1
}

SHA256=$(sha256sum "$IMAGE" | awk '{print $1}')

section "keygen produces a usable Ed25519 keypair"
KEYGEN_OUT=$("$EPH" catalog keygen)
echo "$KEYGEN_OUT"
PRIVATE_KEY=$(echo "$KEYGEN_OUT" | grep -A1 'private key' | tail -1 | tr -d ' ')
PUBLIC_KEY=$(echo "$KEYGEN_OUT" | grep -A1 'public key' | tail -1 | tr -d ' ')
[ -n "$PRIVATE_KEY" ] && [ -n "$PUBLIC_KEY" ] && pass "keygen produced both a private and public key" || fail "keygen output missing a key"

cat > "${TMP}/catalog.json" <<JSON
[]
JSON

section "sign produces a signed catalog entry"
SIGN_OUT=$("$EPH" catalog sign --key "$PRIVATE_KEY" --name test-image --source "$IMAGE" --sha256 "$SHA256" --catalog-file "${TMP}/catalog.json")
echo "$SIGN_OUT" | python3 -m json.tool | head -5
if python3 -c "import json; d=json.load(open('${TMP}/catalog.json')); assert len(d)==1 and d[0]['name']=='test-image' and d[0]['signature']"; then
    pass "signed entry written to catalog file with a signature present"
else
    fail "catalog file missing the expected signed entry"
fi

cat > "${TMP}/ephemera.toml" <<TOML
listen = "127.0.0.1:17799"
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

[catalog]
path = "${TMP}/catalog.json"
trusted_signers = []
TOML
mkdir -p "${TMP}/state"

section "Create with an unsigned entry succeeds when trusted_signers is empty"
cat > "${TMP}/vm-unsigned.json" <<JSON
{"name":"catalog-unsigned","backend":"qemu","image":"test-image","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"agent":{"enabled":true,"port":17777},"ttl_seconds":300}
JSON
OUT=$("$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-unsigned.json")
ID=$(echo "$OUT" | json_field id)
[ -n "$ID" ] && pass "VM created by referencing the catalog alias 'test-image'" || fail "create via catalog alias failed"
if wait_exec "$ID" 20; then
    pass "guest booted the catalog-resolved image and answers exec"
else
    fail "guest never became reachable"
fi
"$EPH" --config "${TMP}/ephemera.toml" delete "$ID"
ID=""

section "With trusted_signers set, an unsigned reference to the SAME name is rejected"
# Re-sign a second, distinct catalog entry that has no signature at all,
# under a different name, to isolate this check from the one above.
python3 -c "
import json
d = json.load(open('${TMP}/catalog.json'))
d.append({'name': 'unsigned-image', 'source': '${IMAGE}', 'sha256': '${SHA256}', 'format': 'qcow2'})
json.dump(d, open('${TMP}/catalog.json', 'w'))
"
python3 -c "
content = open('${TMP}/ephemera.toml').read()
content = content.replace('trusted_signers = []', 'trusted_signers = [\"${PUBLIC_KEY}\"]')
open('${TMP}/ephemera.toml', 'w').write(content)
"
cat > "${TMP}/vm-should-fail.json" <<JSON
{"name":"catalog-should-fail","backend":"qemu","image":"unsigned-image","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"ttl_seconds":300}
JSON
if "$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-should-fail.json" > "${TMP}/should-fail-out.txt" 2>&1; then
    fail "create with an unsigned entry succeeded even though trusted_signers is configured"
    ID=$(json_field id < "${TMP}/should-fail-out.txt")
else
    pass "create correctly rejected the unsigned entry once trusted_signers was configured"
    cat "${TMP}/should-fail-out.txt" | sed 's/^/    /'
fi

section "A validly signed entry is accepted with trusted_signers configured"
cat > "${TMP}/vm-signed.json" <<JSON
{"name":"catalog-signed","backend":"qemu","image":"test-image","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"agent":{"enabled":true,"port":17777},"ttl_seconds":300}
JSON
OUT=$("$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-signed.json")
ID=$(echo "$OUT" | json_field id)
[ -n "$ID" ] && pass "VM created via the signed catalog entry with trusted_signers enforced" || fail "create with a validly signed entry failed"
if wait_exec "$ID" 20; then
    pass "guest booted and answers exec"
else
    fail "guest never became reachable"
fi
"$EPH" --config "${TMP}/ephemera.toml" delete "$ID"
ID=""

section "A plain literal path (not in the catalog) still works unchanged"
cat > "${TMP}/vm-literal.json" <<JSON
{"name":"catalog-literal","backend":"qemu","image":"${IMAGE}","vcpus":1,"memory_mib":768,"network":{"mode":"none"},"ttl_seconds":300}
JSON
OUT=$("$EPH" --config "${TMP}/ephemera.toml" create --spec "${TMP}/vm-literal.json")
ID=$(echo "$OUT" | json_field id)
[ -n "$ID" ] && pass "create with a plain literal image path still works" || fail "create with a literal path broke"
"$EPH" --config "${TMP}/ephemera.toml" delete "$ID"
ID=""

section "GET /v1/images/catalog reports signature_valid correctly"
"$EPH" --config "${TMP}/ephemera.toml" serve > "${TMP}/serve.log" 2>&1 &
SERVE_PID=$!
sleep 2
if ! kill -0 "$SERVE_PID" 2>/dev/null; then
    fail "ephemera serve failed to start (port ${BASE_URL} likely in use by something else) — see ${TMP}/serve.log"
    cat "${TMP}/serve.log" >&2 || true
    SERVE_PID=""
fi
LISTING=$(curl -sS "${BASE_URL}/v1/images/catalog")
echo "$LISTING" | python3 -m json.tool
SIGNED_VALID=$(echo "$LISTING" | python3 -c "import json,sys;d=json.load(sys.stdin);print([e['signature_valid'] for e in d['items'] if e['name']=='test-image'][0])")
UNSIGNED_VALID=$(echo "$LISTING" | python3 -c "import json,sys;d=json.load(sys.stdin);print([e['signature_valid'] for e in d['items'] if e['name']=='unsigned-image'][0])")
[ "$SIGNED_VALID" = "True" ] && pass "catalog listing reports signature_valid=true for the signed entry" || fail "signed entry reported signature_valid=${SIGNED_VALID}"
[ "$UNSIGNED_VALID" = "False" ] && pass "catalog listing reports signature_valid=false for the unsigned entry" || fail "unsigned entry reported signature_valid=${UNSIGNED_VALID}"
kill "$SERVE_PID" >/dev/null 2>&1 || true
wait "$SERVE_PID" 2>/dev/null || true
SERVE_PID=""

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
