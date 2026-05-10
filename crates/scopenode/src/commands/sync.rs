//! `sync` command — runs the ERA1 pipeline to index contract events from local ERA1 files.
//!
//! # Modes
//!
//! - **Plain mode** (default): indicatif progress bars during indexing.
//! - **Quiet mode** (`--quiet`): no progress output, plain text only.
//! - **Dry-run mode** (`--dry-run`): validates config and reports source path, no data read.
//!
//! # Steps
//!
//! 1. Validate that a `[source]` section is present in the config
//! 2. Scan the ERA1 source directory to build a file manifest
//! 3. For each contract in the config, run [`run_era1_scope`]
//! 4. Print a completion message and exit

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressDrawTarget};
use scopenode_core::{
    abi::AbiCache,
    config::{Config, SourceKind},
    era_pipeline::run_era1_scope,
    source::scan_era1_source,
};
use scopenode_storage::Db;
use std::path::PathBuf;

/// Run the `sync` command.
///
/// Reads ERA1 files from the configured source path and indexes events for
/// every contract listed in the config.  Dry-run mode exits early after
/// printing the resolved source path.
pub async fn run(
    config: Config,
    db: Db,
    dry_run: bool,
    quiet: bool,
    _blocks_override: Option<String>,
    _data_dir: PathBuf,
    _config_path: PathBuf,
) -> Result<()> {
    let source = config
        .source
        .as_ref()
        .context("sync requires a [source] config section — add [source] kind = \"era1\" path = \"...\" to your config")?;

    if dry_run {
        println!(
            "ERA1 source configured at {} — run `scopenode index config.toml` first to validate coverage",
            source.path.display()
        );
        return Ok(());
    }

    let scan = match source.kind {
        SourceKind::Era1 => scan_era1_source(&source.path, source.network.as_deref())
            .with_context(|| format!("Failed to scan ERA1 source: {}", source.path.display()))?,
    };

    let progress = MultiProgress::new();
    if quiet {
        progress.set_draw_target(ProgressDrawTarget::hidden());
    }

    let mut abi_cache = AbiCache::new(db.clone());

    for contract in &config.contracts {
        if let Err(e) =
            run_era1_scope(&scan.files, contract, &mut abi_cache, &db, &progress).await
        {
            eprintln!(
                "scope failed for {}: {e}",
                contract.name.as_deref().unwrap_or(&contract.address.to_string())
            );
        }
    }

    println!("sync complete — run `scopenode serve` to start JSON-RPC server");
    Ok(())
}

// ── --blocks flag parsing ─────────────────────────────────────────────────────

/// Parse a `--blocks` range string into `(from_block, to_block)`.
///
/// Formats:
/// - `"16M:17M"` → (16_000_000, Some(17_000_000))
/// - `"16M:+1000"` → (16_000_000, Some(16_001_000))
/// - `"16M:+0"` → (16_000_000, Some(16_000_000))
///
/// Shorthand suffixes `M` and `K` are supported on both sides.
/// Relative offset (`+N`) is resolved against the left bound.
#[allow(dead_code)]
pub fn parse_blocks_flag(s: &str) -> Result<(u64, Option<u64>), String> {
    use scopenode_core::config::parse_block_shorthand;

    let (left, right) = s.split_once(':').ok_or_else(|| {
        format!(
            "expected colon separator in \"{}\" — use e.g. \"16M:17M\" or \"16M:+1000\"",
            s
        )
    })?;

    let from = parse_block_shorthand(left.trim())?;

    let right = right.trim();
    let to = if let Some(offset_str) = right.strip_prefix('+') {
        let offset: u64 = offset_str.parse().map_err(|_| {
            format!(
                "invalid relative offset \"+{}\" — expected a non-negative integer",
                offset_str
            )
        })?;
        from.checked_add(offset)
            .ok_or_else(|| format!("block range overflow: {} + {}", from, offset))?
    } else {
        parse_block_shorthand(right)?
    };

    if to < from {
        return Err(format!(
            "to_block ({}) must be >= from_block ({})",
            to, from
        ));
    }

    Ok((from, Some(to)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_absolute_range() {
        let (from, to) = parse_blocks_flag("16M:17M").unwrap();
        assert_eq!(from, 16_000_000);
        assert_eq!(to, Some(17_000_000));
    }

    #[test]
    fn parse_relative_offset() {
        let (from, to) = parse_blocks_flag("16M:+500").unwrap();
        assert_eq!(from, 16_000_000);
        assert_eq!(to, Some(16_000_500));
    }

    #[test]
    fn parse_zero_offset() {
        let (from, to) = parse_blocks_flag("16M:+0").unwrap();
        assert_eq!(from, 16_000_000);
        assert_eq!(to, Some(16_000_000));
    }

    #[test]
    fn parse_integer_range() {
        let (from, to) = parse_blocks_flag("12376729:12500000").unwrap();
        assert_eq!(from, 12376729);
        assert_eq!(to, Some(12500000));
    }

    #[test]
    fn parse_inverted_errors() {
        assert!(parse_blocks_flag("17M:16M").is_err());
    }

    #[test]
    fn parse_missing_colon_errors() {
        assert!(parse_blocks_flag("16M").is_err());
    }
}
