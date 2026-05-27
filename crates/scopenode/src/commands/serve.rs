//! `serve` command — start the JSON-RPC (:8545) and REST (:8546) servers.

use anyhow::{Context, Result};
use scopenode_core::config::Config;
use scopenode_storage::Db;
use std::path::PathBuf;

use crate::runtime::RuntimeContext;

pub async fn run(config: Config, db: Db) -> Result<()> {
    let port = config.node.port;
    let rest_port = config.node.rest_port;

    let rpc_handle = scopenode_rpc::start_server(db.clone(), port)
        .await
        .with_context(|| format!("Failed to start JSON-RPC server on port {port}"))?;

    scopenode_rpc::start_rest_server(rest_port, db)
        .await
        .with_context(|| format!("Failed to start REST server on port {rest_port}"))?;

    println!("scopenode serving:");
    println!("  JSON-RPC  http://127.0.0.1:{port}");
    println!("  REST      http://127.0.0.1:{rest_port}/events");
    println!("Press Ctrl+C to stop.");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down...");
        }
        _ = rpc_handle.stopped() => {}
    }

    Ok(())
}

/// Entry point called from main.rs — loads config, opens DB, calls run().
pub async fn execute(config_path: PathBuf) -> Result<()> {
    let ctx = RuntimeContext::load(config_path).await?;
    run(ctx.config, ctx.db).await
}
