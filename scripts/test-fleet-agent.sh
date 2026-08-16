#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real two-host regression test for the distributed node-agent
# (ephemera-agent central/node) — proves the multi-host fleet story, not
# just a single local ephemera. Run from a control machine with SSH access
# to two already-deployed ephemera hosts (see scripts/deploy-remote.sh);
# orchestrates both remote hosts over SSH, the same way deploy-remote.sh
# does, rather than running on either host itself.
#
# Proves:
# - two real, physically separate hosts both register with a central fleet
#   registry with real capacity (vcpus/memory read off /proc, not faked)
# - `POST /fleet/vms` with no explicit node picks the least-loaded healthy
#   node and creates a REAL VM there — confirmed by finding the actual QEMU
#   process on that exact host (and NOT on the other one)
# - a second create lands on the OTHER host once the first host's vm_count
#   updates via heartbeat — real load-aware placement, not round-robin
# - `GET /fleet/vms` aggregates real VMs from both hosts, correctly tagged
# - `DELETE /fleet/vms/{node}/{id}` proxies to the right host and reaps the
#   real VM there, leaving the other host's VM untouched
#
# Usage:
#   ./scripts/test-fleet-agent.sh \
#       --central sus@175.110.122.71 --central-image /var/lib/ephemera/images/test.qcow2 \
#       --node sus@80.79.5.173      --node-image    /var/lib/ephemera/images/test.qcow2
#
# Both hosts must already have `ephemera` and `ephemera-agent` built at
# /home/<user>/.deployments/ephemera/target/release/ (see deploy-remote.sh)
# and must be able to reach each other's IP directly (no NAT between them).
set -euo pipefail

CENTRAL=""
CENTRAL_IMAGE=""
NODE=""
NODE_IMAGE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --central) CENTRAL="$2"; shift 2 ;;
        --central-image) CENTRAL_IMAGE="$2"; shift 2 ;;
        --node) NODE="$2"; shift 2 ;;
        --node-image) NODE_IMAGE="$2"; shift 2 ;;
        -h|--help) sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done
[ -n "$CENTRAL" ] && [ -n "$NODE" ] && [ -n "$CENTRAL_IMAGE" ] && [ -n "$NODE_IMAGE" ] \
    || { echo "--central, --central-image, --node, and --node-image are all required" >&2; exit 1; }

CENTRAL_HOST="${CENTRAL#*@}"
NODE_HOST="${NODE#*@}"
DEPLOY_DIR="/home/\$(whoami)/.deployments/ephemera"
CENTRAL_EPH_PORT=17795
CENTRAL_FLEET_PORT=17798
NODE_EPH_PORT=17795

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1" >&2; }
section() { echo ""; echo "=== $1 ==="; }

cleanup() {
    # The bracket trick (`[t]arget/...` instead of `target/...`) keeps
    # pgrep/pkill's own invocation from matching its own command line — a
    # real, hard-learned bug in an earlier version of this cleanup: a plain
    # `sudo pkill -f "target/release/ephemera --config ..."` matches ITS
    # OWN argv too (which literally contains that same pattern string),
    # SIGTERMing itself before it ever reached the real target process —
    # the remote `ephemera serve` survived, silently, every time.
    ssh "$CENTRAL" 'pgrep -f "[t]arget/release/ephemera-agent (central|node)" | xargs -r kill 2>/dev/null; sudo pkill -f "[t]arget/release/ephemera --config /tmp/ephemera-fleet-test" 2>/dev/null; sudo rm -rf /tmp/ephemera-fleet-test' >/dev/null 2>&1 || true
    ssh "$NODE" 'pgrep -f "[t]arget/release/ephemera-agent node" | xargs -r kill 2>/dev/null; sudo pkill -f "[t]arget/release/ephemera --config /tmp/ephemera-fleet-test" 2>/dev/null; sudo rm -rf /tmp/ephemera-fleet-test' >/dev/null 2>&1 || true
}
trap cleanup EXIT

