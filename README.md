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
├── ephemera-guest-protocol        wire types shared by the guest agent and its host client
├── ephemera-guest-agent           in-guest AF_VSOCK agent binary (ping/exec/shutdown)
├── ephemera-vsock-client          host-side vsock dialing (native for QEMU, UDS proxy for CH/Firecracker)
├── ephemera-scheduler             VmManager: VM lifecycle orchestration + TTL reaper
├── ephemera-api                   REST API (axum)
├── ephemera-cli                   `ephemera` CLI binary (composition root)
├── ephemera-agent                 reserved: per-host node-agent daemon (multi-node)
└── ephemera-kube                  reserved: Kubernetes DisposableVM CRD/operator
```

`ephemera-agent` (a distinct concept from `ephemera-guest-agent` above — this one
is the future per-*host* node-agent for multi-node deployments) and `ephemera-kube`
are placeholder crates for the distributed, Kubernetes-native deployment described
below under "Production changes I would make next" — they are workspace members
but contain no functionality yet.

This project also depends on the sibling [`guestkit`](../guestkit) project (a
pure-Rust, qemu-nbd-based disk toolkit) as a path dependency from `ephemera-image`,
for injecting files into an offline image without needing libguestfs/`virt-customize`
for that step — see "Build an image" below.

## What is implemented

- Common `VmBackend` Rust trait: launch, pause, resume, graceful shutdown.
- QEMU backend, pause/resume/shutdown via QMP.
- Cloud Hypervisor backend, pause/resume/shutdown via `ch-remote`.
- Firecracker backend using JSON `--config-file`, pause/resume via `PATCH /vm`, shutdown via `SendCtrlAltDel`.
- Vsock guest agent (`ephemera exec <id> -- <command>`) — run a command inside the guest with no SSH and no network path at all; works over QEMU's native AF_VSOCK device and Cloud Hypervisor/Firecracker's UDS vsock proxy.
- `stop` prefers a graceful VMM shutdown, falling back to force-kill only if the process doesn't exit within a grace period.
- QEMU qcow2 backing overlays for cheap disposable writes.
- Raw reflink copies for Firecracker / Cloud Hypervisor when the host filesystem supports reflinks.
- Raw conversion fallback through `qemu-img`.
- Optional disk growth.
- cloud-init NoCloud seed disk generation.
- TAP interface creation and optional Linux bridge attachment.
- macvtap networking (QEMU and Cloud Hypervisor) — a VM's own MAC directly on a parent link, no bridge.
- QEMU user-mode networking + host port forwarding.
- VM state persisted to JSON.
- REST API.
- CLI.
- TTL reaper that destroys expired VMs.
- Console log path per VM.
- Control sockets: QMP, Cloud Hypervisor API socket, Firecracker API socket.
- Image download/cache + SHA-256 verification.
- `virt-customize` package/hostname/command/SSH-key customization, plus `guestkit`-based `copy_in`/`enable_services` for injecting files (e.g. the guest agent binary) and enabling systemd units without needing a network-capable libguestfs appliance.
- systemd units and one-command host bootstrap (installs QEMU tooling, Cloud Hypervisor, and Firecracker).
- SSH/rsync remote deploy script with full and quick profiles.
- End-to-end networking smoke test (QEMU user-mode NAT, TAP+bridge+DHCP, and macvtap, all SSH-verified).
- End-to-end lifecycle smoke test (vsock exec, pause/resume, graceful shutdown, and vsock-CID uniqueness under concurrent creates, all verified against real VMs).

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
./scripts/deploy-remote.sh 10.0.0.5 deploy --key   # full deploy, SSH key auth
./scripts/deploy-remote.sh deploy@10.0.0.5 --quick  # rsync + build only, skip dep install
./scripts/deploy-remote.sh 10.0.0.5 deploy --verify-only
./scripts/deploy-remote.sh --help
```

## Testing networking end-to-end

`scripts/test-networking.sh` boots real VMs over each supported network mode and proves they're
actually reachable over SSH — not just that the process launched:

