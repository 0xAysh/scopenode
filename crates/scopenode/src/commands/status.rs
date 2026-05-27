//! `status` command — summarize what the local scopenode database contains.

use anyhow::{Context, Result};
use scopenode_core::config::Config;
use scopenode_storage::Db;
use std::path::{Path, PathBuf};

/// Entry point called from main.rs — loads config, opens DB, prints status.
pub async fn execute(config_path: PathBuf) -> Result<()> {
    let config = Config::from_file(&config_path).context("Failed to load config")?;

    let data_dir = expand_tilde(
        config
            .node
            .data_dir
            .clone()
            .unwrap_or_else(default_data_dir),
    );
    let db_path = data_dir.join("scopenode.db");
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("Failed to create data dir: {}", data_dir.display()))?;

    let db = Db::open(db_path.clone())
        .await
        .context("Failed to open database")?;

    print_status(&config, &db, &db_path).await
}

async fn print_status(config: &Config, db: &Db, db_path: &Path) -> Result<()> {
    let summary = db.status_summary().await?;

    println!("scopenode status");
    println!();
    println!("Database:");
    println!("  path: {}", db_path.display());
    println!("  size: {}", human_bytes(summary.db_size_bytes));
    println!();
    println!("Indexed data:");
    println!("  contracts: {}", summary.contract_count);
    println!("  events: {}", summary.event_count);
    match summary.block_range {
        Some((from, to)) => println!("  block range: {from} -> {to}"),
        None => println!("  block range: none"),
    }
    println!();
    println!("Configured serving:");
    println!("  JSON-RPC: http://127.0.0.1:{}", config.node.port);
    println!("  REST:     http://127.0.0.1:{}", config.node.rest_port);
    println!();

    println!("Contracts:");
    if summary.contracts.is_empty() {
        println!("  none");
        return Ok(());
    }

    for contract in &summary.contracts {
        let label = contract.name.as_deref().unwrap_or("Unnamed contract");
        println!("  {label}");
        println!("    address: {}", contract.address);
        println!("    events: {}", contract.total_events);

        if !contract.event_breakdown.is_empty() {
            println!("    event breakdown:");
            for (name, count) in &contract.event_breakdown {
                println!("      {name}: {count}");
            }
        }
    }

    Ok(())
}

fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".scopenode")
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(stripped) = s.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(stripped);
            }
        }
    }
    path
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(42), "42 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }
}
