//! `retry` command — re-fetch all blocks that failed receipt verification.
//!
//! Reads all bloom candidates with `pending_retry = 1` from the database,
//! re-fetches their receipts through the full pipeline (fetch → verify → decode → store),
//! and clears the retry flag on success. Blocks that fail again remain pending.

use anyhow::{Context, Result};
use scopenode_core::{
    config::Config,
    network::DevP2PNetwork,
    pipeline::Pipeline,
};
use scopenode_storage::Db;
use std::sync::Arc;

/// Run the `retry` command.
///
/// Boots a fresh devp2p connection, runs only the receipt-fetch stage for all
/// `pending_retry` blocks, and reports how many were cleared.
pub async fn run(config: Config, db: Db) -> Result<()> {
    let pending = db.count_pending_retry().await.context("Failed to query retry queue")?;
    if pending == 0 {
        println!("No blocks pending retry.");
        return Ok(());
    }

    println!("Retrying {pending} block(s) with failed receipt verification...");

    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_style(
        indicatif::ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
    );
    spinner.set_message("Connecting to devp2p peers...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let network = DevP2PNetwork::start()
        .await
        .context("Failed to start devp2p network")?;
    let network = Arc::new(network);
    spinner.finish_with_message("Connected ✓");

    let progress = indicatif::MultiProgress::new();
    let mut pipeline = Pipeline::new(config, network, db.clone());

    pipeline
        .run_retry(&progress)
        .await
        .context("Retry failed")?;

    let remaining = db.count_pending_retry().await.context("Failed to query retry queue")?;
    let cleared = pending - remaining;
    println!("\nRetry complete: {cleared} block(s) cleared, {remaining} still pending.");
    Ok(())
}
