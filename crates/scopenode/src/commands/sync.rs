//! `sync` command — runs the full pipeline, then serves the JSON-RPC server.
//!
//! Steps:
//! 1. Boot [`DevP2PNetwork`] — connects to Ethereum mainnet peers via devp2p
//! 2. Build [`Pipeline`] and run it — headers → bloom → receipts → decode → store
//! 3. Start the JSON-RPC server on the configured port
//! 4. Wait for Ctrl+C, then shut down gracefully
//!
//! No RPC provider is used. All data comes from devp2p peers and is verified
//! cryptographically via Merkle Patricia Trie before being stored.

use anyhow::{Context, Result};
use indicatif::MultiProgress;
use scopenode_core::{
    config::Config,
    network::DevP2PNetwork,
    pipeline::Pipeline,
};
use scopenode_rpc::start_server;
use scopenode_storage::Db;
use std::sync::Arc;
use tracing::info;

/// Run the `sync` command.
pub async fn run(
    config: Config,
    db: Db,
    dry_run: bool,
    quiet: bool,
) -> Result<()> {
    let progress = MultiProgress::new();
    if quiet {
        progress.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }

    // Show a spinner while waiting for devp2p peers — cold start can take 1–3 min.
    let spinner = progress.add(indicatif::ProgressBar::new_spinner());
    spinner.set_style(
        indicatif::ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
    );
    spinner.set_message("Connecting to Ethereum mainnet peers via devp2p...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let network = DevP2PNetwork::start()
        .await
        .context("Failed to start devp2p network")?;
    let network = Arc::new(network);

    spinner.finish_with_message("Connected to Ethereum mainnet peers ✓");

    let port = config.node.port;
    let mut pipeline = Pipeline::new(config, network, db.clone());

    println!("Starting sync via devp2p...");
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

        tokio::signal::ctrl_c()
            .await
            .context("Failed to listen for Ctrl+C")?;
        println!("\nShutting down...");
        handle.stop()?;
    }

    Ok(())
}
