// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Distributed node-agent for multi-host Zyvor Ephemera deployments. Two
//! halves, both in this one binary (`ephemera-agent central` /
//! `ephemera-agent node`): `central` is a fleet registry + create/list/
//! delete proxy across every registered node; `node` is a per-host
//! heartbeat client reporting capacity + VM count to it. Distinct from
//! `ephemera-kube` (per-node Kubernetes reconciliation against a *local*
//! ephemera) — this is the non-Kubernetes multi-host story: a caller
//! talks to one central endpoint instead of knowing which host a VM is on.

pub mod central;
pub mod node;