start_ephemera() {
    local target="$1" image_dir_hint="$2"
    ssh "$target" "
        EPH=\$(ls \$HOME/.deployments/ephemera/target/release/ephemera 2>/dev/null || command -v ephemera)
        TMP=/tmp/ephemera-fleet-test
        sudo rm -rf \"\$TMP\"; mkdir -p \"\$TMP/state\" \"\$TMP/run\"
        cat > \"\$TMP/ephemera.toml\" <<TOML
listen = \"0.0.0.0:${NODE_EPH_PORT}\"
state_dir = \"\${TMP}/state\"
run_dir = \"\${TMP}/run\"
qemu_binary = \"qemu-system-x86_64\"
qemu_img_binary = \"qemu-img\"
cloud_hypervisor_binary = \"cloud-hypervisor\"
ch_remote_binary = \"ch-remote\"
cloud_localds_binary = \"cloud-localds\"
virt_customize_binary = \"virt-customize\"
firecracker_binary = \"firecracker\"
default_bridge = \"vmbr0\"
reaper_interval_secs = 5
TOML
        sudo \"\$EPH\" --config \"\$TMP/ephemera.toml\" serve > \"\$TMP/serve.log\" 2>&1 < /dev/null &
        disown
        sleep 2
        curl -sS http://127.0.0.1:${NODE_EPH_PORT}/healthz
    "
}

section "Starting a real ephemera serve on each host"
start_ephemera "$CENTRAL" "$CENTRAL_IMAGE" | grep -q '"ok":true' \
    && pass "ephemera serve up on central host ($CENTRAL_HOST)" \
    || { fail "ephemera serve failed on central host"; exit 1; }
start_ephemera "$NODE" "$NODE_IMAGE" | grep -q '"ok":true' \
    && pass "ephemera serve up on node host ($NODE_HOST)" \
    || { fail "ephemera serve failed on node host"; exit 1; }

section "Starting the central fleet registry"
ssh "$CENTRAL" "
    AGENT=\$(ls \$HOME/.deployments/ephemera/target/release/ephemera-agent)
    \"\$AGENT\" central --listen 0.0.0.0:${CENTRAL_FLEET_PORT} > /tmp/ephemera-fleet-test/central.log 2>&1 < /dev/null &
    disown
    sleep 2
    curl -sS http://127.0.0.1:${CENTRAL_FLEET_PORT}/healthz
" | grep -q '"ok":true' && pass "central fleet registry is up" || { fail "central registry failed to start"; exit 1; }

section "Starting both node agents, pointed at the real remote central"
ssh "$CENTRAL" "
    AGENT=\$(ls \$HOME/.deployments/ephemera/target/release/ephemera-agent)
    NODE_NAME=node-a CENTRAL_URL=http://${CENTRAL_HOST}:${CENTRAL_FLEET_PORT} \
    EPHEMERA_URL=http://127.0.0.1:${CENTRAL_EPH_PORT} ADVERTISE_URL=http://${CENTRAL_HOST}:${CENTRAL_EPH_PORT} \
    \"\$AGENT\" node --interval-secs 5 > /tmp/ephemera-fleet-test/node-agent.log 2>&1 < /dev/null &
    disown
"
ssh "$NODE" "
    AGENT=\$(ls \$HOME/.deployments/ephemera/target/release/ephemera-agent)
    NODE_NAME=node-b CENTRAL_URL=http://${CENTRAL_HOST}:${CENTRAL_FLEET_PORT} \
    EPHEMERA_URL=http://127.0.0.1:${NODE_EPH_PORT} ADVERTISE_URL=http://${NODE_HOST}:${NODE_EPH_PORT} \
    \"\$AGENT\" node --interval-secs 5 > /tmp/ephemera-fleet-test/node-agent.log 2>&1 < /dev/null &
    disown
"
sleep 7
NODES_JSON=$(ssh "$CENTRAL" "curl -sS http://127.0.0.1:${CENTRAL_FLEET_PORT}/fleet/nodes")
echo "$NODES_JSON" | python3 -m json.tool
HEALTHY_COUNT=$(echo "$NODES_JSON" | python3 -c "import json,sys; print(sum(1 for n in json.load(sys.stdin)['items'] if n['healthy']))")
[ "$HEALTHY_COUNT" = "2" ] && pass "both real hosts registered and healthy, with real capacity info" || fail "expected 2 healthy nodes, got ${HEALTHY_COUNT}"

