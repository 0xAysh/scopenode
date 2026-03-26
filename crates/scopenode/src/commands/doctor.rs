//! `doctor` command — check node health.
//!
//! Reports: DB size, pending retry queue length, event count, and contract sync state.

use anyhow::{Context, Result};
use scopenode_storage::Db;

/// Run the `doctor` command.
pub async fn run(db: Db) -> Result<()> {
    let pending = db
        .count_pending_retry()
        .await
        .context("Failed to query retry queue")?;

    let size_bytes = db
        .db_size_bytes()
        .await
        .context("Failed to read DB size")?;

    let contracts = db
        .get_all_contracts()
        .await
        .context("Failed to query contracts")?;

    println!("DB size:       {:.1} MB", size_bytes as f64 / 1_048_576.0);
    println!("Contracts:     {}", contracts.len());
    println!("Retry queue:   {pending} block(s) pending");

    if pending > 0 {
        println!("  → Run `scopenode retry <config>` to re-fetch failed blocks.");
    }

    Ok(())
}
