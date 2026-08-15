// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use clap::{Parser, Subcommand};
use ephemera_api as api;
use ephemera_core::{
    config::Config,
    model::{ClaimOverrides, CreateVmRequest},
};
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
    /// Relaunch a Stopped VM from its existing disk (skips image
    /// clone/cloud-init reseed — see VmManager::start).
    Start { id: Uuid },
    Stop { id: Uuid },
    Pause { id: Uuid },
    Resume { id: Uuid },
    /// Run a command inside the guest over vsock (requires agent.enabled in the VM spec).
    Exec {
        id: Uuid,
        #[arg(long)]
        timeout_seconds: Option<u64>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    Delete { id: Uuid },
    BuildImage { #[arg(long)] spec: PathBuf },
    /// Manage warm VM pools — pre-booted, paused VMs handed out on claim in
    /// roughly resume time instead of full create time.
    Pool {
        #[command(subcommand)]
        command: PoolCommand,
    },
}

#[derive(Subcommand)]
enum PoolCommand {
    Create { #[arg(long)] spec: PathBuf },
    List,
    Get { name: String },
    /// Claim one ready VM from the pool (backfill happens automatically).
    Claim {
        name: String,
        #[arg(long)]
        vm_name: Option<String>,
        #[arg(long)]
        ttl_seconds: Option<u64>,
    },
    Delete { name: String },
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
        Command::Start { id } => println!("{}", serde_json::to_string_pretty(&m.start(id).await?)?),
        Command::Stop { id } => println!("{}", serde_json::to_string_pretty(&m.stop(id).await?)?),
        Command::Pause { id } => println!("{}", serde_json::to_string_pretty(&m.pause(id).await?)?),
        Command::Resume { id } => println!("{}", serde_json::to_string_pretty(&m.resume(id).await?)?),
        Command::Exec { id, timeout_seconds, command } => {
            let response = m.exec(id, command.join(" "), timeout_seconds).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Command::Delete { id } => m.delete(id).await?,
        Command::BuildImage { spec } => {
            let req: BuildImageRequest = serde_json::from_slice(&std::fs::read(spec)?)?;
            println!("{}", serde_json::to_string_pretty(&image::build_image(&cfg, &req).await?)?);
        }
        Command::Pool { command } => match command {
            PoolCommand::Create { spec } => {
                let spec: ephemera_core::model::PoolSpec = serde_json::from_slice(&std::fs::read(spec)?)?;
                let name = spec.name.clone();
                m.create_pool(spec).await?;
                // This CLI process exits right after printing — wait for a
                // real backfill here rather than relying on the background
                // task create_pool() also fires off, which would otherwise
                // get killed mid-flight along with this process (see
                // VmManager::backfill_pool_sync's doc comment).
                m.backfill_pool_sync(&name).await?;
                println!("{}", serde_json::to_string_pretty(&m.get_pool(&name).await?)?);
            }
            PoolCommand::List => println!("{}", serde_json::to_string_pretty(&m.list_pools().await)?),
            PoolCommand::Get { name } => println!("{}", serde_json::to_string_pretty(&m.get_pool(&name).await?)?),
            PoolCommand::Claim { name, vm_name, ttl_seconds } => {
                let overrides = ClaimOverrides { name: vm_name, ttl_seconds };
                println!("{}", serde_json::to_string_pretty(&m.claim_from_pool(&name, overrides).await?)?);
            }
            PoolCommand::Delete { name } => m.delete_pool(&name).await?,
        },
    }
    Ok(())
}
