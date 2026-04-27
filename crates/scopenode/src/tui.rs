//! Terminal UI — state model and ratatui rendering.
//!
//! [`AppState`] holds all data the TUI needs to render. It is updated by the
//! sync command via [`AppState::refresh`] (periodic DB poll) and
//! [`AppState::push_event`] (live broadcast events).
//!
//! [`render`] draws the full-screen layout on each tick:
//!
//! ```text
//! ┌─ scopenode ──────────────────────────────────────────────────────┐
//! │ Mode: LIVE   Block: 19,842,301   Speed:  142.0 blk/s  Peers: 12 │
//! ├───────────────────────┬──────────────────────────────────────────┤
//! │ Contracts             │ Recent Events                            │
//! │ Uniswap V3 ETH/USDC  │ 19,842,301  Swap                        │
//! │   Swap    12,431 evts │ 19,842,298  Swap                        │
//! ├───────────────────────┴──────────────────────────────────────────┤
//! │ Peers: 12   Reorgs: 0   Retries: 3                               │
//! │ ████████████████████  19,842,301 / 20,000,000                   │
//! │                                                                  │
//! │ q quit                                                           │
//! └──────────────────────────────────────────────────────────────────┘
//! ```

use std::collections::VecDeque;
use std::io::Stdout;
use std::time::Instant;

use anyhow::Result;
use crossterm::{
    event::{Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Frame, Terminal,
};
use scopenode_core::{beacon::BeaconStatus, config::Config, types::StoredEvent};
use scopenode_storage::Db;
use tokio::sync::watch;

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SyncMode {
    Historical,
    Live,
}

pub struct ContractStat {
    pub name: Option<String>,
    pub address: String,
    pub events: Vec<(String, i64)>,
}

pub struct AppState {
    pub mode: SyncMode,
    pub current_block: u64,
    pub blocks_per_sec: f64,
    pub peer_count: usize,
    pub retry_count: i64,
    pub reorg_count: u64,
    pub contract_stats: Vec<ContractStat>,
    pub recent_events: VecDeque<StoredEvent>,
    /// Current beacon verification state; updated each refresh tick.
    pub beacon_status: BeaconStatus,
    beacon_status_rx: watch::Receiver<BeaconStatus>,
    speed_sample: (Instant, u64),
}

impl AppState {
    pub fn new(config: &Config, beacon_status_rx: watch::Receiver<BeaconStatus>) -> Self {
        Self {
            mode: SyncMode::Historical,
            current_block: 0,
            blocks_per_sec: 0.0,
            peer_count: 0,
            retry_count: 0,
            reorg_count: 0,
            contract_stats: config
                .contracts
                .iter()
                .map(|c| ContractStat {
                    name: c.name.clone(),
                    address: c.address.to_checksum(None),
                    events: c.events.iter().map(|e| (e.clone(), 0i64)).collect(),
                })
                .collect(),
            recent_events: VecDeque::with_capacity(20),
            beacon_status: BeaconStatus::NotConfigured,
            beacon_status_rx,
            speed_sample: (Instant::now(), 0),
        }
    }

    pub fn set_live(&mut self) {
        self.mode = SyncMode::Live;
    }

    /// Poll the DB for current stats, update speed estimate, and sync beacon status.
    pub async fn refresh(&mut self, db: &Db, peer_count: usize) {
        self.peer_count = peer_count;
        self.beacon_status = self.beacon_status_rx.borrow().clone();

        if let Ok(Some(block)) = db.latest_block_number().await {
            let elapsed = self.speed_sample.0.elapsed().as_secs_f64();
            if elapsed >= 0.5 {
                let delta = block.saturating_sub(self.speed_sample.1);
                self.blocks_per_sec = delta as f64 / elapsed;
                self.speed_sample = (Instant::now(), block);
            }
            self.current_block = block;
        }

        for stat in &mut self.contract_stats {
            for (event_name, count) in &mut stat.events {
                if let Ok(n) = db.count_events(&stat.address, event_name).await {
                    *count = n;
                }
            }
        }

        if let Ok(n) = db.count_pending_retry().await {
            self.retry_count = n;
        }
    }

    /// Add a live event to the recent-events list (capped at 20 entries).
    pub fn push_event(&mut self, ev: StoredEvent) {
        if self.recent_events.len() >= 20 {
            self.recent_events.pop_front();
        }
        self.recent_events.push_back(ev);
    }
}

// ── Terminal lifecycle ────────────────────────────────────────────────────────

pub fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Returns `true` if the event should quit the TUI (q, Q, or Ctrl+C).
pub fn is_quit_event(ev: &Event) -> bool {
    matches!(
        ev,
        Event::Key(key)
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL))
    )
}

// ── Rendering ─────────────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, state: &AppState, config: &Config) {
    let area = f.area();

    let outer = Block::default().borders(Borders::ALL).title(" scopenode ");
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(4),
    ])
    .split(inner);

    render_header(f, layout[0], state);
    render_body(f, layout[1], state);
    render_footer(f, layout[2], state, config);
}

