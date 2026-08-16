#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# Real-cluster regression test for the DisposableVm CRD + node-local
# operator (ephemera-kube). Boots a real QEMU VM by creating a Kubernetes
# custom resource, not by calling the REST API directly.
#
# Proves:
# - `ephemera-kube --print-crd` produces a CRD that `kubectl apply` accepts
# - creating a DisposableVm targeting this node reconciles into a real VM
#   (verified via the local ephemera REST API, not just "the CR looks ok")
# - `kubectl delete disposablevm` blocks on the finalizer and only
#   completes once the real VM is actually gone (no leaked QEMU process)
# - self-healing: deleting the underlying VM out-of-band (bypassing the CR
#   entirely) gets it replaced with a genuinely new VM (different id/pid)
#   within a couple of reconcile ticks, with no action needed on the CR
#
# Requires a real Kubernetes cluster with a working KUBECONFIG (k3s is what
# this was developed and verified against — a lightweight single-node k3s
# is enough). This script installs and removes the CRD itself; it does not
# touch anything else already in the cluster.
#
# Usage:
#   sudo KUBECONFIG=/path/to/kubeconfig ./scripts/test-kube-operator.sh \
#       --image /var/lib/ephemera/images/ephemera-lifecycle-test.qcow2
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

IMAGE=""
BASE_URL="http://127.0.0.1:17796"

while [ $# -gt 0 ]; do
    case "$1" in
        --image) IMAGE="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || { echo "This test boots a real VM and requires a Linux/KVM host." >&2; exit 1; }
[ -e /dev/kvm ] || { echo "/dev/kvm missing — enable virtualization first." >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "Run as root (sudo) — VM creation needs /var/lib/ephemera access." >&2; exit 1; }
[ -n "$IMAGE" ] && [ -f "$IMAGE" ] || { echo "--image is required and must exist" >&2; exit 1; }
command -v kubectl >/dev/null 2>&1 || { echo "kubectl not found on PATH" >&2; exit 1; }
kubectl get nodes >/dev/null 2>&1 || { echo "kubectl can't reach a cluster — check KUBECONFIG" >&2; exit 1; }

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
KUBE="${EPHEMERA_KUBE_BIN:-}"
if [ -z "$KUBE" ]; then
    if [ -x "${PROJECT_DIR}/target/release/ephemera-kube" ]; then
        KUBE="${PROJECT_DIR}/target/release/ephemera-kube"
    else
        echo "ephemera-kube binary not found. Build it (cargo build --release -p ephemera-kube) or set EPHEMERA_KUBE_BIN." >&2
        exit 1
    fi
fi

NODE_NAME="$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')"
[ -n "$NODE_NAME" ] || { echo "could not determine a node name from 'kubectl get nodes'" >&2; exit 1; }

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1" >&2; }
info() { echo "  [INFO] $1"; }
section() { echo ""; echo "=== $1 ==="; }

TMP="$(mktemp -d)"
SERVE_PID=""
OPERATOR_PID=""
CRD_APPLIED=""
CR_APPLIED=""
cleanup() {
    [ -n "$CR_APPLIED" ] && kubectl delete disposablevm kube-test-vm --timeout=30s >/dev/null 2>&1 || true
    [ -n "$OPERATOR_PID" ] && { kill "$OPERATOR_PID" >/dev/null 2>&1 || true; wait "$OPERATOR_PID" 2>/dev/null || true; }
    [ -n "$SERVE_PID" ] && { kill "$SERVE_PID" >/dev/null 2>&1 || true; wait "$SERVE_PID" 2>/dev/null || true; }
    [ -n "$CRD_APPLIED" ] && kubectl delete crd disposablevms.ephemera.zyvor.io >/dev/null 2>&1 || true
    rm -rf "$TMP"
}
trap cleanup EXIT

json_field() { python3 -c "import json,sys;v=json.load(sys.stdin).get('$1');print(v if v is not None else '')"; }

section "CRD generated from the real type is accepted by the cluster"
"$KUBE" --print-crd > "${TMP}/crd.json"
kubectl apply -f "${TMP}/crd.json"
CRD_APPLIED=1
pass "kubectl accepted the generated DisposableVm CRD"

mkdir -p "${TMP}/state" "${TMP}/run"
cat > "${TMP}/ephemera.toml" <<TOML
listen = "127.0.0.1:17796"
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

