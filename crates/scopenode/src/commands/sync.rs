//! `sync` command — runs the full pipeline, then serves the JSON-RPC server.
//!
//! # Modes
//!
//! - **Plain mode** (default): indicatif progress bars during historical sync,
//!   one printed line per live event, Ctrl+C to stop.
//! - **Quiet mode** (`--quiet`): no progress output, plain text only.
//! - **Dry-run mode** (`--dry-run`): bloom estimate only, no receipt fetch.
//!
//! # Steps
//!
//! 1. Apply any `--blocks` range override to all contracts in the config
//! 2. Boot [`DevP2PNetwork`] — connects to Ethereum mainnet peers via devp2p
//! 3. Run [`Pipeline`] — headers → bloom → receipts → decode → store
//! 4. Start JSON-RPC server on the configured port
//! 5. If any contract has no `to_block`, enter live sync mode

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use alloy_primitives::B256;
use scopenode_core::{
    beacon::BeaconStatus,
    config::Config,
    live::LiveSyncer,
    network::{DevP2PNetwork, EthNetwork},
    pipeline::Pipeline,
    types::StoredEvent,
};
use scopenode_rpc::{start_rest_server, start_server};
use scopenode_storage::Db;
use tokio::sync::{broadcast, watch};
use tracing::info;

