#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

# ─────────────────────────────────────────────────────────────
# Zyvor Ephemera — Remote deployment (SSH + rsync)
#
# Profiles:
#   default     Sync source → install deps → build on remote → verify
#   --quick     Rsync + remote build only (skip system deps / rustup)
#   --quick --build-local   Rsync pre-built Linux binary (build locally first)
#
# Auth: SSH keys (recommended). Password via sshpass is supported but deprecated.
#
# Post-deploy: remote scripts/preflight.sh
# ─────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
VERSION="1.0.0"
REMOTE_DIR=""
DEPLOY_PROFILE="full"
DEPLOY_LOG="${EPHEMERA_DEPLOY_LOG:-${HOME}/.ephemera/deploy-$(date +%Y%m%d-%H%M%S).log}"

QUICK_MODE=false
UNINSTALL=false
KEY_AUTH=false
DRY_RUN=false
SKIP_SYNC=false
SKIP_VERIFY=false
BUILD_LOCAL=false
VERIFY_ONLY=false
PREFLIGHT_ONLY=false
VERBOSE=false
SSH_RETRIES="${EPHEMERA_SSH_RETRIES:-3}"
POSITIONAL=()

usage() {
    cat <<EOF
Zyvor Ephemera remote deploy v${VERSION}

Usage:
  $0 <host> <user> [options]
  $0 user@host [options]

Profiles:
  (default)     Full remote build + system deps (qemu, cloud-utils)
  --quick       Rsync + cargo build on remote (skip dep install)
  --quick --build-local   Install locally built target/release/ephemera (Linux only)

Options:
  --help              Show this help
  --dry-run           Print steps without SSH/rsync/build
  --preflight-only    SSH + disk/sudo checks, then exit
  --verify-only       Run remote scripts/preflight.sh only (no deploy)
  --skip-sync         Skip rsync (sources already on host)
  --skip-verify       Skip remote preflight check
  --build-local       With --quick: use local release binary (Linux host required)
  --key               SSH key auth (clear password)
  --uninstall         Remove ephemera from host
  -v, --verbose       Verbose rsync

Environment:
  EPHEMERA_DEPLOY_LOG    Log file path
  EPHEMERA_SSH_RETRIES   SSH retry count (default: 3)
  DEPLOY_DIR             Override remote staging dir (default: ~/.deployments/ephemera)

Examples:
  $0 10.0.0.5 deploy --key
  $0 deploy@10.0.0.5 --quick
  $0 10.0.0.5 root --build-local --quick
  $0 10.0.0.5 deploy --verify-only
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help)        usage; exit 0 ;;
        --quick)          QUICK_MODE=true; DEPLOY_PROFILE="quick"; shift ;;
        --uninstall)      UNINSTALL=true; shift ;;
        --key)            KEY_AUTH=true; shift ;;
        --dry-run)        DRY_RUN=true; shift ;;
        --skip-sync)      SKIP_SYNC=true; shift ;;
        --skip-verify)    SKIP_VERIFY=true; shift ;;
        --build-local)    BUILD_LOCAL=true; shift ;;
        --verify-only)    VERIFY_ONLY=true; shift ;;
        --preflight-only) PREFLIGHT_ONLY=true; shift ;;
        -v|--verbose)     VERBOSE=true; shift ;;
        *)
            POSITIONAL+=("$1")
            shift
            ;;
    esac
done

TARGET_HOST="${POSITIONAL[0]:-}"
TARGET_USER="${POSITIONAL[1]:-root}"
TARGET_PASS="${POSITIONAL[2]:-}"

if [ "$KEY_AUTH" = true ]; then
    TARGET_PASS=""
fi

if [[ -n "${TARGET_HOST}" && "${TARGET_HOST}" == *"@"* ]]; then
    TARGET_USER="${TARGET_HOST%%@*}"
    TARGET_HOST="${TARGET_HOST#*@}"
fi