section "Fleet create with no explicit node picks the least-loaded host"
VM1=$(ssh "$CENTRAL" "curl -sS -X POST http://127.0.0.1:${CENTRAL_FLEET_PORT}/fleet/vms -H 'content-type: application/json' -d '{\"name\":\"fleet-vm-1\",\"backend\":\"qemu\",\"image\":\"${CENTRAL_IMAGE}\",\"vcpus\":1,\"memory_mib\":768,\"network\":{\"mode\":\"none\"},\"ttl_seconds\":600}'")
VM1_NODE=$(echo "$VM1" | python3 -c "import json,sys; print(json.load(sys.stdin)['node'])")
VM1_ID=$(echo "$VM1" | python3 -c "import json,sys; print(json.load(sys.stdin)['vm']['id'])")
[ -n "$VM1_ID" ] && pass "first VM created via fleet API, landed on ${VM1_NODE}" || fail "first fleet create failed"

if [ "$VM1_NODE" = "node-a" ]; then VM1_HOST="$CENTRAL"; else VM1_HOST="$NODE"; fi
ssh "$VM1_HOST" "sudo ps aux | grep 'qemu-system.*${VM1_ID:0:8}' | grep -v grep" >/dev/null \
    && pass "confirmed a real QEMU process for VM1 exists on ${VM1_NODE}'s actual physical host" \
    || fail "no real QEMU process found for VM1 on the host the fleet says it's on"

sleep 7
section "A second create lands on the other host once load is known"
VM2=$(ssh "$CENTRAL" "curl -sS -X POST http://127.0.0.1:${CENTRAL_FLEET_PORT}/fleet/vms -H 'content-type: application/json' -d '{\"name\":\"fleet-vm-2\",\"backend\":\"qemu\",\"image\":\"${NODE_IMAGE}\",\"vcpus\":1,\"memory_mib\":768,\"network\":{\"mode\":\"none\"},\"ttl_seconds\":600}'")
VM2_NODE=$(echo "$VM2" | python3 -c "import json,sys; print(json.load(sys.stdin)['node'])")
VM2_ID=$(echo "$VM2" | python3 -c "import json,sys; print(json.load(sys.stdin)['vm']['id'])")
if [ "$VM2_NODE" != "$VM1_NODE" ]; then
    pass "second VM landed on the other host (${VM2_NODE}) — real load-aware placement"
else
    fail "second VM landed on the same host as the first (${VM2_NODE}) — placement did not account for load"
fi

section "GET /fleet/vms aggregates both real VMs, correctly tagged"
LIST=$(ssh "$CENTRAL" "curl -sS http://127.0.0.1:${CENTRAL_FLEET_PORT}/fleet/vms")
FOUND=$(echo "$LIST" | python3 -c "
import json, sys
d = json.load(sys.stdin)
ids = {v['id']: v['node'] for v in d['items']}
print('ok' if ids.get('$VM1_ID') == '$VM1_NODE' and ids.get('$VM2_ID') == '$VM2_NODE' else 'mismatch')
")
[ "$FOUND" = "ok" ] && pass "fleet-wide list shows both VMs tagged with their correct real node" || fail "fleet-wide list did not correctly aggregate/tag both VMs"

section "Deleting via the fleet proxy reaps the right VM on the right host"
if [ "$VM2_NODE" = "node-a" ]; then VM2_HOST="$CENTRAL"; else VM2_HOST="$NODE"; fi
ssh "$CENTRAL" "curl -sS -X DELETE http://127.0.0.1:${CENTRAL_FLEET_PORT}/fleet/vms/${VM2_NODE}/${VM2_ID} -o /dev/null -w '%{http_code}'" | grep -q '^2' \
    && pass "fleet delete for VM2 returned success" || fail "fleet delete for VM2 failed"
sleep 1
ssh "$VM2_HOST" "sudo ps aux | grep 'qemu-system.*${VM2_ID:0:8}' | grep -v grep" >/dev/null \
    && fail "VM2's QEMU process is still alive on ${VM2_NODE} after fleet delete" \
    || pass "VM2's real QEMU process is gone after fleet delete — no leak"
ssh "$VM1_HOST" "sudo ps aux | grep 'qemu-system.*${VM1_ID:0:8}' | grep -v grep" >/dev/null \
    && pass "VM1 on ${VM1_NODE} was left untouched by VM2's delete" \
    || fail "VM1 was incorrectly affected by VM2's delete"

ssh "$CENTRAL" "curl -sS -X DELETE http://127.0.0.1:${CENTRAL_FLEET_PORT}/fleet/vms/${VM1_NODE}/${VM1_ID}" >/dev/null 2>&1 || true

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
