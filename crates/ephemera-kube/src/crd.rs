// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A disposable VM, declared as a Kubernetes custom resource. The operator
/// running on each node (see `crate::controller`) watches these and drives
/// a *local* `ephemera serve` instance's REST API to create/delete the
/// underlying VM — it never talks to another node's ephemera, matching the
/// "node-local daemonset" model described in the project README rather
/// than a centralized scheduler that place VMs across a fleet.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[kube(
    group = "ephemera.zyvor.io",
    version = "v1",
    kind = "DisposableVm",
    plural = "disposablevms",
    shortname = "dvm",
    namespaced,
    status = "DisposableVmStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".spec.node"}"#,
    printcolumn = r#"{"name":"VmId","type":"string","jsonPath":".status.vmId"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct DisposableVmSpec {
    /// Node this VM must run on (matched against `NODE_NAME`/`--node-name`
    /// the operator on that node was started with — see `main.rs`). A
    /// `DisposableVm` with no `node` set, or one that doesn't match any
    /// running operator's node name, is never picked up by any instance;
    /// this project has no central scheduler to place it for you (see
    /// README's "Production changes" — distributed node-agent/central
    /// placement is still a deferred item).
    pub node: String,
    /// One of "qemu", "cloud-hypervisor", "firecracker", "auto" — passed
    /// straight through to the local ephemera's `CreateVmRequest.backend`.
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Passed straight through as `CreateVmRequest.image`. No catalog
    /// aliasing awareness beyond whatever the local ephemera is itself
    /// configured with (see README's "Image catalog & signing").
    pub image: String,
    #[serde(default = "default_vcpus")]
    pub vcpus: u8,
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u64,
    #[serde(default)]
    pub disk_size_gib: Option<u64>,
    /// "none" (default) or "user" — the two networking modes that need no
    /// host-specific device/bridge name in the CR itself. `tap`/`macvtap`
    /// need a `tap_name`/`bridge`/`parent` this CRD doesn't expose yet.
    #[serde(default = "default_network_mode")]
    pub network_mode: String,
    /// One of "default", "lvm-thin", "nbd", "ceph-rbd" — see README's
    /// "Storage backends". "default" (qcow2/raw) needs no other host setup;
    /// the other three assume the local ephemera's config/host already has
    /// the matching infrastructure (a thin pool, `qemu-nbd`, a Ceph
    /// cluster) — this CRD doesn't validate that ahead of time, the same
    /// way a raw `POST /v1/vms` doesn't either.
    #[serde(default = "default_storage")]
    pub storage: String,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

fn default_backend() -> String { "qemu".into() }
fn default_vcpus() -> u8 { 2 }
fn default_memory_mib() -> u64 { 2048 }
fn default_network_mode() -> String { "none".into() }
fn default_storage() -> String { "default".into() }

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DisposableVmStatus {
    /// "Pending" (finalizer not yet attached), "Running", or "Failed" —
    /// mirrors `ephemera_core::model::VmStatus` where they overlap, plus
    /// the CR-lifecycle-only "Pending". "Failed" is momentary in practice:
    /// this CRD is declarative (like a `Deployment`, not a one-shot `Job`)
    /// — if the underlying VM vanishes on its own (its `ttl_seconds`
    /// expired, or something deleted it directly via the REST API) the
    /// *next* reconcile clears `vm_id` and briefly reports "Failed", but
    /// the reconcile immediately after that creates a fresh VM and returns
    /// to "Running". Confirmed on real hardware: deleting a VM out from
    /// under a live CR via `DELETE /v1/vms/{id}` produced a genuinely new
    /// VM (a different id, a different pid) within two reconcile ticks,
    /// with no action needed on the CR itself. Only deleting the
    /// `DisposableVm` object stops this — see `controller::cleanup`.
    #[serde(default)]
    pub phase: String,
    /// The underlying VM's UUID once `POST /v1/vms` has succeeded — `None`
    /// before that, and briefly `None` again if the VM disappeared and is
    /// about to be recreated (see `phase`'s doc comment).
    #[serde(default)]
    pub vm_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}