section "Starting a local ephemera serve + the ephemera-kube operator"
"$EPH" --config "${TMP}/ephemera.toml" serve > "${TMP}/serve.log" 2>&1 &
SERVE_PID=$!
sleep 2
kill -0 "$SERVE_PID" 2>/dev/null || { fail "ephemera serve failed to start"; cat "${TMP}/serve.log" >&2; exit 1; }
pass "ephemera serve is up on ${BASE_URL}"

NODE_NAME="$NODE_NAME" EPHEMERA_URL="$BASE_URL" RUST_LOG=info "$KUBE" > "${TMP}/operator.log" 2>&1 &
OPERATOR_PID=$!
sleep 2
kill -0 "$OPERATOR_PID" 2>/dev/null || { fail "ephemera-kube failed to start"; cat "${TMP}/operator.log" >&2; exit 1; }
pass "ephemera-kube operator is running (node=${NODE_NAME})"

section "Creating a DisposableVm reconciles into a real VM"
cat > "${TMP}/dvm.yaml" <<YAML
apiVersion: ephemera.zyvor.io/v1
kind: DisposableVm
metadata:
  name: kube-test-vm
spec:
  node: ${NODE_NAME}
  backend: qemu
  image: ${IMAGE}
  vcpus: 1
  memoryMib: 768
  networkMode: none
  ttlSeconds: 600
YAML
kubectl apply -f "${TMP}/dvm.yaml"
CR_APPLIED=1

VM_ID=""
for _ in $(seq 1 20); do
    PHASE=$(kubectl get disposablevm kube-test-vm -o jsonpath='{.status.phase}' 2>/dev/null || true)
    VM_ID=$(kubectl get disposablevm kube-test-vm -o jsonpath='{.status.vmId}' 2>/dev/null || true)
    [ "$PHASE" = "Running" ] && [ -n "$VM_ID" ] && break
    sleep 2
done
[ "$PHASE" = "Running" ] && [ -n "$VM_ID" ] && pass "CR reconciled to phase=Running with a real vmId" || fail "CR never reached Running (phase=${PHASE})"

if curl -sS "${BASE_URL}/v1/vms/${VM_ID}" | python3 -c "import json,sys; d=json.load(sys.stdin); exit(0 if d.get('status')=='running' and d.get('pid') else 1)"; then
    pass "the VM the CR points at is a real, running VM with a live pid"
else
    fail "GET /v1/vms/${VM_ID} did not show a running VM"
fi

section "Self-healing: deleting the VM out-of-band gets it replaced"
curl -sS -X DELETE "${BASE_URL}/v1/vms/${VM_ID}" -o /dev/null -w '%{http_code}\n' | grep -q '^2' \
    && pass "out-of-band delete of the underlying VM succeeded" \
    || fail "out-of-band delete of the underlying VM failed"

NEW_VM_ID=""
for _ in $(seq 1 20); do
    NEW_VM_ID=$(kubectl get disposablevm kube-test-vm -o jsonpath='{.status.vmId}' 2>/dev/null || true)
    [ -n "$NEW_VM_ID" ] && [ "$NEW_VM_ID" != "$VM_ID" ] && break
    sleep 2
done
if [ -n "$NEW_VM_ID" ] && [ "$NEW_VM_ID" != "$VM_ID" ]; then
    pass "operator noticed the VM was gone and created a genuinely new one (${NEW_VM_ID})"
else
    fail "operator did not recreate the VM after it was deleted out-of-band"
fi
VM_ID="$NEW_VM_ID"

section "Deleting the DisposableVm blocks on the finalizer and reaps the real VM"
kubectl delete disposablevm kube-test-vm --timeout=30s
CR_APPLIED=""
if [ -n "$VM_ID" ] && curl -sS "${BASE_URL}/v1/vms/${VM_ID}" | grep -q "not found"; then
    pass "the real VM is gone after the CR was deleted (finalizer-driven cleanup)"
else
    fail "the real VM still exists after the CR was deleted — finalizer cleanup did not work"
fi
if ! pgrep -f "qemu-system.*${TMP}" >/dev/null 2>&1; then
    pass "no leaked QEMU process under this test's workspace"
else
    fail "a QEMU process from this test is still running"
fi

section "Summary"
echo "  pass: ${PASS}  fail: ${FAIL}"
[ "$FAIL" -eq 0 ]