_use_color() { [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; }
if _use_color; then
    C_OK=$'\033[32m'; C_FAIL=$'\033[31m'; C_INFO=$'\033[36m'; C_WARN=$'\033[33m'
    C_DIM=$'\033[2m'; C_BOLD=$'\033[1m'; C_MAG=$'\033[35m'; C_CYAN=$'\033[96m'; C_RST=$'\033[0m'
else
    C_OK= C_FAIL= C_INFO= C_WARN= C_DIM= C_BOLD= C_MAG= C_CYAN= C_RST=
fi

_log_file() { mkdir -p "$(dirname "$DEPLOY_LOG")" 2>/dev/null || true; echo "[$(date -Iseconds)] $*" >>"$DEPLOY_LOG" 2>/dev/null || true; }
ok()   { echo "${C_OK}  [OK] $*${C_RST}"; _log_file "OK $*"; }
fail() { echo "${C_FAIL}  [FAIL] $*${C_RST}" >&2; _log_file "FAIL $*"; exit 1; }
info() { echo "${C_INFO}  [INFO] $*${C_RST}"; _log_file "INFO $*"; }
warn() { echo "${C_WARN}  [WARN] $*${C_RST}"; _log_file "WARN $*"; }
dry()  { echo "${C_MAG}  [DRY] $*${C_RST}"; _log_file "DRY $*"; }

profile_label() {
    if [ "$UNINSTALL" = true ]; then echo "uninstall"; return; fi
    if [ "$VERIFY_ONLY" = true ]; then echo "verify-only"; return; fi
    if [ "$PREFLIGHT_ONLY" = true ]; then echo "preflight"; return; fi
    echo "${DEPLOY_PROFILE}"
}

print_banner() {
    echo ""
    echo "${C_CYAN}${C_BOLD}  == Zyvor Ephemera Remote Deploy v${VERSION} ==${C_RST}"
    echo "  target: ${C_BOLD}${TARGET_USER}@${TARGET_HOST}${C_RST}  profile: $(profile_label)"
    [ "$DRY_RUN" = true ] && echo "${C_MAG}  DRY-RUN — no remote changes${C_RST}"
    echo ""
}

STEP_T0=0
STEP_IDX=0

step_begin() {
    STEP_IDX=$((STEP_IDX + 1))
    STEP_T0=$(date +%s)
    echo ""
    echo "${C_BOLD}${C_CYAN}  -- Step ${STEP_IDX}: $* --${C_RST}"
    _log_file "STEP ${STEP_IDX}: $*"
}

step_end() {
    echo "${C_OK}  done in $(( $(date +%s) - STEP_T0 ))s${C_RST}"
}

run_step() {
    step_begin "$1"; shift
    if [ "$DRY_RUN" = true ]; then dry "would run: $*"; step_end; return 0; fi
    "$@"; step_end
}

SSH_OPTS="-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=15 -o ServerAliveInterval=30"
if [ -z "${TARGET_PASS}" ]; then
    SSH_OPTS+=" -o BatchMode=yes -o PreferredAuthentications=publickey"
fi

_ssh_once() {
    if [ -n "${TARGET_PASS}" ] && command -v sshpass &>/dev/null; then
        export SSHPASS="${TARGET_PASS}"
        sshpass -e ssh ${SSH_OPTS} "${TARGET_USER}@${TARGET_HOST}" "$@"
    else
        ssh ${SSH_OPTS} "${TARGET_USER}@${TARGET_HOST}" "$@"
    fi
}

_ssh() {
    local attempt=1 max="${SSH_RETRIES}"
    while [ "$attempt" -le "$max" ]; do
        if _ssh_once "$@"; then
            return 0
        fi
        attempt=$((attempt + 1))
        if [ "$attempt" -le "$max" ]; then
            local _d=$(( 2 * (attempt - 1) )); _d=$(( _d < 2 ? 2 : _d > 30 ? 30 : _d ))
            warn "SSH retry ${attempt}/${max}" && sleep "${_d}"
        fi
    done
    return 1
}

_rsync() {
    local opts="-az --delete"
    [ "$VERBOSE" = true ] && opts+=" --progress"
    if [ -n "${TARGET_PASS}" ] && command -v sshpass &>/dev/null; then
        export SSHPASS="${TARGET_PASS}"
        rsync ${opts} -e "sshpass -e ssh ${SSH_OPTS}" "$@"
    else
        rsync ${opts} -e "ssh ${SSH_OPTS}" "$@"
    fi
}

validate() {
    [ -n "${TARGET_HOST}" ] || { usage; exit 1; }
    [ -f "${PROJECT_DIR}/Cargo.toml" ] || fail "Not in ephemera repo: ${PROJECT_DIR}"
    if [ -n "${TARGET_PASS}" ]; then
        warn "Password auth is deprecated. Prefer: ssh-copy-id ${TARGET_USER}@${TARGET_HOST}"
        command -v sshpass &>/dev/null || fail "sshpass required for password auth (dnf/apt install sshpass)"
    fi
}

check_connectivity() {
    info "SSH -> ${TARGET_USER}@${TARGET_HOST}  log: ${DEPLOY_LOG}"
    if [ "$DRY_RUN" = true ]; then
        REMOTE_DIR="${DEPLOY_DIR:-${HOME}/.deployments/ephemera}"
        return 0
    fi
    _ssh "echo ok" &>/dev/null || fail "SSH failed — try: ssh-copy-id ${TARGET_USER}@${TARGET_HOST}"
    ok "SSH connected"
    local remote_home
    remote_home=$(_ssh "echo \$HOME" 2>/dev/null | tr -d '\r')
    remote_home="${remote_home:-/home/${TARGET_USER}}"
    REMOTE_DIR="${DEPLOY_DIR:-${remote_home}/.deployments/ephemera}"
    info "Remote path: ${REMOTE_DIR}"
}

preflight_remote() {
    info "Preflight on ${TARGET_HOST}..."
    if [ "$DRY_RUN" = true ]; then return 0; fi
    _ssh bash <<'REMOTE' || fail "Preflight failed"
set -e
echo "  host: $(hostname -f 2>/dev/null || hostname)"
echo "  os:   $(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME" || uname -s)"
echo "  arch: $(uname -m)"
echo "  mem:  $(free -h 2>/dev/null | awk '/^Mem:/{print $2}' || echo n/a)"
echo "  disk: $(df -h / 2>/dev/null | awk 'NR==2{print $4 " free on " $1}' || echo n/a)"
if [ -e /dev/kvm ]; then
    echo "  kvm:  /dev/kvm present"
else
    echo "  WARNING: /dev/kvm missing — enable virtualization or ephemera VMs will fail to launch"
fi
AVAIL=$(df -BG / 2>/dev/null | awk 'NR==2{gsub(/G/,"",$4); print $4}' || echo 99)
if [ "${AVAIL}" -lt 4 ] 2>/dev/null; then
    echo "  WARNING: less than 4G free on / — release build may fail"
fi
if [ "$(id -u)" -ne 0 ]; then
    if ! sudo -n true 2>/dev/null; then
        echo "  ERROR: non-root user needs passwordless sudo for package install / binary install"
        exit 1
    fi
    echo "  passwordless sudo: OK"
else
    echo "  running as root: OK"
fi
command -v curl >/dev/null && echo "  curl: OK" || echo "  WARNING: curl missing (needed for rustup)"
REMOTE
    ok "Preflight passed"
}

