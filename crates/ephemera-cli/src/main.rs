// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use clap::{Parser, Subcommand};
use ephemera_api as api;
use ephemera_core::{config::Config, model::CreateVmRequest};
use ephemera_image::{self as image, BuildImageRequest};
use ephemera_scheduler::VmManager;
use std::{path::PathBuf, sync::Arc};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Parser)]
#[command(name="ephemera", version, about="Zyvor Ephemera: disposable compute engine for QEMU, Cloud Hypervisor and Firecracker")]
struct Cli {
    #[arg(long, env="EPHEMERA_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Create { #[arg(long)] spec: PathBuf },
    List,
    Get { id: Uuid },
    Stop { id: Uuid },
    Delete { id: Uuid },
    BuildImage { #[arg(long)] spec: PathBuf },
}

async fn manager(cfg: Config) -> Result<Arc<VmManager>> { VmManager::new(cfg) }

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        EnvFilter::try_from_default_env().unwrap_or_else(|_| "ephemera=info,tower_http=info".into())
    ).init();
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref())?;
    let m = manager(cfg.clone()).await?;

    match cli.command {
        Command::Serve => {
            m.start_reaper();
            let listener = TcpListener::bind(&cfg.listen).await?;
            tracing::info!(listen=%cfg.listen, "API listening");
            axum::serve(listener, api::router(m)).await?;
        }
        Command::Create { spec } => {
            let req: CreateVmRequest = serde_json::from_slice(&std::fs::read(spec)?)?;
            println!("{}", serde_json::to_string_pretty(&m.create(req).await?)?);
        }
        Command::List => println!("{}", serde_json::to_string_pretty(&m.list().await)?),
        Command::Get { id } => println!("{}", serde_json::to_string_pretty(&m.get(id).await?)?),
        Command::Stop { id } => println!("{}", serde_json::to_string_pretty(&m.stop(id).await?)?),
        Command::Delete { id } => m.delete(id).await?,
        Command::BuildImage { spec } => {
            let req: BuildImageRequest = serde_json::from_slice(&std::fs::read(spec)?)?;
            println!("{}", serde_json::to_string_pretty(&image::build_image(&cfg, &req).await?)?);
        }
    }
    Ok(())
}
