# Zyvor Ephemera

**Disposable Compute Engine** — create secure, isolated, short-lived virtual machines using
Firecracker, Cloud Hypervisor, and QEMU/KVM from one Rust-native control plane.

- **QEMU/KVM** — broad guest/device compatibility, qcow2 CoW overlays, QMP socket.
- **Cloud Hypervisor** — Rust VMM for modern cloud workloads, direct-kernel or firmware boot.
- **Firecracker** — microVM backend using a Linux kernel + raw root filesystem.

It also contains a small **virt-builder-style image pipeline**: use a local/HTTP base image, verify SHA-256, convert/resize it, and customize it with `virt-customize`.

> This repository is a complete MVP/control-plane skeleton, not a finished multi-tenant security boundary. Before exposing it to untrusted tenants, add authentication/RBAC, Firecracker jailer, cgroups, seccomp/AppArmor/SELinux policy, per-tenant network namespaces, quotas, audit logging and stronger image provenance.

## Architecture

```text
                     +-------------------------+
 CLI / REST -------->| Rust VmManager          |
                     | state + TTL reaper      |
                     +------------+------------+
                                  |
               +------------------+------------------+
               |                  |                  |
        +------v------+    +------v-------+   +------v------+
        | QEMU/KVM    |    | Cloud        |   | Firecracker |
        | qcow2 CoW   |    | Hypervisor   |   | raw rootfs  |
        +------+------+    +------+-------+   +------+------+ 
               |                  |                  |
               +---------+--------+------------------+
                         |
              KVM + TAP/bridge + Linux host

Image path:
base image -> SHA256 -> qemu-img -> virt-customize -> reusable template
                                      |
VM launch: template -> disposable clone -> cloud-init -> VMM -> TTL delete
```

## Project layout

The MVP is a Cargo workspace, structured to match Zyvor Ephemera's longer-term
multi-node architecture:

```text
crates/
├── ephemera-core                 domain types, config, VmBackend trait
├── ephemera-storage               VM-record state persistence
├── ephemera-network               TAP/bridge network preparation
├── ephemera-image                 image build/clone + cloud-init seed generation
├── ephemera-qemu                  QEMU/KVM backend
├── ephemera-cloud-hypervisor      Cloud Hypervisor backend
├── ephemera-firecracker           Firecracker backend
├── ephemera-scheduler             VmManager: VM lifecycle orchestration + TTL reaper
├── ephemera-api                   REST API (axum)
├── ephemera-cli                   `ephemera` CLI binary (composition root)
├── ephemera-agent                 reserved: per-host node-agent daemon (multi-node)
└── ephemera-kube                  reserved: Kubernetes DisposableVM CRD/operator
```

`ephemera-agent` and `ephemera-kube` are placeholder crates for the distributed,
Kubernetes-native deployment described below under "Production changes I would
make next" — they are workspace members but contain no functionality yet.

## What is implemented

- Common `VmBackend` Rust trait.
- QEMU backend.
- Cloud Hypervisor backend.
- Firecracker backend using JSON `--config-file`.
- QEMU qcow2 backing overlays for cheap disposable writes.
- Raw reflink copies for Firecracker / Cloud Hypervisor when the host filesystem supports reflinks.
- Raw conversion fallback through `qemu-img`.
- Optional disk growth.
- cloud-init NoCloud seed disk generation.
- TAP interface creation and optional Linux bridge attachment.
- QEMU user-mode networking + host port forwarding.
- VM state persisted to JSON.
- REST API.
- CLI.
- TTL reaper that destroys expired VMs.
- Console log path per VM.
- Control sockets: QMP, Cloud Hypervisor API socket, Firecracker API socket.
- Image download/cache + SHA-256 verification.
- `virt-customize` package/hostname/command/SSH-key customization.
- systemd unit and one-command host bootstrap (installs QEMU tooling, Cloud Hypervisor, and Firecracker).
- SSH/rsync remote deploy script with full and quick profiles.

## Host requirements

Linux x86_64 with virtualization enabled and `/dev/kvm` available.

Typical packages/tools:

```bash
qemu-system-x86_64
qemu-img
cloud-localds
virt-customize
ip
cp
```

Neither Cloud Hypervisor nor Firecracker is packaged by `apt`/`dnf`, so this repo ships installer
scripts that fetch the upstream release binary for your CPU architecture (x86_64 or aarch64) and
verify it against the SHA-256 digest GitHub records for that release asset before installing it.