/// Run the `sync` command.
pub async fn run(
    mut config: Config,
    db: Db,
    dry_run: bool,
    quiet: bool,
    blocks_override: Option<String>,
    data_dir: PathBuf,
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

    // Spinner during peer discovery (shown in all modes).
    let spinner_progress = MultiProgress::new();
    if quiet {
        spinner_progress.set_draw_target(ProgressDrawTarget::hidden());
    }
    let spinner = spinner_progress.add(ProgressBar::new_spinner());
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spinner.set_message("Connecting to Ethereum mainnet peers via devp2p...");
    spinner.enable_steady_tick(Duration::from_millis(100));

    let network = Arc::new(
        DevP2PNetwork::start()
            .await
            .context("Failed to start devp2p network")?,
    );

    let port = config.node.port;
    let has_live = config.contracts.iter().any(|c| c.to_block.is_none());

    if dry_run || quiet {
        spinner.finish_with_message("Connected to Ethereum mainnet peers ✓");

        let progress = MultiProgress::new();
        if quiet {
            progress.set_draw_target(ProgressDrawTarget::hidden());
        }

        let mut pipeline = Pipeline::new(config.clone(), Arc::clone(&network), db.clone());
        pipeline
            .run(dry_run, &progress)
            .await
            .context("Pipeline failed")?;

        if !dry_run {
            let (tx, _) = broadcast::channel::<StoredEvent>(1024);
            let (headers_tx, _) = broadcast::channel::<(u64, B256, u64)>(1024);
            let peer_count_atom = Arc::new(AtomicUsize::new(network.peer_count().await));
            println!("\nSync complete. Starting servers on ports {port} (JSON-RPC) and {} (REST)...", port + 1);
            let handle = start_server(port, db.clone(), tx.clone(), headers_tx.clone(), Arc::clone(&peer_count_atom))
                .await
                .context("Failed to start JSON-RPC server")?;
            start_rest_server(port + 1, db.clone(), tx.clone())
                .await
                .context("Failed to start REST server")?;

            if has_live {
                println!("Entering live sync (Ctrl+C to stop)...");
                let (beacon_tx, _beacon_rx) = watch::channel(BeaconStatus::NotConfigured);
                let beacon_tx = Arc::new(beacon_tx);
                let syncer =
                    LiveSyncer::new(config, network, db, tx, headers_tx, beacon_tx, Some(data_dir.clone()));
                tokio::select! {
                    res = syncer.run() => {
                        if let Err(e) = res { eprintln!("Live sync error: {e:#}"); }
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
    } else {
        spinner.finish_with_message("Connected to Ethereum mainnet peers ✓");
        run_plain(config, db, network, port, has_live, data_dir).await?;
    }

    Ok(())
}

/// Run the sync pipeline and live sync with plain terminal output.
///
/// Historical sync shows indicatif Stage 1/2/3 progress bars. After historical
/// sync completes, prints the server addresses and enters live sync mode,
/// printing one line per event. Ctrl+C exits cleanly.
async fn run_plain<N: EthNetwork + 'static>(
    config: Config,
    db: Db,
    network: Arc<N>,
    port: u16,
    has_live: bool,
    data_dir: PathBuf,
) -> Result<()> {
    let contracts = config.contracts.len();
    let peers = network.peer_count().await;
    let config_name = "config.toml";
    println!("scopenode  config: {config_name}  contracts: {contracts}  peers: {peers}  port: {port}");

    // Historical sync — visible progress bars from the pipeline.
    let progress = MultiProgress::new();
    let mut pipeline = Pipeline::new(config.clone(), Arc::clone(&network), db.clone());
    pipeline.run(false, &progress).await.context("Pipeline failed")?;

    println!("\nHistorical sync complete.");
    println!("JSON-RPC  → localhost:{port}");
    println!("REST/SSE  → localhost:{}", port + 1);

    let (broadcast_tx, _) = broadcast::channel::<StoredEvent>(1024);
    let (headers_tx, _) = broadcast::channel::<(u64, B256, u64)>(1024);
    // peer_count_atom is set at startup; refreshed on a best-effort basis in the live loop.
    let peer_count_atom = Arc::new(AtomicUsize::new(peers));

    let handle = start_server(port, db.clone(), broadcast_tx.clone(), headers_tx.clone(), Arc::clone(&peer_count_atom))
        .await
        .context("Failed to start JSON-RPC server")?;
    start_rest_server(port + 1, db.clone(), broadcast_tx.clone())
        .await
        .context("Failed to start REST server")?;

    if has_live {
        println!("\nLive sync started — Ctrl+C to stop\n");

        // Subscribe before spawning LiveSyncer so no events are missed in the gap.
        let mut broadcast_rx = broadcast_tx.subscribe();

        let (beacon_tx, _beacon_rx) = watch::channel(BeaconStatus::NotConfigured);
        let beacon_tx = Arc::new(beacon_tx);
        let syncer = LiveSyncer::new(
            config.clone(),
            Arc::clone(&network),
            db.clone(),
            broadcast_tx.clone(),
            headers_tx.clone(),
            Arc::clone(&beacon_tx),
            Some(data_dir.clone()),
        );
        tokio::spawn(async move {
            if let Err(e) = syncer.run().await {
                eprintln!("Live sync error: {e:#}");
            }
        });

        let mut blocks_since_event: u64 = 0;
        let mut tick = tokio::time::interval(Duration::from_secs(6));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                ev = broadcast_rx.recv() => {
                    match ev {
                        Ok(event) => {
                            blocks_since_event = 0;
                            let name = config.contracts.iter()
                                .find(|c| c.address == event.contract)
                                .and_then(|c| c.name.clone())
                                .unwrap_or_else(|| event.contract.to_checksum(None));
                            let tx_short = {
                                let s = format!("{}", event.tx_hash);
                                format!("{}...", &s[..s.len().min(12)])
                            };
                            println!("[{:>10}]  {:<24}  {:<20}  tx={tx_short}",
                                event.block_number, name, event.event_name);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("warn: live event receiver lagged, skipped {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                _ = tick.tick() => {
                    blocks_since_event += 1;
                    if blocks_since_event > 0 && blocks_since_event % 100 == 0 {
                        let p = network.peer_count().await;
                        peer_count_atom.store(p, Ordering::Relaxed);
                        println!("[          ]  live — {p} peers, no events in last 100 blocks");
                    }
                }

                _ = tokio::signal::ctrl_c() => break,
            }
        }
    } else {
        info!(port, "Historical sync done. Press Ctrl+C to stop.");
        tokio::signal::ctrl_c()
            .await
            .context("Failed to listen for Ctrl+C")?;
    }

    println!("\nscopenode stopped.");
    handle.stop()?;
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
