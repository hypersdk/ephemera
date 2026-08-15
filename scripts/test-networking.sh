#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# End-to-end networking smoke test for Zyvor Ephemera: boots a real VM over
# each supported network mode and verifies it is actually reachable over SSH.
#
# Test 1 — QEMU user-mode NAT + host port forward (no host network changes).
# Test 2 — TAP attached to an existing Linux bridge with a DHCP server on it
#           (e.g. libvirt's "default" network on virbr0). Skipped with a
#           warning if the bridge doesn't exist.
#
# Both tests also verify cleanup: the QEMU process and (for TAP) the tap
# interface must be gone after `ephemera delete`.
#
# Usage:
#   sudo ./scripts/test-networking.sh [--bridge NAME] [--image PATH] [--config PATH]
#
# Env:
#   EPHEMERA_BIN   path to the ephemera binary (default: resolved from PATH or target/release)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

BRIDGE="vmbr0"
IMAGE=""
CONFIG="/etc/ephemera.toml"
[ -f "$CONFIG" ] || CONFIG=""

while [ $# -gt 0 ]; do
    case "$1" in
        --bridge) BRIDGE="$2"; shift 2 ;;
        --image)  IMAGE="$2"; shift 2 ;;
        --config) CONFIG="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

PASS=0
FAIL=0
WARN=0
pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1" >&2; }
warn() { WARN=$((WARN + 1)); echo "  [WARN] $1"; }
section() { echo ""; echo "=== $1 ==="; }

[ "$(uname -s)" = "Linux" ] || { echo "This test boots real VMs and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — VM creation needs CAP_NET_ADMIN and /var/lib/ephemera access." >&2; exit 1; }

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
ssh-keygen -t ed25519 -N "" -f "${TMP}/key" -C "ephemera-smoketest" >/dev/null
PUBKEY="$(cat "${TMP}/key.pub")"

if [ -z "$IMAGE" ]; then
    IMAGE="${STATE_DIR}/images/ephemera-smoketest.qcow2"
    if [ ! -f "$IMAGE" ]; then
        section "Building a test image (Ubuntu 24.04 cloud image)"
        cat > "${TMP}/build.json" <<JSON
{
  "source": "https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img",
  "output": "${IMAGE}",
  "format": "qcow2"
}
JSON
        eph build-image --spec "${TMP}/build.json" >/dev/null
        pass "test image built: ${IMAGE}"
    fi
fi

json_field() { python3 -c "import json,sys;v=json.load(sys.stdin).get('$1');print(v if v is not None else '')"; }

wait_ssh() {
    local host="$1" port="$2" attempts="${3:-20}"
    for _ in $(seq 1 "$attempts"); do
        if ssh -i "${TMP}/key" -p "$port" -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
               -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
               eph@"$host" 'echo ok' >/dev/null 2>&1; then
            return 0
        fi
        sleep 4
    done
    return 1
}

# create_and_verify LABEL SPEC_FILE PORT [RESOLVE_IP_CMD]
# RESOLVE_IP_CMD, if given, is run after create to discover the guest IP
# (used for TAP/DHCP where the host isn't 127.0.0.1); it receives the MAC
# as $1 and must print the IP or nothing.
create_and_verify() {
    local label="$1" spec="$2" port="$3" resolve="${4:-}"
    local out id tap pid mac host=127.0.0.1

    out=$(eph create --spec "$spec")
    id=$(echo "$out" | json_field id)
    tap=$(echo "$out" | json_field tap_name)
    pid=$(echo "$out" | json_field pid)
    mac=$(python3 -c "import json;print(json.load(open('$spec'))['network'].get('mac',''))")

    if [ -n "$resolve" ]; then
        host=""
        for _ in $(seq 1 30); do
            host=$("$resolve" "$mac")
            [ -n "$host" ] && break
            sleep 3
        done
        if [ -z "$host" ]; then
            fail "${label}: no DHCP lease observed for ${mac} within 90s"
            eph stop "$id" >/dev/null 2>&1 || true
            eph delete "$id" >/dev/null 2>&1 || true
            return
        fi
        pass "${label}: DHCP lease acquired: ${host}"
    fi

    if wait_ssh "$host" "$port"; then
        pass "${label}: SSH reachable at ${host}:${port}"
    else
        fail "${label}: SSH never became reachable at ${host}:${port}"
    fi

    eph stop "$id" >/dev/null
    eph delete "$id"

    if kill -0 "$pid" 2>/dev/null; then
        fail "${label}: VMM process ${pid} still alive after delete"
    else
        pass "${label}: VMM process exited cleanly"
    fi

    if [ -n "$tap" ]; then
        if ip link show "$tap" >/dev/null 2>&1; then
            fail "${label}: tap interface ${tap} leaked after delete"
        else
            pass "${label}: tap interface ${tap} removed"
        fi
    fi
}

section "Test 1: QEMU user-mode NAT + host port forward"
PORT=$(( (RANDOM % 5000) + 20000 ))
cat > "${TMP}/user-net.json" <<JSON
{
  "name": "ephemera-nettest-user",
  "backend": "qemu",
  "image": "${IMAGE}",
  "vcpus": 1,
  "memory_mib": 768,
  "network": {"mode": "user", "forwards": [{"host_port": ${PORT}, "guest_port": 22, "protocol": "tcp"}]},
  "cloud_init": {"hostname": "ephemera-nettest-user", "user": "eph", "ssh_authorized_keys": ["${PUBKEY}"]},
  "ttl_seconds": 300
}
JSON
create_and_verify "user-mode" "${TMP}/user-net.json" "$PORT"

section "Test 2: TAP + bridge (${BRIDGE}) + DHCP"
if ! ip link show "$BRIDGE" >/dev/null 2>&1; then
    warn "bridge ${BRIDGE} does not exist — skipping TAP test (pass --bridge NAME, e.g. --bridge virbr0)"
else
    MAC=$(printf '52:54:00:%02x:%02x:%02x' $((RANDOM % 256)) $((RANDOM % 256)) $((RANDOM % 256)))
    cat > "${TMP}/tap-net.json" <<JSON
{
  "name": "ephemera-nettest-tap",
  "backend": "qemu",
  "image": "${IMAGE}",
  "vcpus": 1,
  "memory_mib": 768,
  "network": {"mode": "tap", "bridge": "${BRIDGE}", "mac": "${MAC}"},
  "cloud_init": {"hostname": "ephemera-nettest-tap", "user": "eph", "ssh_authorized_keys": ["${PUBKEY}"]},
  "ttl_seconds": 300
}
JSON
    resolve_by_neigh() {
        ip neigh show dev "$BRIDGE" 2>/dev/null | awk -v mac="$1" 'tolower($0) ~ tolower(mac) {print $1; exit}'
    }
    create_and_verify "TAP" "${TMP}/tap-net.json" 22 resolve_by_neigh
fi

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}  warn: ${WARN}"
[ "$FAIL" -eq 0 ]
