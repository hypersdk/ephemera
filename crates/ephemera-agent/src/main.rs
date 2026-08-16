// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "ephemera-agent", about = "Distributed node-agent + fleet registry for multi-host Zyvor Ephemera")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the central fleet registry + create/list/delete proxy.
    Central {
        #[arg(long, env = "LISTEN", default_value = "0.0.0.0:7799")]
        listen: String,
    },
    /// Run this node's heartbeat client, reporting to a central registry.
    Node {
        /// This node's name, as it'll appear in `POST /fleet/vms {"node": "..."}`.
        #[arg(long, env = "NODE_NAME")]
        name: String,
        /// Base URL of the `ephemera-agent central` instance.
        #[arg(long, env = "CENTRAL_URL")]
        central: String,
        /// Base URL THIS agent itself uses to reach its local `ephemera
        /// serve` — almost always a loopback address.
        #[arg(long, env = "EPHEMERA_URL", default_value = "http://127.0.0.1:7788")]
        ephemera_url: String,
        /// Base URL the CENTRAL registry should use to reach this same
        /// `ephemera serve` — must be this host's real, externally
        /// routable address when central runs elsewhere (see
        /// `node::NodeConfig::advertise_url`). Defaults to `ephemera_url`
        /// unchanged, which is only correct when central and this node
        /// happen to run on the same host.
        #[arg(long, env = "ADVERTISE_URL")]
        advertise_url: Option<String>,
        #[arg(long, default_value = "10")]
        interval_secs: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Central { listen } => {
            let app = ephemera_agent::central::router();
            let listener = tokio::net::TcpListener::bind(&listen).await
                .with_context(|| format!("binding {listen}"))?;
            tracing::info!(listen = %listen, "fleet registry listening");
            axum::serve(listener, app).await.context("serving")?;
        }
        Command::Node { name, central, ephemera_url, advertise_url, interval_secs } => {
            let advertise_url = advertise_url.unwrap_or_else(|| ephemera_url.clone());
            ephemera_agent::node::run(ephemera_agent::node::NodeConfig {
                name,
                central_url: central,
                ephemera_url,
                advertise_url,
                interval: Duration::from_secs(interval_secs.max(1)),
            })
            .await;
        }
    }
    Ok(())
}