For Firecracker, provide a compatible uncompressed guest kernel (`vmlinux`) and a Linux rootfs. For Cloud Hypervisor, use either direct kernel boot or firmware boot. The project's Rust Hypervisor Firmware (`hypervisor-fw`) is passed through the request's `kernel` field, matching the Cloud Hypervisor quick-start; `firmware` is reserved for firmware loaded through the VMM's `--firmware` option.

## Prepare host (one command)

On a fresh Linux box, this installs the system packages (`qemu-system-x86_64`, `qemu-img`,
`cloud-localds`, `virt-customize`), Cloud Hypervisor, Firecracker, and Rust Hypervisor Firmware,
then creates the state directories and an optional bridge:

```bash
sudo ./scripts/bootstrap-host.sh vmbr0
```

Skip pieces you don't want with `SKIP_CLOUD_HYPERVISOR=1`, `SKIP_FIRECRACKER=1`, or `SKIP_BRIDGE=1`.
If a VM needs outbound connectivity through a TAP bridge, configure bridge addressing/NAT/DHCP for
your environment yourself — the MVP intentionally does not mutate host firewall/NAT policy.

Run `./scripts/preflight.sh` afterward to confirm every tool is on `PATH`.

### Installing (or updating) a single VMM

```bash
./scripts/install-cloud-hypervisor.sh            # latest release, both cloud-hypervisor + hypervisor-fw
./scripts/install-cloud-hypervisor.sh v53.0       # pin a version
./scripts/install-cloud-hypervisor.sh --no-firmware

./scripts/install-firecracker.sh                  # latest release, firecracker + jailer
./scripts/install-firecracker.sh v1.16.1          # pin a version
```

Both scripts resolve the requested (or latest) GitHub release, download the arch-appropriate
binary, verify its SHA-256 digest, and `install` it to `/usr/local/bin` (override with
`INSTALL_DIR=...`). They are safe to re-run — an already-installed matching version is a no-op.

## Build

Use a current stable Rust toolchain. This is a Cargo workspace; `cargo build` builds every crate,
producing the `ephemera` CLI at `target/release/ephemera`:

```bash
cargo build --release
sudo install -m 0755 target/release/ephemera /usr/local/bin/ephemera
sudo install -m 0644 config.example.toml /etc/ephemera.toml
```

## Deploy to a remote host

`scripts/deploy-remote.sh` does the above end-to-end over SSH: rsync the source, install system
packages + Cloud Hypervisor/Firecracker, install a Rust toolchain if needed, build, and install the
binary, config, and systemd unit.

```bash
./scripts/deploy-remote.sh 80.79.5.173 sus --key   # full deploy, SSH key auth
./scripts/deploy-remote.sh sus@80.79.5.173 --quick  # rsync + build only, skip dep install
./scripts/deploy-remote.sh 80.79.5.173 sus --verify-only
./scripts/deploy-remote.sh --help
```

## Create a QEMU disposable VM

Edit `examples/qemu.json` to point at your base image and SSH public key.

```bash
sudo /usr/local/bin/ephemera --config /etc/ephemera.toml create \
  --spec examples/qemu.json
```

Example behavior:

- base image stays untouched;
- a qcow2 overlay is created for the instance;
- cloud-init seed disk is generated;
- TCP host port 2222 is forwarded to guest port 22;
- the VM automatically expires after 900 seconds.

## Create a Cloud Hypervisor VM

```bash
sudo /usr/local/bin/ephemera --config /etc/ephemera.toml create \
  --spec examples/cloud-hypervisor.json
```

The backend uses a raw per-instance disk. If the base image is already raw and the filesystem supports reflinks, the clone is copy-on-write at the filesystem level.

## Create a Firecracker microVM

```bash
sudo /usr/local/bin/ephemera --config /etc/ephemera.toml create \
  --spec examples/firecracker.json
```

Firecracker does not use BIOS/UEFI in this flow. The request supplies the Linux kernel and the manager supplies a raw block rootfs.

## Build an image like a small virt-builder

```bash
sudo /usr/local/bin/ephemera --config /etc/ephemera.toml build-image \
  --spec examples/build-image.json
```

The `source` can be a local path or an `http(s)` URL. You can add `sha256` to the request to pin the artifact.

Example request:

```json
{
  "source": "https://example.invalid/ubuntu-base.qcow2",
  "sha256": "PUT_REAL_SHA256_HERE",
  "output": "/var/lib/ephemera/images/ubuntu-dev.qcow2",
  "format": "qcow2",
  "size_gib": 20,
  "hostname": "zyvor-template",
  "packages": ["curl", "jq", "qemu-guest-agent"],
  "commands": ["systemctl enable qemu-guest-agent"]
}
```

