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

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{EventStream, KeyModifiers};
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
use tokio::sync::{broadcast, oneshot, watch};
use tokio_stream::StreamExt as _;
use tracing::info;

use scopenode_core::beacon::{read_beacon_status, BeaconStatusFile};
use crate::tui::{self, AppState, CommandInput, InputMode, Panel};

/// What the key handler wants the event loop to do after processing a key.
enum KeyAction {
    /// No special action — event loop continues normally.
    None,
    /// Dispatch a subcommand string for async execution.
    RunCommand(String),
    /// Suspend the TUI, hand control to the shell, then resume.
    ShellPause,
}

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
        // TUI mode: clear the spinner line before entering the alternate screen.
        spinner.finish_and_clear();
        run_with_tui(config, db, network, port, has_live, data_dir).await?;
    }

    Ok(())
}

/// Execute a TUI command string and return result lines for display.
///
/// Only read-only subcommands (`query`, `status`, `doctor`) are accepted.
/// All output is returned as `Vec<String>` — never printed to stdout.
async fn execute_command(input: String, db: Db, data_dir: PathBuf) -> Vec<String> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    let Some(&subcommand) = parts.first() else {
        return vec!["error: empty command".to_string()];
    };

    match subcommand {
        "query" => execute_query(&parts[1..], &db).await,
        "status" => execute_status(&db).await,
        "doctor" => execute_doctor(&db, &data_dir).await,
        other => vec![format!("error: unknown or unsafe command '{other}'  (allowed: query, status, doctor)")],
    }
}

async fn execute_query(args: &[&str], db: &Db) -> Vec<String> {
    let mut event_name: Option<String> = None;
    let mut contract: Option<String> = None;
    let mut limit: usize = 20;

    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--event" | "-e" if i + 1 < args.len() => { event_name = Some(args[i + 1].to_string()); i += 2; }
            "--contract" | "-c" if i + 1 < args.len() => { contract = Some(args[i + 1].to_string()); i += 2; }
            "--limit" | "-n" if i + 1 < args.len() => {
                limit = args[i + 1].parse().unwrap_or(20);
                i += 2;
            }
            _ => { i += 1; }
        }
    }

    match db.query_events_for_filter(
        contract.as_deref(), event_name.as_deref(), None, None, None, limit, 0,
    ).await {
        Ok(events) if events.is_empty() => vec!["no results".to_string()],
        Ok(events) => {
            let mut lines = vec![format!("─ {} result(s) ─", events.len())];
            for ev in &events {
                lines.push(format!("{:>13}  {}  {:.12}...", ev.block_number, ev.event_name, ev.tx_hash));
            }
            lines
        }
        Err(e) => vec![format!("error: {e}")],
    }
}

async fn execute_status(db: &Db) -> Vec<String> {
    let mut lines = vec!["─ status ─".to_string()];
    match db.get_all_contracts().await {
        Ok(contracts) => {
            for c in &contracts {
                let label = c.name.as_deref().unwrap_or(&c.address);
                lines.push(format!("  {label}"));
                match db.count_events_for_contract(&c.address).await {
                    Ok(n) => lines.push(format!("    {n} events")),
                    Err(e) => lines.push(format!("    error: {e}")),
                }
            }
        }
        Err(e) => lines.push(format!("error: {e}")),
    }
    match db.db_size_bytes().await {
        Ok(b) => lines.push(format!("  DB size: {} KB", b / 1024)),
        Err(e) => lines.push(format!("  DB size: error: {e}")),
    }
    lines
}

async fn execute_doctor(db: &Db, data_dir: &PathBuf) -> Vec<String> {
    let mut lines = vec!["─ doctor ─".to_string()];

    // Beacon status
    match read_beacon_status(data_dir) {
        Some(BeaconStatusFile::Synced { head_block, .. }) =>
            lines.push(format!("  beacon: synced  block {head_block}")),
        Some(BeaconStatusFile::Syncing { .. }) =>
            lines.push("  beacon: syncing".to_string()),
        Some(BeaconStatusFile::Error { message, .. }) =>
            lines.push(format!("  beacon: error — {message}")),
        Some(BeaconStatusFile::Stalled { consecutive_mismatches, .. }) =>
            lines.push(format!("  beacon: stalled ({consecutive_mismatches} mismatches)")),
        Some(BeaconStatusFile::NotConfigured { .. }) | None =>
            lines.push("  beacon: not configured".to_string()),
        Some(BeaconStatusFile::FallbackUnverified { .. }) =>
            lines.push("  beacon: fallback unverified".to_string()),
    }

    // Pending retries
    match db.count_pending_retry().await {
        Ok(0) => lines.push("  retries: none pending".to_string()),
        Ok(n) => lines.push(format!("  retries: {n} pending")),
        Err(e) => lines.push(format!("  retries: error: {e}")),
    }

    // DB size
    match db.db_size_bytes().await {
        Ok(b) => lines.push(format!("  DB: {} KB", b / 1024)),
        Err(e) => lines.push(format!("  DB: error: {e}")),
    }

    lines
}

