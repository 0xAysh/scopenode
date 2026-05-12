//! `doctor` command — report local node health.

use anyhow::{Context, Result};
use scopenode_storage::Db;
use std::path::Path;

/// Run the `doctor` command.
pub async fn run(db: Db, data_dir: &Path) -> Result<()> {
    let pending = db
        .count_pending_retry()
        .await
        .context("Failed to query retry queue")?;

    let event_count = db
        .count_all_events()
        .await
        .context("Failed to count events")?;

    let contracts = db
        .get_all_contracts()
        .await
        .context("Failed to query contracts")?;

    let db_size = db
        .db_size_bytes()
        .await
        .context("Failed to read DB size")?;

    let db_path = data_dir.join("scopenode.db");

    println!("Database");
    println!("  Path:           {}", db_path.display());
    println!("  Size:           {:.1} MB", db_size as f64 / 1_048_576.0);
    println!("  Contracts:      {}", contracts.len());
    println!("  Events indexed: {event_count}");
    println!("  Pending retry:  {pending}");

    if pending > 0 {
        println!("  → Re-run `scopenode sync` to reprocess failed blocks.");
    }

    Ok(())
}