- **QEMU user-mode NAT** + host port forward (no host network changes required).
- **TAP + Linux bridge + DHCP** (against an existing bridge with a DHCP server on it, e.g.
  libvirt's `virbr0` or a bridge set up by `bootstrap-host.sh`). Skipped with a warning if the
  bridge doesn't exist.
- **macvtap**, against a throwaway `dummy0` parent by default so the test never touches a real
  physical NIC/switch (pass `--macvtap-parent eth0` to test against a real uplink instead). Since
  macvtap's `bridge` mode can't reach the parent/host directly, the test creates a second, host-side
  macvtap sibling on the same parent to reach the guest's statically-assigned IP.

All three also assert cleanup: the QEMU process and (for TAP/macvtap) the interface must actually be
gone after `ephemera delete` — this is what caught a TAP-interface leak during development (fixed
by making VM shutdown wait for the process to actually exit before releasing its network resources).

```bash
sudo ./scripts/test-networking.sh                          # bridge defaults to vmbr0, macvtap uses dummy0
sudo ./scripts/test-networking.sh --bridge virbr0           # test TAP against libvirt's default network
sudo ./scripts/test-networking.sh --macvtap-parent eth0     # test macvtap against a real uplink
sudo ./scripts/test-networking.sh --image /path/to/base.qcow2   # skip auto-downloading a test image
```

It downloads an Ubuntu 24.04 cloud image on first run (cached under `<state_dir>/images/`) unless
`--image` is given, generates a throwaway SSH keypair, and prints a pass/fail/warn summary.

`scripts/test-lifecycle.sh` covers the rest of the VM lifecycle the same way: boots a QEMU VM with
the guest agent enabled and `network.mode=none`, proves `exec` round-trips real output over vsock
(no network path exists at all), forces a CPU-bound loop into the guest so pausing has something to
verify (an idle guest's VMM process shows ~flat CPU time whether it's paused or just idle — this
avoids that false signal), confirms the VMM's own CPU-time counter actually freezes while paused,
confirms `exec` works again after resume, confirms `stop` exits the VMM process, and confirms two
concurrently-created VMs get distinct vsock CIDs. QEMU only — Cloud Hypervisor and Firecracker were
validated manually (see "Pause, resume, and exec" below) since they need a Firecracker-compatible
uncompressed `vmlinux` / extracted whole-disk rootfs respectively, more setup than belongs in an
unattended script.

```bash
sudo ./scripts/test-lifecycle.sh
sudo ./scripts/test-lifecycle.sh --image /path/to/base.qcow2
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

## Auto backend selection

Set `"backend": "auto"` and the manager picks a concrete backend for you, resolved once at the very
start of `create` (the resolved value — never `"auto"` — is what's persisted and returned):

1. **Firecracker** if the request has a `kernel`, or `firecracker_kernel` is set in the config — the
   fastest microVM start when a direct-boot kernel is available.
2. otherwise **Cloud Hypervisor** if the request has a `kernel`/`firmware`, or
   `cloud_hypervisor_firmware` is set in the config.
3. otherwise **QEMU** — the only one of the three that boots from just a disk image, via its own
   BIOS/UEFI, with no kernel or firmware required.

```json
{ "name": "auto-example", "backend": "auto", "image": "/var/lib/ephemera/images/ubuntu.qcow2", "...": "..." }
```

Verified on real hardware (`scripts/test-auto-backend.sh`): all three resolution paths actually boot
the chosen backend and answer over vsock, not just that `resolve_backend` returns the right enum
value in isolation.

## Policy (admission limits)

`[policy]` in the config file (see `config.example.toml`) lets an operator cap what a `create`
request is allowed to ask for. Every field is optional and defaults to unrestricted — an absent or
empty `[policy]` table behaves exactly like no policy at all:

```toml
[policy]
max_vcpus = 8
max_memory_mib = 16384
max_disk_gib = 100
max_ttl_seconds = 86400          # every request must set ttl_seconds <= this; unbounded VMs are rejected
allowed_backends = ["qemu", "firecracker"]
allowed_image_dirs = ["/var/lib/ephemera/images"]
```

Checked once, right after `"auto"` resolves to a concrete backend and before any disk/network work
starts, so a rejected request fails fast with a specific reason (`request vcpus (4) exceeds policy
max_vcpus (2)`, `policy requires ttl_seconds to be set...`, `backend Firecracker is not permitted by
policy allowed_backends [Qemu]`, etc.) rather than a generic 400. `allowed_image_dirs` is a plain
path-prefix check — good enough to stop a tenant pointing `image` at an arbitrary host path, not a
symlink-resistant sandboxing boundary. Verified against a real config on real hardware: all five
cases (four rejections, one compliant create that actually boots) behave as documented.

## Pause, resume, and exec

```bash
sudo /usr/local/bin/ephemera --config /etc/ephemera.toml pause <id>
sudo /usr/local/bin/ephemera --config /etc/ephemera.toml resume <id>
sudo /usr/local/bin/ephemera --config /etc/ephemera.toml exec <id> -- echo hello
```

`exec` requires `agent.enabled: true` in the VM spec (see the JSON contract below) and the guest
image to have `ephemera-guest-agent` installed and running — build it with `cargo build --release
-p ephemera-guest-agent` and bake it into an image via `build-image`'s `copy_in`/`enable_services`
(see "Build an image" below, and `systemd/ephemera-guest-agent.service`).

**Guest-agent auth:** every agent-enabled VM gets a random shared-secret token (or the one you set in
`agent.token`) burned into that VM's own disk — never the shared base image — before it boots, at
`/etc/ephemera-guest-agent.token`. The agent checks it on every request; `eph exec`/the REST `/agent`
route supply it automatically from the VM's own record, so callers never handle it directly. This
stops a process on the host *other than ephemera* from opening a raw vsock socket to the VM's CID and
running commands as root — it does not replace REST-layer auth (see below), which answers a different
question ("can this caller reach ephemera's API at all"). A VM created before this existed, or with no
token file baked into its image for another reason, still runs the agent unauthenticated — check the
agent's own startup log line to be sure. Verified on real hardware
(`scripts/test-guest-agent-auth.sh`): a raw, tokenless (or wrong-token) vsock request is rejected,
the correct token succeeds, and `eph exec` keeps working unmodified.

`stop` always tries a graceful VMM-level shutdown first (QMP `system_powerdown` for QEMU, `ch-remote
shutdown` for Cloud Hypervisor, `SendCtrlAltDel` for Firecracker — x86_64 only, no ARM equivalent in
Firecracker's API today) and only force-kills the process if it doesn't exit within a grace period.

**Firecracker-specific note:** pause/resume were verified correct and fast against Firecracker's own
authoritative `GET /` state (not CPU-time heuristics — an idle guest and a paused one both show flat
CPU time, which is a false "it's paused" signal either way). `exec` over vsock works before a VM is
ever paused, but did not survive a pause/resume cycle in testing on this Firecracker version — a
Cloud Hypervisor VM's vsock connection *did* survive the identical pause/resume/exec sequence using
the same client code, so this looks like a Firecracker vsock characteristic rather than an ephemera
bug, but it's not something this project has a fix for.

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

`copy_in` places files directly into the image (via `guestkit`, before `virt-customize` runs) and
`enable_services` runs `systemctl enable` for each named unit — both independent of `virt-customize`'s
appliance, which needs outbound networking (`passt`) that isn't guaranteed to work on every host.
This is how the guest agent gets baked into an image:

```json
{
  "source": "/var/lib/ephemera/images/ubuntu.qcow2",
  "output": "/var/lib/ephemera/images/ubuntu-agent.qcow2",
  "format": "qcow2",
  "copy_in": [
    {"src": "/path/to/target/release/ephemera-guest-agent", "dest": "/usr/local/bin/ephemera-guest-agent"},
    {"src": "systemd/ephemera-guest-agent.service", "dest": "/etc/systemd/system/ephemera-guest-agent.service"}
  ],
  "enable_services": ["ephemera-guest-agent"]
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
GET    /metrics
POST   /v1/vms
GET    /v1/vms
GET    /v1/vms/{uuid}
POST   /v1/vms/{uuid}/stop
POST   /v1/vms/{uuid}/pause
POST   /v1/vms/{uuid}/resume
POST   /v1/vms/{uuid}/agent
DELETE /v1/vms/{uuid}
POST   /v1/images/build
```

`GET /metrics` returns Prometheus text-exposition-format gauges: `ephemera_vms_total{status="..."}`,
`ephemera_vms_by_backend{backend="..."}`, and `ephemera_vms_agent_enabled` — point a Prometheus
`scrape_config` at it directly, no exporter needed.

### Auth / RBAC

`[[auth.tokens]]` entries in the config (see `config.example.toml`) enable bearer-token auth on every
route except `GET /healthz`. Absent or empty `auth.tokens` (the default) leaves the API exactly as
open as the pre-auth MVP — every request is treated as `admin`. Two roles:

- `admin` — everything: create/stop/pause/resume/exec/delete/build-image.
- `read-only` — `GET /v1/vms`, `GET /v1/vms/{uuid}`, `GET /metrics` only; any mutating route returns 403.

```bash
curl -sS http://127.0.0.1:7788/v1/vms -H 'Authorization: Bearer <token>'
```

No token, or a token not in the config, gets 401. A `read-only` token on a mutating route gets 403.
Token comparison is constant-time. Verified on real hardware: 401 with no/wrong token, 200 for
`read-only` on `GET /v1/vms`, 403 for `read-only` on `POST /v1/vms`, 400 for `admin` on the same route
with an invalid body (proving auth let it through to the actual handler), 200 on `/healthz` with no
token at all even with auth enabled.

Create through REST:

```bash
curl -sS http://127.0.0.1:7788/v1/vms \
  -H 'content-type: application/json' \
  --data-binary @examples/qemu.json | jq
```

Exec through REST (`agent.enabled: true` required, see below):

```bash
curl -sS http://127.0.0.1:7788/v1/vms/<uuid>/agent \
  -H 'content-type: application/json' \
  -d '{"command": "echo hello", "timeout_seconds": 30}' | jq
```

## VM JSON contract

`backend` is one of `"qemu"`, `"cloud-hypervisor"`, `"firecracker"`, or `"auto"` (see "Auto backend
selection" above — the persisted/returned record always shows the resolved concrete backend, never
`"auto"`).

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
  "agent": {"enabled": true, "port": 17777},
  "ttl_seconds": 600,
  "extra_args": []
}
```

`agent.enabled` turns on the vsock guest agent (`ephemera exec`) for this VM — the guest image must
have `ephemera-guest-agent` installed and enabled (see "Build an image" above). `agent.port` is the
AF_VSOCK port the guest listens on (not a host TCP port); it defaults to `17777` and rarely needs
changing, since each VM already gets its own host-unique vsock CID.

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

macvtap (QEMU and Cloud Hypervisor only — see below):

```json
{
  "mode": "macvtap",
  "parent": "eth0",
  "macvtap_mode": "bridge",
  "mac": "52:54:00:aa:bb:cc"
}
```

Gives the VM its own MAC directly on `parent`'s link — no host bridge involved. `macvtap_mode` is
the macvtap link mode: `bridge` (default — siblings on the same parent can reach each other, but
not the parent itself directly), `vepa`, `private`, or `passthru`. The manager creates a per-VM
macvtap device on `parent`, opens its `/dev/tapN` character device, and passes that file descriptor
directly to the VMM (`-netdev tap,fd=N` for QEMU, `--net fd=N` for Cloud Hypervisor) — there's no
persistent named tap the VMM opens itself, which is why **Firecracker doesn't support this mode**:
its API only accepts a host device name it opens via `/dev/net/tun`, with no fd-passing option.

## State layout

```text
/var/lib/ephemera/
  vms.json
  vms.lock
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
      vsock.sock              (Cloud Hypervisor/Firecracker only, when agent.enabled)
      firecracker.json
```

`vms.lock` coordinates `vms.json` reads/writes across concurrent `ephemera` processes (each CLI
invocation is a separate process, not just a separate task inside `serve`) via an OS-level `flock` —
without it, two VMs created at the same moment could silently lose one's record, or both get
assigned the same vsock CID.

## Production changes I would make next

1. **Firecracker jailer backend** — chroot, uid/gid isolation, cgroups, resource limits.
2. **Network namespaces** — one namespace per VM, veth/TAP, nftables, DHCP/IPAM.
3. **Snapshots** — full VM state + disk snapshots per backend, and warm VM pools (pre-created paused VMs) for sub-second job start.
4. **Storage abstraction** — qcow2, raw reflink, LVM thin, Ceph RBD, NVMe local, NBD.
5. **Image catalog** — signed template manifests, distro/version/arch aliases, cosign/Sigstore or your own signing policy.
6. **Policy** — allowed networking modes are still unrestricted (max vCPU/RAM/disk/TTL and allowed backends/image directories are already implemented; see "Policy (admission limits)" above).
7. **Auth** — mTLS/OIDC, tenant IDs and audit events. Bearer-token REST auth/RBAC (admin/read-only) and a per-VM authenticated guest-agent protocol are already implemented; see "Auth / RBAC" and "Pause, resume, and exec" above.
8. **Observability** — tracing, per-VM boot timing and failure reasons (a basic Prometheus `/metrics` endpoint — VM counts by status/backend, agent-enabled count — is already implemented; see the REST API section).
9. **Kubernetes CRD/operator** — `DisposableVM` CRD backed by node-local daemonsets (`ephemera-kube`).
10. **Distributed node-agent** — `ephemera-agent` (the per-*host* one, not `ephemera-guest-agent`) running per hypervisor host, reporting to a central `ephemera-scheduler`.
11. **Scheduler placement** — NUMA awareness, CPU pinning, hugepages and GPU/VFIO assignment.
12. **Windows path** — QEMU/Cloud Hypervisor only; UEFI, virtio-win injection, sysprep and unattend support.

"auto" backend selection is already implemented — see "Auto backend selection" above.

## Important limitations in this MVP

- QEMU user networking is supported; Cloud Hypervisor and Firecracker require TAP, macvtap (Cloud Hypervisor only), or no networking.
- TAP and macvtap setup require host network privilege (CAP_NET_ADMIN).
- The bridge (for TAP) or parent link (for macvtap) must already exist and be configured for the network behavior you want.
- macvtap's `bridge` mode is asymmetric by design: sibling macvtap devices on the same parent can reach each other, but not the parent/host interface itself directly. Firecracker has no fd-passing option in its API, so macvtap isn't supported there.
- Firecracker image preparation is stricter than QEMU because it boots a kernel/rootfs directly.
- Guest disk partition/filesystem expansion after `qemu-img resize` is an image/guest concern. Use cloud-init growpart or your image pipeline.
- `extra_args` is intentionally an administrator escape hatch. Do not expose it to untrusted tenants.
- The API is localhost-only by default. Bearer-token auth/RBAC is opt-in (see "Auth / RBAC") — an operator who doesn't configure `[[auth.tokens]]` still gets the old open-by-default behavior.
- The vsock guest agent is authenticated by default for any VM created with `agent.enabled: true` (see "Pause, resume, and exec"), but this doesn't extend to mTLS/OIDC-style identity — it's one shared secret per VM, good enough to stop an unrelated host process, not a multi-tenant authorization model.
- `guestkit`'s `inspect_os()` (used by `copy_in`) only recognizes partitioned disks and LVM volumes as OS roots by default; support for a bare, unpartitioned whole-disk filesystem (the shape Firecracker rootfs images are typically built in) was added as part of this project's testing and needs to make it into a real guestkit release — until then, building against a `guestkit` checkout without that fix will fail `copy_in` on such images with "no operating system found in image".

## License

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). Copyright 2026 Zyvor.