/// Process a single key event from the TUI event loop.
///
/// Returns a `KeyAction` telling the event loop what to do next.
fn handle_key(
    ev: &crossterm::event::Event,
    state: &mut AppState,
    _event_stream: &mut EventStream,
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> Result<KeyAction> {
    use crossterm::event::{Event, KeyCode};

    // Command mode: route characters to the input buffer.
    if let InputMode::Command { ref buf } = state.input_mode.clone() {
        let term_width = terminal.size().map(|s| s.width as usize).unwrap_or(80);
        let max_len = term_width.saturating_sub(3);
        match tui::handle_command_input(ev, buf, max_len) {
            CommandInput::Append(c) => {
                if let InputMode::Command { ref mut buf } = state.input_mode {
                    buf.push(c);
                }
            }
            CommandInput::Pop => {
                if let InputMode::Command { ref mut buf } = state.input_mode {
                    buf.pop();
                }
            }
            CommandInput::Submit(s) => {
                let prev = match state.panel.clone() {
                    Panel::Results { prev, .. } => *prev,
                    other => other,
                };
                state.start_command(prev);
                return Ok(KeyAction::RunCommand(s));
            }
            CommandInput::Cancel => {
                state.input_mode = InputMode::Normal;
            }
            CommandInput::Ignored => {}
        }
        return Ok(KeyAction::None);
    }

    // Normal mode key routing.
    let Event::Key(key) = ev else { return Ok(KeyAction::None); };

    // Shell pause — only in Normal mode, not while typing a command.
    if key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(KeyAction::ShellPause);
    }

    // Scroll results panel.
    if let Panel::Results { ref mut scroll, ref lines, .. } = state.panel {
        match key.code {
            KeyCode::Up => {
                *scroll = scroll.saturating_sub(1);
                return Ok(KeyAction::None);
            }
            KeyCode::Down => {
                let max = (lines.len() as u16).saturating_sub(1);
                *scroll = (*scroll + 1).min(max);
                return Ok(KeyAction::None);
            }
            _ => {}
        }
    }

    // Enter command mode.
    if key.code == KeyCode::Char(':') {
        let prev_non_results = match state.panel.clone() {
            Panel::Results { prev: p, .. } => *p,
            other => other,
        };
        state.input_mode = InputMode::Command { buf: String::new() };
        let _ = prev_non_results;
        return Ok(KeyAction::None);
    }

    // Panel toggle / dismiss.
    if let Some(panel) = tui::handle_key_event(ev, &state.input_mode, &state.panel) {
        if matches!(panel, Panel::Events) && matches!(state.panel, Panel::Results { .. }) {
            state.dismiss_results();
        } else {
            state.set_panel(panel);
        }
    }

    Ok(KeyAction::None)
}

