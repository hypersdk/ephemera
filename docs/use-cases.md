# Use cases

Ephemera is a disposable-VM control plane: create a short-lived, isolated
virtual machine backed by QEMU/KVM, Cloud Hypervisor, or Firecracker, use it,
and let a TTL reaper clean it up. This doc walks through the use cases that
map directly onto what's actually implemented (see the main
[README](../README.md#what-is-implemented) for the full feature list) —
nothing here is aspirational.

## Ephemeral CI/CD build and test runners

Spin up a real VM per job, run the job inside it over vsock `exec` (no SSH,
no network path needed at all), and let `ttl_seconds` guarantee cleanup even
if the job crashes or the runner disappears mid-job.

```bash
cat > ci-job.json <<'JSON'
{
  "name": "ci-job-4821",
  "backend": "firecracker",
  "image": "/var/lib/ephemera/images/ci-runner.raw",
  "vcpus": 2,
  "memory_mib": 2048,
  "network": {"mode": "none"},
  "ttl_seconds": 900,
  "agent": {"enabled": true, "port": 5000}
}
JSON

id=$(ephemera create --spec ci-job.json | jq -r .id)
ephemera exec "$id" -- ./run-tests.sh
```

Firecracker's jailer (chroot + uid/gid drop, see "Firecracker jailer" in the
README) gives each job its own privilege-dropped sandbox, and cgroup v2
resource control caps what a single job can consume on a shared runner host.
`network.mode: "none"` plus vsock `exec` means a compromised or malicious
test suite has no network path out at all — the same isolation shape as
gVisor/Firecracker-based CI sandboxes, built on this project's own control
plane instead of a hosted service.

## Golden-image pipeline

Build a customized, versioned base image once — package installs, hostname,
SSH keys, a baked-in agent binary — and reuse it across every VM you create
from it, instead of provisioning each VM from scratch at boot time.

```bash
cat > golden-image.json <<'JSON'
{
  "source": "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img",
  "sha256": "...",
  "output": "/var/lib/ephemera/images/team-golden-v12.qcow2",
  "packages": ["docker.io", "jq", "qemu-guest-agent"],
  "commands": ["systemctl enable docker"],
  "enable_services": ["qemu-guest-agent"]
}
JSON

sudo ephemera build-image --spec golden-image.json
```

See [`docs/build-image-tutorials.md`](build-image-tutorials.md) for the same
walkthrough across Debian/Ubuntu, RHEL-family, and Arch base images. Pair it
with the image catalog (SHA-256 + optional Ed25519 signing, see "Image
catalog & signing" in the README) to give every VM a provenance guarantee —
`allowed_image_dirs` and `trusted_signers` mean a tenant can reference an
image by name and have the daemon refuse anything that isn't a known,
signed entry.

## Kubernetes-native disposable workloads

For teams already running Kubernetes who want a real VM (not a container)
for a specific workload — untrusted code, a kernel-dependent test, a legacy
binary — the `DisposableVm` CRD plus the node-local `ephemera-kube` operator
lets a VM be requested the same way any other Kubernetes resource is:

```yaml
apiVersion: ephemera.zyvor.io/v1
kind: DisposableVm
metadata:
  name: untrusted-job-7
spec:
  node: worker-3
  backend: firecracker
  image: /var/lib/ephemera/images/sandbox.raw
  vcpus: 1
  memoryMib: 1024
  networkMode: none
  ttlSeconds: 600
```

`kubectl delete disposablevm` blocks on a finalizer until the real VM is
actually gone (no leaked QEMU/Firecracker process), and the operator
self-heals — if the underlying VM disappears out-of-band, it gets replaced
automatically without touching the CR. This is verified against a real k3s
cluster, not just unit-tested against a fake API server (see "Kubernetes
CRD/operator" in the README).

## Multi-host fleets without Kubernetes

Not every team wants a Kubernetes control plane just to spread disposable
VMs across a handful of bare-metal or edge hosts. `ephemera-agent` is a
lighter-weight alternative: a central fleet registry plus a per-host
heartbeat client, with load-aware placement deciding which host a new VM
request lands on — verified across two real, physically separate hosts.
This fits edge deployments, colo racks, or any fleet where standing up a
full Kubernetes cluster is disproportionate to the actual workload.

## Sandboxed / untrusted code execution

The combination that makes Ephemera suitable for running code you don't
trust:

- **Firecracker jailer** — chroot + uid/gid drop, so even a Firecracker
  process compromise doesn't hand over root on the host.
- **cgroup v2 resource control** — hard caps on CPU/memory/IO per VM.
- **Network namespaces** and `network.mode: "none"` — no network path out of
  the guest at all when the workload doesn't need one.
- **vsock exec** — get output back from the guest without opening any
  network port, SSH included.
- **TTL reaper** — a VM that's forgotten about (crashed harness, orphaned
  job) gets torn down anyway.

This is the same isolation shape used for malware analysis sandboxes, "run
this untrusted PR's code" CI steps, and multi-tenant code-execution products
— built from primitives this project already has, not a separate product.

## Disposable dev/test environments

Give every branch, PR, or engineer their own real VM — not a shared
staging box — with automatic cleanup via `ttl_seconds` so nothing lingers
past its usefulness. QEMU's qcow2 copy-on-write overlays mean spinning up a
new VM from a golden image is cheap (no full disk copy), and `pause`/`resume`
let you park an environment instead of destroying and rebuilding it if
someone steps away mid-session.

## Bring-your-own storage backend

Beyond the default qcow2/raw overlay, Ephemera supports LVM thin snapshots,
NBD-exported disks, and Ceph RBD as storage backends for VM disks — Ceph RBD
verified against a real Rook Ceph cluster. This matters if you're deploying
into infrastructure that already standardized on one of these (a SAN
exposed over NBD, an existing Ceph cluster, LVM thin pools on local NVMe)
instead of adopting a new storage layer just for VM disks.

## Networking that matches the environment, not a fixed default

- **QEMU user-mode NAT** — zero host config, good for a single dev machine.
- **TAP + Linux bridge** — a VM on the same L2 as the host, for lab/test
  networks that need real bridged connectivity.
- **macvtap** — a VM's own MAC address directly on a parent link (no
  bridge), for environments where the VM needs to look like an independent
  host on the physical network.

All three are SSH-verified end-to-end in this project's own regression
tests, not just configured-and-hoped-for.
