//! `status` command — shows indexed contracts and sync state.

use std::path::Path;

use anyhow::Result;
use scopenode_storage::Db;

/// Print indexed contracts and DB stats.
pub async fn run(db: Db, _data_dir: &Path) -> Result<()> {
    print_db_status(&db).await?;
    Ok(())
}

async fn print_db_status(db: &Db) -> Result<()> {
    let contracts = db.get_all_contracts().await?;
    let pending_retry = db.count_pending_retry().await?;
    let db_size = db.db_size_bytes().await?;

    if contracts.is_empty() {
        println!("\nNo contracts indexed yet. Run `scopenode sync <config>` to start.");
        return Ok(());
    }

    println!("\nscopenode — indexed contracts\n");
    println!("{:<44}  {:<30}  Events", "Address", "Name");
    println!("{}", "─".repeat(80));

    for c in &contracts {
        let name = c.name.as_deref().unwrap_or("(unnamed)");
        let total: i64 = db.count_events_for_contract(&c.address).await?;
        println!("{:<44}  {:<30}  {}", c.address, name, total);
    }

    println!();
    println!("Database size:     {} KB", db_size / 1024);
    println!("Pending retry:     {pending_retry} blocks");

    Ok(())
}