build_local_artifacts() {
    step_begin "Local build (release)"
    if [ "$DRY_RUN" = true ]; then
        dry "would run: cargo build --release -p ephemera-cli -p ephemera-guest-agent"
        return 0
    fi
    if [ "$(uname -s)" != "Linux" ]; then
        fail "--build-local requires a Linux build host (same arch as remote). Use full deploy without --build-local."
    fi
    (cd "${PROJECT_DIR}" && cargo build --release -p ephemera-cli -p ephemera-guest-agent)
    [ -f "${PROJECT_DIR}/target/release/ephemera" ] || fail "target/release/ephemera missing"
    ok "Local binary ready"
    step_end
}

sync_files() {
    if [ "$SKIP_SYNC" = true ]; then
        info "Skipping rsync (--skip-sync)"
        return 0
    fi
    _ssh "mkdir -p '${REMOTE_DIR}'"
    local excludes=(
        --exclude '.git'
        --exclude 'target'
        --exclude '*.qcow2' --exclude '*.raw' --exclude '*.img'
        --exclude '*.iso' --exclude '*.vmdk'
    )
    _rsync "${excludes[@]}" "${PROJECT_DIR}/" "${TARGET_USER}@${TARGET_HOST}:${REMOTE_DIR}/"
    ok "Source synced to ${REMOTE_DIR}"

    # ephemera-image depends on guestkit via a relative sibling path
    # (../../../guestkit from crates/ephemera-image) — it has to land at the
    # same relative depth next to REMOTE_DIR for that path dependency to
    # resolve on the remote host.
    local guestkit_local="${PROJECT_DIR}/../guestkit"
    if [ -d "$guestkit_local" ]; then
        local guestkit_remote
        guestkit_remote="$(dirname "$REMOTE_DIR")/guestkit"
        _ssh "mkdir -p '${guestkit_remote}'"
        _rsync --exclude '.git' --exclude 'target' "${guestkit_local}/" "${TARGET_USER}@${TARGET_HOST}:${guestkit_remote}/"
        ok "guestkit (sibling path dependency) synced to ${guestkit_remote}"
    else
        warn "no sibling guestkit checkout found at ${guestkit_local} — the build will fail if ephemera-image needs it"
    fi
}

