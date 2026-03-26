//! `snapshot` command — copy the database to the snapshots directory.
//!
//! Creates a point-in-time copy of `scopenode.db` under `<data_dir>/snapshots/`.
//! The snapshot is safe to copy even while sync is running because SQLite WAL mode
//! guarantees a consistent read of the database.

use anyhow::{Context, Result};
use std::path::Path;

/// Run the `snapshot` command.
///
/// `label` is an optional human-readable name for the snapshot. When absent,
/// the current Unix timestamp in seconds is used.
pub async fn run(data_dir: &Path, label: Option<String>) -> Result<()> {
    let db_path = data_dir.join("scopenode.db");
    if !db_path.exists() {
        anyhow::bail!(
            "No database found at {}. Run `scopenode sync` first.",
            db_path.display()
        );
    }

    let snapshots_dir = data_dir.join("snapshots");
    std::fs::create_dir_all(&snapshots_dir)
        .with_context(|| format!("Failed to create snapshots dir: {}", snapshots_dir.display()))?;

    let name = if let Some(l) = label {
        if !l.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
            anyhow::bail!("--label must contain only alphanumeric characters, dashes, underscores, and dots");
        }
        l
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string()
    };

    let dest = snapshots_dir.join(format!("{name}.db"));
    if dest.exists() {
        anyhow::bail!(
            "Snapshot \"{}\" already exists at {}. Choose a different label.",
            name,
            dest.display()
        );
    }

    // Use SQLite's VACUUM INTO for a clean, compacted, WAL-checkpointed snapshot.
    // This avoids copying partial WAL frames and produces a self-contained file.
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .context("Failed to open database for snapshot")?;

    sqlx::query(&format!("VACUUM INTO '{}'", dest.to_string_lossy()))
        .execute(&pool)
        .await
        .with_context(|| format!("VACUUM INTO failed for snapshot \"{}\"", name))?;

    pool.close().await;

    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    println!(
        "Snapshot saved: {} ({:.1} MB)",
        dest.display(),
        size as f64 / 1_048_576.0
    );
    Ok(())
}