## REST API

Start the server:

```bash
sudo /usr/local/bin/ephemera --config /etc/ephemera.toml serve
```

Default bind address:

```text
127.0.0.1:7788
```

Endpoints:

```text
GET    /healthz
POST   /v1/vms
GET    /v1/vms
GET    /v1/vms/{uuid}
POST   /v1/vms/{uuid}/stop
DELETE /v1/vms/{uuid}
POST   /v1/images/build
```

Create through REST:

```bash
curl -sS http://127.0.0.1:7788/v1/vms \
  -H 'content-type: application/json' \
  --data-binary @examples/qemu.json | jq
```

## VM JSON contract

```json
{
  "name": "job-123",
  "backend": "qemu",
  "image": "/var/lib/ephemera/images/ubuntu.qcow2",
  "vcpus": 2,
  "memory_mib": 2048,
  "disk_size_gib": 20,
  "network": {
    "mode": "user",
    "forwards": [
      {"host_port": 2222, "guest_port": 22, "protocol": "tcp"}
    ]
  },
  "cloud_init": {
    "hostname": "job-123",
    "user": "zyvor",
    "ssh_authorized_keys": ["ssh-ed25519 AAAA..."],
    "packages": ["curl"],
    "runcmd": ["echo hello > /tmp/hello"]
  },
  "ttl_seconds": 600,
  "extra_args": []
}
```

### Networking modes

`none`:

```json
{"mode":"none"}
```

QEMU user networking:

```json
{
  "mode":"user",
  "forwards":[{"host_port":2222,"guest_port":22,"protocol":"tcp"}]
}
```

TAP/bridge (all VMMs):

```json
{
  "mode":"tap",
  "bridge":"vmbr0",
  "mac":"06:00:AC:10:00:02"
}
```

When `tap_name` is omitted, the manager creates one from the VM UUID.

## State layout

```text
/var/lib/ephemera/
  vms.json
  downloads/
  images/
  kernels/
  instances/
    <uuid>/
      root.qcow2 | root.raw
      seed.img
      user-data
      meta-data
      console.log
      qmp.sock | ch-api.sock | firecracker.sock
      firecracker.json
```

## Production changes I would make next

1. **Firecracker jailer backend** — chroot, uid/gid isolation, cgroups, resource limits.
2. **Network namespaces** — one namespace per VM, veth/TAP, nftables, DHCP/IPAM.
3. **Vsock agent** — a small guest agent for readiness, command execution, file copy and clean shutdown.
4. **Snapshot pools** — Firecracker snapshots and warm VM pools for sub-second job start.
5. **Storage abstraction** — qcow2, raw reflink, LVM thin, Ceph RBD, NVMe local, NBD.
6. **Image catalog** — signed template manifests, distro/version/arch aliases, cosign/Sigstore or your own signing policy.
7. **Policy** — max vCPU/RAM/disk/TTL, allowed VMMs, allowed images, allowed networking.
8. **Auth** — mTLS/OIDC, RBAC, tenant IDs and audit events.
9. **Observability** — Prometheus metrics, tracing, per-VM boot timing and failure reasons.
10. **Kubernetes CRD/operator** — `DisposableVM` CRD backed by node-local daemonsets (`ephemera-kube`).
11. **Distributed node-agent** — `ephemera-agent` running per hypervisor host, reporting to a central `ephemera-scheduler`.
12. **Scheduler placement** — NUMA awareness, CPU pinning, hugepages and GPU/VFIO assignment.
13. **Windows path** — QEMU/Cloud Hypervisor only; UEFI, virtio-win injection, sysprep and unattend support.

## Important limitations in this MVP

- QEMU user networking is supported; Cloud Hypervisor and Firecracker require TAP or no networking.
- TAP setup requires host network privilege.
- The bridge must already be configured for the network behavior you want.
- Firecracker image preparation is stricter than QEMU because it boots a kernel/rootfs directly.
- Guest disk partition/filesystem expansion after `qemu-img resize` is an image/guest concern. Use cloud-init growpart or your image pipeline.
- `extra_args` is intentionally an administrator escape hatch. Do not expose it to untrusted tenants.
- The API is localhost-only by default and has no authentication in this MVP.

## License

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). Copyright 2026 Zyvor.
