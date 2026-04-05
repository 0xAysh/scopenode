//! `sync` command — runs the full pipeline, then serves the JSON-RPC server.
//!
//! # Modes
//!
//! - **TUI mode** (default): replaces indicatif bars with a full-screen ratatui
//!   UI showing mode, block, speed, peer count, per-event totals, and recent events.
//! - **Quiet mode** (`--quiet`): no progress output, plain text only.
//! - **Dry-run mode** (`--dry-run`): bloom estimate only, no receipt fetch, no TUI.
//!
//! # Steps
//!
//! 1. Apply any `--blocks` range override to all contracts in the config
//! 2. Boot [`DevP2PNetwork`] — connects to Ethereum mainnet peers via devp2p
//! 3. Run [`Pipeline`] — headers → bloom → receipts → decode → store
//! 4. Start JSON-RPC server on the configured port
//! 5. If any contract has no `to_block`, enter live sync mode

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::EventStream;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use scopenode_core::{
    config::Config,
    live::LiveSyncer,
    network::{DevP2PNetwork, EthNetwork},
    pipeline::Pipeline,
    types::StoredEvent,
};
use scopenode_rpc::start_server;
use scopenode_storage::Db;
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;
use tracing::info;

use crate::tui::{self, AppState};

/// Run the `sync` command.
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
            println!("\nSync complete. Starting JSON-RPC server on port {port}...");
            let handle = start_server(port, db.clone())
                .await
                .context("Failed to start JSON-RPC server")?;

            if has_live {
                println!("Entering live sync (Ctrl+C to stop)...");
                let syncer = LiveSyncer::new(config, network, db, tx);
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
        // TUI mode: clear the spinner line before entering the alternate screen.
        spinner.finish_and_clear();
        run_with_tui(config, db, network, port, has_live).await?;
    }

    Ok(())
}

/// Run the sync pipeline and live sync behind a full-screen ratatui TUI.
///
/// The pipeline runs as a spawned task; the TUI event loop runs on the calling
/// task and draws at 500 ms intervals. When the pipeline finishes the TUI
/// switches to LIVE mode and starts the live syncer (if any contract is live).
///
/// The loop exits when the user presses `q`, `Q`, or `Ctrl+C`.
async fn run_with_tui<N: EthNetwork + 'static>(
    config: Config,
    db: Db,
    network: Arc<N>,
    port: u16,
    has_live: bool,
) -> Result<()> {
    let mut state = AppState::new(&config);
    let (broadcast_tx, mut broadcast_rx) = broadcast::channel::<StoredEvent>(1024);

    // Set up the terminal before spawning anything so any early errors go to
    // the alternate screen rather than clobbering the spinner output.
    let mut terminal = tui::init_terminal()?;

    // Pipeline runs in a background task; result is sent via oneshot.
    let (pipeline_done_tx, mut pipeline_done_rx) =
        tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    {
        let db2 = db.clone();
        let net2 = Arc::clone(&network);
        let cfg2 = config.clone();
        tokio::spawn(async move {
            let hidden = {
                let mp = MultiProgress::new();
                mp.set_draw_target(ProgressDrawTarget::hidden());
                mp
            };
            let mut pipeline = Pipeline::new(cfg2, net2, db2);
            let result = pipeline.run(false, &hidden).await.map_err(anyhow::Error::from);
            let _ = pipeline_done_tx.send(result);
        });
    }

    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut pipeline_done = false;
    let mut rpc_handle: Option<Box<dyn FnOnce() -> Result<()> + Send>> = None;

    let result: Result<()> = async {
        loop {
            terminal.draw(|f| tui::render(f, &state, &config))?;

            tokio::select! {
                _ = tick.tick() => {
                    let peers = network.peer_count().await;
                    state.refresh(&db, peers).await;
                }

                ev = broadcast_rx.recv() => {
                    if let Ok(ev) = ev {
                        state.push_event(ev);
                    }
                }

                result = &mut pipeline_done_rx, if !pipeline_done => {
                    pipeline_done = true;
                    let pipeline_result = result
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("pipeline task panicked")));
                    pipeline_result?;

                    let handle = start_server(port, db.clone())
                        .await
                        .context("Failed to start JSON-RPC server")?;
                    // Store the stop function for cleanup after the loop.
                    rpc_handle = Some(Box::new(move || handle.stop().map_err(anyhow::Error::from)));

                    if has_live {
                        state.set_live();
                        let syncer = LiveSyncer::new(
                            config.clone(),
                            Arc::clone(&network),
                            db.clone(),
                            broadcast_tx.clone(),
                        );
                        tokio::spawn(async move {
                            if let Err(e) = syncer.run().await {
                                tracing::warn!("Live sync error: {e:#}");
                            }
                        });
                    }
                }

                maybe_ev = event_stream.next() => {
                    match maybe_ev {
                        Some(Ok(ref ev)) if tui::is_quit_event(ev) => break,
                        None => break,
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    tui::restore_terminal(&mut terminal)?;

    if let Some(stop) = rpc_handle {
        stop()?;
    }

    result
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