sync_binary_only() {
    local bin="${PROJECT_DIR}/target/release/ephemera"
    [ -f "$bin" ] || fail "Missing $bin — run with --build-local after building on Linux"
    _ssh "mkdir -p '${REMOTE_DIR}/bin'"
    _rsync "$bin" "${TARGET_USER}@${TARGET_HOST}:${REMOTE_DIR}/bin/ephemera"
    _rsync "${PROJECT_DIR}/config.example.toml" "${TARGET_USER}@${TARGET_HOST}:${REMOTE_DIR}/config.example.toml"
    _rsync "${PROJECT_DIR}/systemd/ephemera.service" "${TARGET_USER}@${TARGET_HOST}:${REMOTE_DIR}/ephemera.service"
    ok "Release binary synced"
}

install_system_deps() {
    _ssh bash <<'REMOTE'
set -euo pipefail
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

# Prefer apt-get on Debian/Ubuntu even if dnf is installed as an extra package
. /etc/os-release 2>/dev/null || true
_ID="${ID:-}" _ID_LIKE="${ID_LIKE:-}"
if [[ "$_ID" == "debian" || "$_ID" == "ubuntu" || "$_ID_LIKE" == *"debian"* || "$_ID_LIKE" == *"ubuntu"* ]]; then
    PKG=apt-get
    $SUDO apt-get update -qq
elif command -v dnf &>/dev/null; then
    PKG=dnf
elif command -v yum &>/dev/null; then
    PKG=yum
elif command -v apt-get &>/dev/null; then
    PKG=apt-get
    $SUDO apt-get update -qq
else
    echo "ERROR: unsupported package manager"
    exit 1
fi

if [ "$PKG" = "apt-get" ]; then
    $SUDO apt-get install -y -qq \
        qemu-system-x86 qemu-utils cloud-image-utils \
        iproute2 build-essential pkg-config curl git
else
    $SUDO "$PKG" install -y \
        qemu-kvm qemu-img cloud-utils \
        iproute gcc make openssl-devel pkg-config curl git
fi

# guestkit (used by `ephemera build-image`'s image customization) mounts
# qcow2/raw images via qemu-nbd, which needs the nbd kernel module loaded.
$SUDO modprobe nbd max_part=16 2>/dev/null || echo "WARN: modprobe nbd failed — image customization will not work until the nbd module is loaded" >&2

echo "System dependencies installed"
REMOTE
}

install_vmms() {
    _ssh env REMOTE_STAGING="${REMOTE_DIR}" bash <<'REMOTE'
set -e
cd "${REMOTE_STAGING}"
bash scripts/install-cloud-hypervisor.sh || echo "  [WARN] Cloud Hypervisor install failed — continuing without it"
bash scripts/install-firecracker.sh || echo "  [WARN] Firecracker install failed — continuing without it"
REMOTE
}

ensure_rust_remote() {
    _ssh bash <<'REMOTE'
set -e
if command -v cargo &>/dev/null; then
    echo "Rust: $(rustc --version 2>/dev/null || true)"
    exit 0
fi
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
echo "Rust installed: $(rustc --version)"
REMOTE
}

build_install_remote() {
    _ssh env REMOTE_STAGING="${REMOTE_DIR}" bash <<'REMOTE'
set -e
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"
source "$HOME/.cargo/env" 2>/dev/null || true
cd "${REMOTE_STAGING}"
cargo build --release -p ephemera-cli -p ephemera-guest-agent 2>&1 | tail -8
$SUDO install -m755 target/release/ephemera /usr/local/bin/ephemera
[ -f /etc/ephemera.toml ] || $SUDO install -m644 config.example.toml /etc/ephemera.toml
$SUDO install -m644 systemd/ephemera.service /etc/systemd/system/ephemera.service
$SUDO systemctl daemon-reload 2>/dev/null || true
echo "Installed: $(ephemera --version 2>/dev/null || echo ok)"
REMOTE
}