/// Temporarily suspend the TUI, give the user a real shell, and resume.
///
/// Sequence (Unix only):
/// 1. Drop the EventStream so the crossterm background thread shuts down cleanly.
/// 2. Save stderr fd and redirect to /dev/null so sync log output doesn't corrupt the shell.
/// 3. Restore the terminal (leave alternate screen, disable raw mode).
/// 4. Print a resume prompt.
/// 5. Block on stdin.read_line via spawn_blocking (keeps tokio runtime alive for the pipeline).
/// 6. Restore stderr.
/// 7. Re-init the terminal and a fresh EventStream.
#[cfg(unix)]
fn do_shell_pause(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    event_stream: &mut EventStream,
) -> Result<()> {
    use std::os::unix::io::RawFd;

    // 1. Drop EventStream — its background OS thread shuts down on Drop.
    let fresh_stream = {
        // We temporarily replace the stream with a new one after resume.
        // Drop the old one first by overwriting.
        drop(std::mem::replace(event_stream, EventStream::new()));
        EventStream::new()
    };

    // 2. Save + redirect stderr.
    let saved_stderr: RawFd = unsafe { libc::dup(2) };
    let devnull = std::fs::OpenOptions::new().write(true).open("/dev/null")
        .map_err(|e| anyhow::anyhow!("open /dev/null: {e}"))?;
    use std::os::unix::io::IntoRawFd;
    unsafe { libc::dup2(devnull.into_raw_fd(), 2); }

    // 3. Restore terminal.
    tui::restore_terminal(terminal)
        .map_err(|e| { unsafe { libc::dup2(saved_stderr, 2); libc::close(saved_stderr); } e })?;

    // 4. Print prompt.
    println!("\nscopenode syncing in background — press Enter to return to TUI");

    // 5. Blocking stdin read (spawn_blocking so tokio threads stay free).
    tokio::task::block_in_place(|| {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    });

    // 6. Restore stderr.
    unsafe { libc::dup2(saved_stderr, 2); libc::close(saved_stderr); }

    // 7. Re-init terminal and EventStream.
    *terminal = tui::init_terminal()?;
    *event_stream = fresh_stream;
    terminal.clear()?;

    Ok(())
}

#[cfg(not(unix))]
fn do_shell_pause(
    _terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    _event_stream: &mut EventStream,
) -> Result<()> {
    // Shell pause not supported on non-Unix platforms.
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
    data_dir: PathBuf,
) -> Result<()> {
    let (beacon_tx, beacon_rx) = watch::channel(BeaconStatus::NotConfigured);
    let beacon_tx = Arc::new(beacon_tx);
    let mut state = AppState::new(&config, beacon_rx);
    let (broadcast_tx, mut broadcast_rx) = broadcast::channel::<StoredEvent>(1024);
    let (headers_tx, _) = broadcast::channel::<(u64, B256, u64)>(1024);
    // Shared peer count: updated on every tick, served via net_peerCount.
    let peer_count_atom = Arc::new(AtomicUsize::new(0));

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
                    peer_count_atom.store(peers, Ordering::Relaxed);
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

                    state.push_log("Historical sync complete.".to_string());

                    let handle = start_server(port, db.clone(), broadcast_tx.clone(), headers_tx.clone(), Arc::clone(&peer_count_atom))
                        .await
                        .context("Failed to start JSON-RPC server")?;
                    start_rest_server(port + 1, db.clone(), broadcast_tx.clone())
                        .await
                        .context("Failed to start REST server")?;
                    state.push_log(format!("JSON-RPC server listening on port {port}."));
                    // Store the stop function for cleanup after the loop.
                    rpc_handle = Some(Box::new(move || handle.stop().map_err(anyhow::Error::from)));

                    if has_live {
                        state.set_live();
                        state.push_log("Live sync started — following chain tip.".to_string());
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
                                tracing::warn!("Live sync error: {e:#}");
                            }
                        });
                    }
                }

                maybe_ev = event_stream.next() => {
                    match maybe_ev {
                        Some(Ok(ref ev)) if tui::is_quit_event(ev, &state.input_mode) => break,
                        Some(Ok(ref ev)) => {
                            match handle_key(ev, &mut state, &mut event_stream, &mut terminal)? {
                                KeyAction::RunCommand(cmd) => {
                                    let db2 = db.clone();
                                    let dir2 = data_dir.clone();
                                    let (tx, rx) = oneshot::channel();
                                    tokio::spawn(async move {
                                        let lines = execute_command(cmd, db2, dir2).await;
                                        let _ = tx.send(lines);
                                    });
                                    state.cmd_result_rx = Some(rx);
                                }
                                KeyAction::ShellPause => {
                                    do_shell_pause(&mut terminal, &mut event_stream)?;
                                }
                                KeyAction::None => {}
                            }
                        }
                        None => break,
                        _ => {}
                    }
                }

                result = async {
                    if let Some(ref mut rx) = state.cmd_result_rx {
                        rx.await.ok()
                    } else {
                        std::future::pending().await
                    }
                } => {
                    state.cmd_result_rx = None;
                    if let Some(lines) = result {
                        state.set_results(lines);
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
