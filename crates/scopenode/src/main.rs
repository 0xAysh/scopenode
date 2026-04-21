//! Binary entry point for `scopenode`.
//!
//! Parses CLI arguments via `clap`, initializes structured logging, opens the
//! SQLite database, and dispatches to the appropriate command handler.
//!
//! # Commands
//! - `scopenode sync <config>` — run the sync pipeline and serve JSON-RPC
//! - `scopenode status` — show indexed contracts and DB stats
//! - `scopenode query` — query indexed events from SQLite

#![deny(warnings)]

mod cli;
mod commands;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use scopenode_core::config::Config;
use scopenode_storage::Db;
use tracing_subscriber::{fmt, EnvFilter};

/// Entry point.
///
/// Sets up structured logging (level controlled by `-v` flags or `RUST_LOG`),
/// then routes to the correct command handler. The `RUST_LOG` environment variable
/// takes precedence over `-v` flags when set.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Map verbosity flags to log levels:
    // (no -v) = warn   — only errors and warnings
    //    -v   = info   — progress messages, important events
    //   -vv   = debug  — per-block details, RPC calls
    //   -vvv  = trace  — everything (very verbose)
    let log_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    // RUST_LOG env var overrides the -v flag when set.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(log_level));
    fmt().with_env_filter(filter).with_target(false).init();

    match &cli.command {
        Command::Sync { config, dry_run, blocks } => {
            let mut cfg =
                Config::from_file(config).context("Failed to load config")?;

            // CLI/env override takes precedence over the config file setting.
            // Order of precedence: --data-dir flag > SCOPENODE_DATA_DIR env > config file > default.
            if let Some(dir) = &cli.data_dir {
                cfg.node.data_dir = Some(dir.clone());
            }

            let data_dir = resolve_data_dir(&cfg);
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("Failed to create data dir: {}", data_dir.display()))?;

            let db = Db::open(data_dir.join("scopenode.db"))
                .await
                .context("Failed to open database")?;

            commands::sync::run(cfg, db, *dry_run, cli.quiet, blocks.clone()).await?;
        }

        Command::Status => {
            let data_dir = resolve_data_dir_no_config(&cli.data_dir);
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("Failed to create data dir: {}", data_dir.display()))?;

            let db = Db::open(data_dir.join("scopenode.db"))
                .await
                .context("Failed to open database")?;

            commands::status::run(db).await?;
        }

        Command::Query {
            contract,
            event,
            from_block,
            to_block,
            limit,
            output,
        } => {
            let data_dir = resolve_data_dir_no_config(&cli.data_dir);
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("Failed to create data dir: {}", data_dir.display()))?;

            let db = Db::open(data_dir.join("scopenode.db"))
                .await
                .context("Failed to open database")?;

            commands::query::run(
                db,
                contract.clone(),
                event.clone(),
                *from_block,
                *to_block,
                *limit,
                output.clone(),
            )
            .await?;
        }

        Command::Abi { address } => {
            commands::abi::run(address).await?;
        }

        Command::Validate { config } => {
            commands::validate::run(config.clone()).await?;
        }

        Command::Retry { config } => {
            let cfg = Config::from_file(config).context("Failed to load config")?;
            let data_dir = resolve_data_dir(&cfg);
            let db = Db::open(data_dir.join("scopenode.db"))
                .await
                .context("Failed to open database")?;
            commands::retry::run(cfg, db).await?;
        }

        Command::Snapshot { label } => {
            let data_dir = resolve_data_dir_no_config(&cli.data_dir);
            commands::snapshot::run(&data_dir, label.clone()).await?;
        }

        Command::Restore { label } => {
            let data_dir = resolve_data_dir_no_config(&cli.data_dir);
            commands::restore::run(&data_dir, label.clone()).await?;
        }

        Command::Doctor => {
            let data_dir = resolve_data_dir_no_config(&cli.data_dir);
            let db = Db::open(data_dir.join("scopenode.db"))
                .await
                .context("Failed to open database")?;
            commands::doctor::run(db).await?;
        }

        Command::Export {
            contract,
            event,
            topic0,
            from_block,
            to_block,
            format,
        } => {
            let data_dir = resolve_data_dir_no_config(&cli.data_dir);
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("Failed to create data dir: {}", data_dir.display()))?;

            let db = Db::open(data_dir.join("scopenode.db"))
                .await
                .context("Failed to open database")?;

            commands::export::run(
                db,
                contract.clone(),
                event.clone(),
                topic0.clone(),
                *from_block,
                *to_block,
                format.clone(),
            )
            .await?;
        }
    }

    Ok(())
}

/// Resolve the data directory from a loaded config (which may have CLI override applied).
///
/// Priority: CLI `--data-dir` flag (already applied to `config.node.data_dir`)
/// > `SCOPENODE_DATA_DIR` env var (not yet implemented here, applied above)
/// > `data_dir` in config file > `~/.scopenode` default.
fn resolve_data_dir(config: &Config) -> std::path::PathBuf {
    config
        .node
        .data_dir
        .clone()
        .map(expand_tilde)
        .unwrap_or_else(default_data_dir)
}

/// Resolve the data directory without a config file (for `status` and `query` commands).
///
/// These commands don't require a config, so they read the data dir from the
/// `--data-dir` CLI flag or fall back to the default `~/.scopenode`.
fn resolve_data_dir_no_config(override_dir: &Option<std::path::PathBuf>) -> std::path::PathBuf {
    override_dir
        .clone()
        .map(expand_tilde)
        .unwrap_or_else(default_data_dir)
}

/// Default data directory: `~/.scopenode`.
///
/// Falls back to `./.scopenode` (relative to cwd) if the home directory cannot
/// be determined (unusual on most systems, but possible in containers).
fn default_data_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".scopenode")
}

/// Expand a leading `~/` to the user's home directory.
///
/// Does nothing if the path doesn't start with `~/`. This allows users to write
/// `data_dir = "~/my-data"` in the config file and have it work as expected.
fn expand_tilde(path: std::path::PathBuf) -> std::path::PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(stripped) = s.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(stripped);
            }
        }
    }
    path
}
