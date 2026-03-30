//! `sync` command — runs the full pipeline, then serves the JSON-RPC server.
//!
//! Steps:
//! 1. Boot [`DevP2PNetwork`] — connects to Ethereum mainnet peers via devp2p
//! 2. Apply any `--blocks` range override to all contracts in the config
//! 3. Build [`Pipeline`] and run it — headers → bloom → receipts → decode → store
//! 4. Start the JSON-RPC server on the configured port
//! 5. If any contract has no `to_block`, enter live sync mode; otherwise wait for Ctrl+C
//!
//! No RPC provider is used. All data comes from devp2p peers and is verified
//! cryptographically via Merkle Patricia Trie before being stored.

use anyhow::{Context, Result};
use indicatif::MultiProgress;
use scopenode_core::{
    config::Config,
    live::{self, LiveSyncer},
    network::DevP2PNetwork,
    pipeline::Pipeline,
};
use scopenode_rpc::start_server;
use scopenode_storage::Db;
use std::sync::Arc;
use tracing::info;

/// Run the `sync` command.
///
/// `blocks_override` is an optional `--blocks` flag value like `"16M:17M"` or `"16M:+1000"`.
/// When set, it overrides `from_block` / `to_block` for every contract in the config.
pub async fn run(
    mut config: Config,
    db: Db,
    dry_run: bool,
    quiet: bool,
    blocks_override: Option<String>,
) -> Result<()> {
    if let Some(ref range_str) = blocks_override {
        let (from, to) = parse_blocks_flag(range_str)
            .map_err(|e| anyhow::anyhow!("Invalid --blocks argument \"{range_str}\": {e}"))?;
        for contract in &mut config.contracts {
            contract.from_block = from;
            contract.to_block = to;
        }
        info!(?from, ?to, "--blocks override applied to all contracts");
    }

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
    let has_live = config.contracts.iter().any(|c| c.to_block.is_none());

    let mut pipeline = Pipeline::new(config.clone(), Arc::clone(&network), db.clone());

    println!("Starting sync via devp2p...");
    pipeline
        .run(dry_run, &progress)
        .await
        .context("Pipeline failed")?;

    if !dry_run {
        // Broadcast channel for live events — receivers can be added via subscribe().
        // Currently consumed by the live syncer; will fan out to SSE/webhooks in Phase 4.
        let (tx, _) = live::channel(1024);

        println!("\nSync complete. Starting JSON-RPC server on port {port}...");
        let handle = start_server(port, db.clone())
            .await
            .context("Failed to start JSON-RPC server")?;

        if has_live {
            println!("Entering live sync (Ctrl+C to stop)...");
            let syncer = LiveSyncer::new(config, network, db, tx);
            tokio::select! {
                res = syncer.run() => {
                    if let Err(e) = res {
                        eprintln!("Live sync error: {e:#}");
                    }
                }
                _ = tokio::signal::ctrl_c() => {}
            }
        } else {
            info!(port, "Historical sync done. Press Ctrl+C to stop.");
            tokio::signal::ctrl_c()
                .await
                .context("Failed to listen for Ctrl+C")?;
        }

        println!("\nShutting down...");
        handle.stop()?;
    }

    Ok(())
}

/// Parse a `--blocks` range string into `(from_block, to_block)`.
///
/// Formats:
/// - `"16M:17M"` → (16_000_000, Some(17_000_000))
/// - `"16M:+1000"` → (16_000_000, Some(16_001_000))
/// - `"16M:+0"` → (16_000_000, Some(16_000_000))
///
/// Shorthand suffixes `M` and `K` are supported on both sides.
/// Relative offset (`+N`) is resolved against the left bound.
pub fn parse_blocks_flag(s: &str) -> Result<(u64, Option<u64>), String> {
    use scopenode_core::config::parse_block_shorthand;

    let (left, right) = s.split_once(':')
        .ok_or_else(|| format!("expected colon separator in \"{}\" — use e.g. \"16M:17M\" or \"16M:+1000\"", s))?;

    let from = parse_block_shorthand(left.trim())?;

    let right = right.trim();
    let to = if let Some(offset_str) = right.strip_prefix('+') {
        let offset: u64 = offset_str.parse().map_err(|_| {
            format!("invalid relative offset \"+{}\" — expected a non-negative integer", offset_str)
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
