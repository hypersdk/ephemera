// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use ephemera_kube::{controller, crd::DisposableVm, ephemera_client::EphemeraClient};
use kube::CustomResourceExt;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // `ephemera-kube --print-crd` emits the CRD manifest (JSON — a valid
    // YAML subset `kubectl apply -f -` accepts directly) instead of
    // running the operator, so the CRD can be regenerated from the actual
    // Rust type whenever it changes rather than hand-maintained.
    if std::env::args().any(|a| a == "--print-crd") {
        let crd = DisposableVm::crd();
        println!("{}", serde_json::to_string_pretty(&crd)?);
        return Ok(());
    }

    let node_name = std::env::var("NODE_NAME")
        .context("NODE_NAME env var is required — the node name this operator instance's spec.node filter matches against")?;
    let base_url = std::env::var("EPHEMERA_URL").unwrap_or_else(|_| "http://127.0.0.1:7788".into());
    let token = std::env::var("EPHEMERA_TOKEN").ok();

    let client = kube::Client::try_default()
        .await
        .context("connecting to Kubernetes (reads KUBECONFIG, or in-cluster config when run as a pod)")?;
    let ephemera = EphemeraClient::new(base_url, token);

    controller::run(client, ephemera, node_name).await;
    Ok(())
}