fn render_header(f: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let (mode_str, mode_style) = match state.mode {
        SyncMode::Historical => (
            "HISTORICAL",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        SyncMode::Live => (
            "LIVE      ",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
    };

    let mut spans = vec![
        Span::raw("Mode: "),
        Span::styled(mode_str, mode_style),
        Span::raw(format!(
            "   Block: {:>13}   Speed: {:>7.1} blk/s   Peers: {}",
            fmt_block(state.current_block),
            state.blocks_per_sec,
            state.peer_count,
        )),
    ];

    if area.width >= 100 {
        let (label, color) = beacon_label_and_color(&state.beacon_status);
        spans.push(Span::raw("   beacon: "));
        spans.push(Span::styled(label, Style::default().fg(color)));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Map a `BeaconStatus` to a short label and ratatui color for the TUI header.
fn beacon_label_and_color(s: &BeaconStatus) -> (&'static str, Color) {
    match s {
        BeaconStatus::NotConfigured => ("not configured", Color::DarkGray),
        BeaconStatus::Syncing { .. } => ("syncing", Color::Yellow),
        BeaconStatus::Synced { .. } => ("synced", Color::Green),
        BeaconStatus::Stalled { .. } => ("stalled", Color::LightYellow),
        BeaconStatus::Error(_) => ("error", Color::Red),
        BeaconStatus::FallbackUnverified => ("unverified", Color::Red),
    }
}

fn render_body(f: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let cols = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // Contracts panel
    let mut items: Vec<ListItem> = Vec::new();
    for stat in &state.contract_stats {
        let label = stat.name.as_deref().unwrap_or(&stat.address);
        items.push(ListItem::new(Line::from(Span::styled(
            label.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ))));
        for (name, count) in &stat.events {
            items.push(ListItem::new(format!("  {:<16} {:>10} evts", name, count)));
        }
    }
    f.render_widget(
        List::new(items).block(Block::default().title(" Contracts ").borders(Borders::ALL)),
        cols[0],
    );

    // Recent events panel
    let event_items: Vec<ListItem> = state
        .recent_events
        .iter()
        .rev()
        .map(|ev| {
            ListItem::new(format!(
                "{:>13}  {}",
                fmt_block(ev.block_number),
                ev.event_name
            ))
        })
        .collect();
    f.render_widget(
        List::new(event_items)
            .block(Block::default().title(" Recent Events ").borders(Borders::ALL)),
        cols[1],
    );
}

fn render_footer(
    f: &mut Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    config: &Config,
) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    f.render_widget(
        Paragraph::new(format!(
            "Peers: {:>3}   Reorgs: {}   Retries: {}",
            state.peer_count, state.reorg_count, state.retry_count,
        )),
        rows[0],
    );

    let (ratio, label) = sync_progress(state, config);
    let gauge_style = match state.mode {
        SyncMode::Historical => Style::default().fg(Color::Yellow),
        SyncMode::Live => Style::default().fg(Color::Green),
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(gauge_style)
            .ratio(ratio)
            .label(label),
        rows[1],
    );

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "q quit",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[3],
    );
}

fn sync_progress(state: &AppState, config: &Config) -> (f64, String) {
    match state.mode {
        SyncMode::Live => (1.0, "live — following chain tip".to_string()),
        SyncMode::Historical => {
            let from = config.contracts.iter().map(|c| c.from_block).min().unwrap_or(0);
            let to = config.contracts.iter().filter_map(|c| c.to_block).min();
            match to {
                Some(to_block) if to_block > from => {
                    let done = state.current_block.saturating_sub(from);
                    let total = to_block - from;
                    let ratio = (done as f64 / total as f64).clamp(0.0, 1.0);
                    let label = format!("{} / {}", fmt_block(state.current_block), fmt_block(to_block));
                    (ratio, label)
                }
                _ => (0.0, format!("syncing — {}", fmt_block(state.current_block))),
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use std::time::Instant;

    #[test]
    fn beacon_label_not_configured() {
        let (label, color) = beacon_label_and_color(&BeaconStatus::NotConfigured);
        assert_eq!(label, "not configured");
        assert_eq!(color, Color::DarkGray);
    }

    #[test]
    fn beacon_label_syncing() {
        let (label, color) = beacon_label_and_color(&BeaconStatus::Syncing { started: Instant::now() });
        assert_eq!(label, "syncing");
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn beacon_label_synced() {
        let (label, color) = beacon_label_and_color(&BeaconStatus::Synced {
            head_block: 20_000_000,
            head_hash: B256::ZERO,
        });
        assert_eq!(label, "synced");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn beacon_label_stalled() {
        let (label, color) =
            beacon_label_and_color(&BeaconStatus::Stalled { consecutive_mismatches: 3, at_block: 100 });
        assert_eq!(label, "stalled");
        assert_eq!(color, Color::LightYellow);
    }

    #[test]
    fn beacon_label_error() {
        let (label, color) = beacon_label_and_color(&BeaconStatus::Error("timeout".into()));
        assert_eq!(label, "error");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn beacon_label_fallback_unverified() {
        let (label, color) = beacon_label_and_color(&BeaconStatus::FallbackUnverified);
        assert_eq!(label, "unverified");
        assert_eq!(color, Color::Red);
    }
}

/// Format a block number with comma separators: 19842301 → "19,842,301".
fn fmt_block(n: u64) -> String {
    if n == 0 {
        return "—".to_string();
    }
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}
