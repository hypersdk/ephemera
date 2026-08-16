// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use crate::crd::{DisposableVm, DisposableVmStatus};
use crate::ephemera_client::EphemeraClient;
use futures::StreamExt;
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        finalizer::{finalizer, Event as FinalizerEvent},
        watcher,
    },
    Client, ResourceExt,
};
use std::{sync::Arc, time::Duration};

const FINALIZER: &str = "ephemera.zyvor.io/cleanup";
/// Baseline poll interval for an already-`Running` VM — cheap (one
/// `GET /v1/vms/{id}` against the local REST API), just enough to notice
/// if the VM vanished out from under the CR on its own (its `ttl_seconds`
/// expired, or it crashed and the reaper cleaned it up).
const STEADY_STATE_REQUEUE: Duration = Duration::from_secs(30);
const ERROR_REQUEUE: Duration = Duration::from_secs(15);

pub struct Context {
    pub client: Client,
    pub ephemera: EphemeraClient,
    pub node_name: String,
}

/// `kube_runtime::finalizer` requires its reconcile closure's error type to
/// implement `std::error::Error` — `anyhow::Error` deliberately doesn't, so
/// `apply`/`cleanup` return this instead, converting from `anyhow::Error` at
/// their few fallible call sites via the `From` impl below.
#[derive(thiserror::Error, Debug)]
pub enum ReconcileError {
    #[error("{0}")]
    Ephemera(String),
    #[error("kube API error: {0}")]
    Kube(#[from] kube::Error),
}

impl From<anyhow::Error> for ReconcileError {
    fn from(e: anyhow::Error) -> Self {
        Self::Ephemera(format!("{e:#}"))
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("finalizer error: {0}")]
    Finalizer(#[from] kube::runtime::finalizer::Error<ReconcileError>),
}

/// Runs forever (until the process is killed), watching `DisposableVm`
/// objects across every namespace and reconciling the ones targeting this
/// node. Meant to run as one instance per node (a daemonset, in the real
/// deployment this project doesn't yet package as a container — see
/// README's "Kubernetes CRD/operator" section).
pub async fn run(client: Client, ephemera: EphemeraClient, node_name: String) {
    let api: Api<DisposableVm> = Api::all(client.clone());
    let ctx = Arc::new(Context { client, ephemera, node_name });
    tracing::info!(node = %ctx.node_name, "starting DisposableVm controller");
    Controller::new(api, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(action) => tracing::debug!(?action, "reconciled"),
                Err(e) => tracing::warn!(error = %e, "reconcile error"),
            }
        })
        .await;
}

async fn reconcile(obj: Arc<DisposableVm>, ctx: Arc<Context>) -> Result<Action, Error> {
    // Not targeted at this node — some other node's operator instance owns
    // it (or no instance does yet, if `spec.node` names a node nothing is
    // running on). Every instance still gets a watch event for every CR in
    // the cluster (Api::all has no server-side field selector on a custom
    // field like spec.node), so this per-node filter has to happen here.
    if obj.spec.node != ctx.node_name {
        return Ok(Action::await_change());
    }
    let ns = obj.namespace().unwrap_or_else(|| "default".into());
    let api: Api<DisposableVm> = Api::namespaced(ctx.client.clone(), &ns);
    finalizer(&api, FINALIZER, obj, |event| async {
        match event {
            FinalizerEvent::Apply(obj) => apply(&obj, &api, &ctx).await,
            FinalizerEvent::Cleanup(obj) => cleanup(&obj, &ctx).await,
        }
    })
    .await
    .map_err(Error::Finalizer)
}

fn error_policy(_obj: Arc<DisposableVm>, err: &Error, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "reconcile failed, retrying");
    Action::requeue(ERROR_REQUEUE)
}

async fn apply(obj: &DisposableVm, api: &Api<DisposableVm>, ctx: &Context) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let already_has_vm = obj.status.as_ref().and_then(|s| s.vm_id.as_deref()).map(str::to_string);

    let Some(vm_id) = already_has_vm else {
        // First time seeing this CR (or a previous create attempt failed
        // without ever recording a vm_id) — create it.
        return match ctx.ephemera.create_vm(&obj.spec).await {
            Ok(record) => {
                let vm_id = record.get("id").and_then(|v| v.as_str()).map(str::to_string);
                patch_status(api, &name, DisposableVmStatus { phase: "Running".into(), vm_id, error: None }).await?;
                Ok(Action::requeue(STEADY_STATE_REQUEUE))
            }
            Err(e) => {
                patch_status(api, &name, DisposableVmStatus {
                    phase: "Failed".into(),
                    vm_id: None,
                    error: Some(format!("{e:#}")),
                })
                .await?;
                // Real failures (bad image path, policy rejection, ...)
                // won't fix themselves by retrying immediately — still
                // retry eventually in case it's transient (VMM binary
                // temporarily missing during a host upgrade, etc.).
                Ok(Action::requeue(ERROR_REQUEUE))
            }
        };
    };

    // Already created — just confirm it's still there. A VM can vanish on
    // its own via `ttl_seconds` (see README's TTL reaper) with nothing
    // touching the CR to tell it so.
    match ctx.ephemera.get_vm(&vm_id).await {
        Ok(Some(_)) => Ok(Action::requeue(STEADY_STATE_REQUEUE)),
        Ok(None) => {
            // Clearing vm_id here (rather than leaving it and just setting
            // phase=Failed) is what makes this self-healing: the status
            // patch below triggers an immediate re-reconcile, which then
            // takes the `already_has_vm == None` branch above and creates
            // a fresh VM — confirmed on real hardware, a VM deleted via
            // `DELETE /v1/vms/{id}` out from under a live CR was replaced
            // by a genuinely new one (different id, different pid) within
            // two reconcile ticks. This CRD is declarative like a
            // `Deployment`, not one-shot like a `Job` — see
            // `DisposableVmStatus::phase`'s doc comment.
            patch_status(api, &name, DisposableVmStatus {
                phase: "Failed".into(),
                vm_id: None,
                error: Some("underlying VM no longer exists (expired via its own ttl_seconds, or was deleted out of band) — recreating".into()),
            })
            .await?;
            Ok(Action::requeue(ERROR_REQUEUE))
        }
        Err(e) => {
            tracing::warn!(vm = %vm_id, error = %e, "failed to refresh VM status, will retry");
            Ok(Action::requeue(ERROR_REQUEUE))
        }
    }
}

/// Runs when the CR itself is deleted (`kubectl delete disposablevm ...`).
/// Deletes the real VM first — only once that succeeds does `finalizer()`
/// remove the finalizer and let Kubernetes actually reclaim the object, so
/// a `DisposableVm` can never be deleted out from under a still-running VM.
async fn cleanup(obj: &DisposableVm, ctx: &Context) -> Result<Action, ReconcileError> {
    if let Some(id) = obj.status.as_ref().and_then(|s| s.vm_id.as_deref()) {
        ctx.ephemera.delete_vm(id).await?;
    }
    Ok(Action::await_change())
}

async fn patch_status(api: &Api<DisposableVm>, name: &str, status: DisposableVmStatus) -> Result<(), kube::Error> {
    let patch = serde_json::json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(patch)).await?;
    Ok(())
}
