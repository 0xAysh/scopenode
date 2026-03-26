//! `restore` command — replace the live database with a snapshot.
//!
//! Copies a snapshot file from `<data_dir>/snapshots/` back to `scopenode.db`.
//! When no label is given, the most recent snapshot (by filename) is used.

use anyhow::{Context, Result};
use std::path::Path;

/// Run the `restore` command.
pub async fn run(data_dir: &Path, label: Option<String>) -> Result<()> {
    let snapshots_dir = data_dir.join("snapshots");

    let name = match label {
        Some(l) => l,
        None => {
            // Find the most recent snapshot by filename (lexicographic order on timestamps).
            let mut entries: Vec<String> = std::fs::read_dir(&snapshots_dir)
                .with_context(|| format!("Failed to read snapshots dir: {}", snapshots_dir.display()))?
                .filter_map(|e| {
                    let e = e.ok()?;
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_suffix(".db").map(|s| s.to_string())
                })
                .collect();
            entries.sort();
            entries
                .pop()
                .context("No snapshots found. Run `scopenode snapshot` first.")?
        }
    };

    let src = snapshots_dir.join(format!("{name}.db"));
    if !src.exists() {
        anyhow::bail!("Snapshot \"{}\" not found at {}.", name, src.display());
    }

    let dest = data_dir.join("scopenode.db");
    std::fs::copy(&src, &dest)
        .with_context(|| format!("Failed to restore snapshot \"{}\"", name))?;

    println!("Restored snapshot \"{}\" to {}.", name, dest.display());
    Ok(())
}
