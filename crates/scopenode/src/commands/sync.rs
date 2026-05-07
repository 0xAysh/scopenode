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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use scopenode_core::{
    config::Config,
    daemon::{
        connect_daemon, pid_path, read_live_pid, send_request, socket_path, spawn_daemon,
        DaemonRequest, DaemonResponse,
    },
    network::DevP2PNetwork,
    pipeline::Pipeline,
};
use scopenode_storage::Db;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};
use tracing::info;

/// Run the `sync` command.
///
/// Non-dry-run mode connects to (or starts) the daemon and submits a SyncJob.
/// Dry-run mode runs the bloom estimate in-process without the daemon.
pub async fn run(
    mut config: Config,
    _db: Db,
    dry_run: bool,
    quiet: bool,
    blocks_override: Option<String>,
    data_dir: PathBuf,
    config_path: PathBuf,
) -> Result<()> {
    // Non-dry-run → daemon mode.
    if !dry_run {
        let abs_config = config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.clone());
        return run_daemon_mode(abs_config, blocks_override, &data_dir).await;
    }

    // Dry-run: bloom estimate only, in-process, no daemon.
    if let Some(ref range_str) = blocks_override {
        let (from, to) = parse_blocks_flag(range_str)
            .map_err(|e| anyhow::anyhow!("Invalid --blocks argument \"{range_str}\": {e}"))?;
        for contract in &mut config.contracts {
            contract.from_block = from;
            contract.to_block = to;
        }
        info!(?from, ?to, "--blocks override applied to all contracts");
    }

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
        DevP2PNetwork::start(&data_dir)
            .await
            .context("Failed to start devp2p network")?,
    );
    spinner.finish_with_message("Connected to Ethereum mainnet peers ✓");

    let progress = MultiProgress::new();
    if quiet {
        progress.set_draw_target(ProgressDrawTarget::hidden());
    }
    let mut pipeline = Pipeline::new(config, Arc::clone(&network), _db);
    pipeline.run(true, &progress).await.context("Pipeline failed")?;

    Ok(())
}

// ── Daemon-aware sync ─────────────────────────────────────────────────────────

/// Connect to a running daemon or start one, then submit a SyncJob and stream
/// Progress messages to indicatif bars. Detaches on Ctrl+C — job continues.
async fn run_daemon_mode(
    config_path: PathBuf,
    blocks_override: Option<String>,
    data_dir: &Path,
) -> Result<()> {
    let mut framed = connect_or_start_daemon(data_dir).await?;

    let req = DaemonRequest::SyncJob {
        config_path: config_path.display().to_string(),
        blocks_override,
    };
    send_request(&mut framed, &req).await?;

    use tokio_stream::StreamExt;
    let mp = MultiProgress::new();
    let mut bars: [Option<ProgressBar>; 3] = [None, None, None];

    loop {
        tokio::select! {
            msg = framed.next() => {
                match msg {
                    Some(Ok(line)) => {
                        let resp: DaemonResponse = match serde_json::from_str(&line) {
                            Ok(r) => r,
                            Err(_) => continue,
                        };
                        match resp {
                            DaemonResponse::JobAccepted { job_id, queue_position } => {
                                if queue_position > 1 {
                                    println!("queued at position {queue_position} — waiting for current job to finish...");
                                } else {
                                    println!("job {job_id} accepted — syncing...");
                                }
                            }
                            DaemonResponse::JobStateSnapshot { stage, pos, total, .. } => {
                                ensure_bar(&mp, &mut bars, stage, pos, total);
                            }
                            DaemonResponse::Progress { stage, pos, total, msg, .. } => {
                                let bar = ensure_bar(&mp, &mut bars, stage, pos, total);
                                bar.set_position(pos);
                                if !msg.is_empty() {
                                    bar.set_message(msg);
                                }
                            }
                            DaemonResponse::JobComplete { events_found, .. } => {
                                for bar in bars.iter().flatten() {
                                    bar.finish_and_clear();
                                }
                                println!("done — {events_found} events");
                                return Ok(());
                            }
                            DaemonResponse::Error { msg } => {
                                anyhow::bail!("daemon error: {msg}");
                            }
                            _ => {}
                        }
                    }
                    Some(Err(e)) => anyhow::bail!("connection error: {e}"),
                    None => {
                        println!("daemon disconnected");
                        return Ok(());
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\ndetached — job continues in daemon");
                return Ok(());
            }
        }
    }
}

fn ensure_bar<'a>(
    mp: &MultiProgress,
    bars: &'a mut [Option<ProgressBar>; 3],
    stage: u8,
    pos: u64,
    total: u64,
) -> &'a ProgressBar {
    let idx = stage.saturating_sub(1) as usize;
    let idx = idx.min(2);
    if bars[idx].is_none() {
        let label = match stage {
            1 => "Stage 1/3 Header sync",
            2 => "Stage 2/3 Bloom scan",
            _ => "Stage 3/3 Receipt fetch",
        };
        let bar = mp.add(ProgressBar::new(total.max(1)));
        bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.cyan} {msg} [{bar:40.green/blue}] {pos}/{len}")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );
        bar.set_message(label);
        bar.set_position(pos);
        bars[idx] = Some(bar);
    }
    bars[idx].as_ref().unwrap()
}

/// Connect to a running daemon or spawn one and wait for the socket to appear.
async fn connect_or_start_daemon(data_dir: &Path) -> Result<Framed<UnixStream, LinesCodec>> {
    let pid_p = pid_path(data_dir);
    let sock_p = socket_path(data_dir);

    let daemon_running = read_live_pid(&pid_p).is_some() && sock_p.exists();

    if !daemon_running {
        spawn_daemon(data_dir)?;
        // Retry connecting for up to 3s (30 × 100ms).
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(framed) = connect_daemon(data_dir).await {
                return Ok(framed);
            }
        }
        anyhow::bail!("daemon failed to start — check {}", data_dir.join("daemon.log").display());
    }

    connect_daemon(data_dir)
        .await
        .context("failed to connect to running daemon")
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