install_binary_quick() {
    _ssh env REMOTE_STAGING="${REMOTE_DIR}" bash <<'REMOTE'
set -e
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"
$SUDO install -m755 "${REMOTE_STAGING}/bin/ephemera" /usr/local/bin/ephemera
[ -f /etc/ephemera.toml ] || $SUDO install -m644 "${REMOTE_STAGING}/config.example.toml" /etc/ephemera.toml
$SUDO install -m644 "${REMOTE_STAGING}/ephemera.service" /etc/systemd/system/ephemera.service
$SUDO systemctl daemon-reload 2>/dev/null || true
echo "Installed: $(ephemera --version 2>/dev/null || echo ok)"
REMOTE
}

verify_remote() {
    info "Running scripts/preflight.sh on ${TARGET_HOST}..."
    if [ "$DRY_RUN" = true ]; then
        dry "would run: bash ${REMOTE_DIR}/scripts/preflight.sh"
        return 0
    fi
    if _ssh_once "bash '${REMOTE_DIR}/scripts/preflight.sh'"; then
        ok "preflight passed"
    else
        warn "preflight reported missing tools (ephemera is installed; see above)"
        return 0
    fi
}

do_uninstall() {
    _ssh env REMOTE_STAGING="${REMOTE_DIR}" bash <<'REMOTE'
set -e
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"
$SUDO systemctl stop ephemera 2>/dev/null || true
$SUDO systemctl disable ephemera 2>/dev/null || true
$SUDO rm -f /usr/local/bin/ephemera /etc/systemd/system/ephemera.service
rm -rf "${REMOTE_STAGING}"
echo "ephemera removed (config /etc/ephemera.toml and /var/lib/ephemera left in place)"
REMOTE
    ok "Uninstalled on ${TARGET_HOST}"
}

deploy_profile_full() {
    run_step "Sync sources" sync_files
    run_step "System dependencies" install_system_deps
    run_step "Cloud Hypervisor / Firecracker" install_vmms
    run_step "Rust toolchain" ensure_rust_remote
    run_step "Build and install" build_install_remote
}

deploy_profile_quick() {
    if [ "$BUILD_LOCAL" = true ]; then
        build_local_artifacts
        run_step "Sync binary" sync_binary_only
        run_step "Install binary" install_binary_quick
    else
        run_step "Sync sources" sync_files
        run_step "Build and install" build_install_remote
    fi
}

print_deployment_summary() {
    echo ""
    echo "${C_OK}${C_BOLD}  Deploy complete — ${TARGET_USER}@${TARGET_HOST}${C_RST}"
    echo "  log:     ${DEPLOY_LOG}"
    echo "  remote:  ${REMOTE_DIR}"
    echo ""
    echo "  ssh ${TARGET_USER}@${TARGET_HOST}"
    echo "  ephemera --config /etc/ephemera.toml create --spec examples/qemu.json"
    echo "  ephemera --config /etc/ephemera.toml serve"
    echo "  bash ${REMOTE_DIR}/scripts/preflight.sh"
    echo ""
}

main() {
    print_banner
    validate
    check_connectivity
    preflight_remote

    if [ "$PREFLIGHT_ONLY" = true ]; then
        ok "Preflight-only complete"
        exit 0
    fi

    if [ "$UNINSTALL" = true ]; then
        run_step "Uninstall ephemera" do_uninstall
        exit 0
    fi

    if [ "$VERIFY_ONLY" = true ]; then
        [ "$SKIP_VERIFY" != true ] && run_step "Verify" verify_remote
        print_deployment_summary
        exit 0
    fi

    if [ "$BUILD_LOCAL" = true ] && [ "$QUICK_MODE" != true ]; then
        warn "--build-local is intended with --quick; ignoring for full profile"
        BUILD_LOCAL=false
    fi

    case "${DEPLOY_PROFILE}" in
        quick) deploy_profile_quick ;;
        *)     deploy_profile_full ;;
    esac

    [ "$SKIP_VERIFY" != true ] && run_step "Verify (preflight)" verify_remote
    print_deployment_summary
}

main "$@"
