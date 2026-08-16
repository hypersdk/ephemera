// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Thin REST client against a *local* `ephemera serve` — see
//! `crate::crd::DisposableVmSpec` for why this is local-only, not a
//! fan-out to arbitrary nodes.

use crate::crd::DisposableVmSpec;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct EphemeraClient {
    base_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl EphemeraClient {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self { base_url, token, http: reqwest::Client::new() }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self.http.request(method, format!("{}{}", self.base_url, path));
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// `POST /v1/vms` with a `CreateVmRequest`-shaped body built from `spec`.
    /// Returns the full VM JSON record (has `.id`) on success.
    pub async fn create_vm(&self, spec: &DisposableVmSpec) -> Result<Value> {
        let network = match spec.network_mode.as_str() {
            "none" => json!({"mode": "none"}),
            "user" => json!({"mode": "user", "forwards": []}),
            other => bail!(
                "network_mode '{other}' is not supported by this CRD yet (only \"none\" and \"user\" — tap/macvtap need a device/bridge name this CRD doesn't expose)"
            ),
        };
        let body = json!({
            "name": format!("dvm-{}", uuid_like_suffix()),
            "backend": spec.backend,
            "image": spec.image,
            "vcpus": spec.vcpus,
            "memory_mib": spec.memory_mib,
            "disk_size_gib": spec.disk_size_gib,
            "network": network,
            "storage": spec.storage,
            "ttl_seconds": spec.ttl_seconds,
        });
        let resp = self.request(reqwest::Method::POST, "/v1/vms")
            .json(&body)
            .send()
            .await
            .context("sending POST /v1/vms")?;
        response_json_or_err(resp).await
    }

    /// `GET /v1/vms/{id}`. `Ok(None)` for a "not found" response so the
    /// caller can treat that as "already gone" (e.g. expired via its own
    /// TTL) rather than a transient error.
    ///
    /// Real API characteristic found while testing this against a live
    /// `ephemera serve`: every error this API returns — "not found"
    /// included — comes back as a generic `400 Bad Request` with an
    /// `{"error": "..."}` body (`ApiError`'s blanket `From<E>` impl maps
    /// *all* errors to 400, there is no distinct 404 anywhere in this API).
    /// So status-code sniffing for 404 (what a REST client would normally
    /// do, and what this originally did) never actually matches — this has
    /// to check the message text instead, matching the real contract.
    pub async fn get_vm(&self, id: &str) -> Result<Option<Value>> {
        let resp = self.request(reqwest::Method::GET, &format!("/v1/vms/{id}"))
            .send()
            .await
            .context("sending GET /v1/vms/{id}")?;
        if resp.status().is_success() {
            return Ok(Some(response_json_or_err(resp).await?));
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if is_not_found(&body) {
            return Ok(None);
        }
        bail!("GET /v1/vms/{id} failed: {status}: {body}")
    }

    /// `DELETE /v1/vms/{id}`. "Not found" (see `get_vm`'s doc comment for
    /// why that's message-text-based, not a status code) is treated as
    /// success — deleting an already-gone VM is exactly the outcome the
    /// caller wanted.
    pub async fn delete_vm(&self, id: &str) -> Result<()> {
        let resp = self.request(reqwest::Method::DELETE, &format!("/v1/vms/{id}"))
            .send()
            .await
            .context("sending DELETE /v1/vms/{id}")?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if is_not_found(&body) {
            return Ok(());
        }
        bail!("DELETE /v1/vms/{id} failed: {status}: {body}")
    }
}

/// The exact string ephemera-scheduler's `VmManager::get` produces (via
/// `.context("VM not found")`) for both `GET` and `DELETE` on an unknown
/// id — checked as a substring since the body is JSON-wrapped
/// (`{"error":"VM not found"}`), not matched exactly.
fn is_not_found(body: &str) -> bool {
    body.contains("VM not found")
}

async fn response_json_or_err(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body = resp.text().await.context("reading response body")?;
    if !status.is_success() {
        bail!("ephemera API returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parsing ephemera API response as JSON: {body}"))
}

/// A short, filesystem/name-safe suffix for the VM's display name — the
/// CR's own name is already the stable identity kubectl/etcd care about,
/// this only needs to be readable in `ephemera list` output.
fn uuid_like_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{:x}", nanos & 0xffff_ffff)
}
