#!/usr/bin/env bash
# Prepare a fresh Linux host to run Zyvor Ephemera: install QEMU/cloud-init/
# libguestfs tooling via the system package manager, install Cloud Hypervisor
# and Firecracker from upstream releases, create the state directories and an
# optional bridge.
#
# Usage:
#   sudo ./scripts/bootstrap-host.sh [bridge-name]
#
# Env:
#   SKIP_CLOUD_HYPERVISOR=1   don't install Cloud Hypervisor
#   SKIP_FIRECRACKER=1        don't install Firecracker
#   SKIP_BRIDGE=1             don't create a Linux bridge
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BRIDGE="${1:-vmbr0}"

info() { echo "  [*] $*"; }
ok()   { echo "  [ok] $*"; }
warn() { echo "  [WARN] $*" >&2; }

[ "$(uname -s)" = "Linux" ] || { echo "ERROR: this script only supports Linux hosts" >&2; exit 1; }

if [[ ! -e /dev/kvm ]]; then
  echo "ERROR: /dev/kvm is missing. Enable CPU virtualization/KVM first." >&2
  exit 1
fi

SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

info "Installing system packages (qemu, cloud-localds, virt-customize)..."
. /etc/os-release 2>/dev/null || true
if [[ "${ID:-}" == "debian" || "${ID:-}" == "ubuntu" || "${ID_LIKE:-}" == *"debian"* ]]; then
    $SUDO apt-get update -qq
    $SUDO apt-get install -y -qq qemu-system-x86 qemu-utils cloud-image-utils libguestfs-tools iproute2 curl
elif command -v dnf >/dev/null || command -v yum >/dev/null; then
    PKG="$(command -v dnf >/dev/null && echo dnf || echo yum)"
    $SUDO "$PKG" install -y qemu-kvm qemu-img cloud-utils libguestfs-tools-c iproute curl
else
    warn "unrecognized package manager — install qemu-system-x86_64, qemu-img, cloud-localds and virt-customize manually"
fi
ok "system packages ready"

if [ "${SKIP_BRIDGE:-0}" != "1" ]; then
    if ! ip link show "$BRIDGE" >/dev/null 2>&1; then
        $SUDO ip link add name "$BRIDGE" type bridge
        $SUDO ip link set "$BRIDGE" up
        ok "created bridge ${BRIDGE}"
    else
        ok "bridge ${BRIDGE} already exists"
    fi
fi

$SUDO install -d -m 0755 /var/lib/ephemera/{images,kernels,instances,downloads}
$SUDO install -d -m 0755 /run/ephemera
ok "state directories ready under /var/lib/ephemera"

if [ "${SKIP_CLOUD_HYPERVISOR:-0}" != "1" ]; then
    info "Installing Cloud Hypervisor..."
    bash "${SCRIPT_DIR}/install-cloud-hypervisor.sh" || warn "Cloud Hypervisor install failed — see above"
else
    info "Skipping Cloud Hypervisor (SKIP_CLOUD_HYPERVISOR=1)"
fi

if [ "${SKIP_FIRECRACKER:-0}" != "1" ]; then
    info "Installing Firecracker..."
    bash "${SCRIPT_DIR}/install-firecracker.sh" || warn "Firecracker install failed — see above"
else
    info "Skipping Firecracker (SKIP_FIRECRACKER=1)"
fi

echo ""
echo "Host ready. Run ./scripts/preflight.sh to confirm everything is on PATH."
