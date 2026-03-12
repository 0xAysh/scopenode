//! `status` command — shows what contracts are indexed and basic DB stats.
//!
//! Reads directly from SQLite without requiring a config file. Useful for
//! checking the state of a running or paused scopenode instance.

use anyhow::Result;
use scopenode_storage::Db;

/// Print a summary table of all indexed contracts.
///
/// For each contract, shows:
/// - EIP-55 checksummed address
/// - Human-readable name (from config, if set)
/// - Total non-reorged event count
///
/// Also shows overall stats: database size on disk and pending retry block count.
/// A non-zero pending retry count means some blocks failed Merkle verification
/// and should be retried with `scopenode retry` (Phase 3a).
pub async fn run(db: Db) -> Result<()> {
    let contracts = db.get_all_contracts().await?;
    let pending_retry = db.count_pending_retry().await?;
    let db_size = db.db_size_bytes().await?;

    if contracts.is_empty() {
        println!("No contracts indexed yet. Run `scopenode sync <config>` to start.");
        return Ok(());
    }

    println!("scopenode — indexed contracts\n");
    println!("{:<44}  {:<30}  Events", "Address", "Name");
    println!("{}", "─".repeat(80));

    for c in &contracts {
        let name = c.name.as_deref().unwrap_or("(unnamed)");
        // Count all non-reorged events for this contract across all event types.
        let total: i64 = db.count_events_for_contract(&c.address).await?;
        println!("{:<44}  {:<30}  {}", c.address, name, total);
    }

    println!();
    println!("Database size:     {} KB", db_size / 1024);
    println!("Pending retry:     {pending_retry} blocks");

    Ok(())
}
