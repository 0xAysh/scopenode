//! `sync` command — runs the full pipeline and then serves the JSON-RPC server.
//!
//! This is the primary command. It:
//! 1. Fetches headers, runs bloom scans, fetches + verifies + decodes receipts
//! 2. Stores all indexed events in SQLite
//! 3. Starts the JSON-RPC server on the configured port
//! 4. Waits for Ctrl+C, then shuts down gracefully

use anyhow::{Context, Result};
use indicatif::MultiProgress;
use scopenode_core::{
    config::Config,
    network::RpcNetwork,
    pipeline::Pipeline,
};
use scopenode_rpc::start_server;
use scopenode_storage::Db;
use std::sync::Arc;
use tracing::info;

/// Run the `sync` command.
///
/// Steps:
/// 1. Create the [`RpcNetwork`] transport (Phase 1 implementation of [`EthNetwork`])
/// 2. Build the [`Pipeline`] and run it — fetches headers, bloom scans, receipts, decodes
/// 3. After sync: start the JSON-RPC server and wait for Ctrl+C
///
/// If `dry_run` is `true`, the pipeline prints a bloom estimate and exits
/// without fetching receipts or starting the server.
pub async fn run(
    config: Config,
    db: Db,
    dry_run: bool,
    quiet: bool,
) -> Result<()> {
    let network = RpcNetwork::from_env().context("Failed to create RPC network")?;
    let network = Arc::new(network);

    // Progress bars are hidden in --quiet mode (e.g. when piping output or running in CI).
    let progress = MultiProgress::new();
    if quiet {
        progress.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }

    let port = config.node.port;
    let mut pipeline = Pipeline::new(config, network, db.clone());

    println!("Starting sync...");
    pipeline
        .run(dry_run, &progress)
        .await
        .context("Pipeline failed")?;

    if !dry_run {
        println!("\nSync complete. Starting JSON-RPC server on port {port}...");
        let handle = start_server(port, db)
            .await
            .context("Failed to start JSON-RPC server")?;
        info!(port, "Server running. Press Ctrl+C to stop.");

        // Block here forever — the server runs on background tokio tasks.
        // ctrl_c() resolves only when SIGINT is received.
        tokio::signal::ctrl_c()
            .await
            .context("Failed to listen for Ctrl+C")?;
        println!("\nShutting down...");
        handle.stop()?;
    }

    Ok(())
}
