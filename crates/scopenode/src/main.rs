//! Binary entry point for `scopenode`.

#![deny(warnings)]

mod cli;
mod commands;
mod runtime;
mod sourcify;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));
    fmt().with_env_filter(filter).with_target(false).init();

    match cli.command {
        Command::Sync { config, dry_run } => {
            commands::sync::execute(config, dry_run, false).await?;
        }
        Command::Serve { config } => {
            commands::serve::execute(config).await?;
        }
        Command::Status { config } => {
            commands::status::execute(config).await?;
        }
    }

    Ok(())
}
