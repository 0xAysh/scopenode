use clap::{ArgAction, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "scopenode", version, about = "Scoped Ethereum node — sync and serve specific contract events")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Override data directory (also: SCOPENODE_DATA_DIR env var)
    #[arg(long, global = true, env = "SCOPENODE_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Increase logging verbosity (repeat for more: -v, -vv, -vvv)
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Suppress progress output
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start sync (resumes if interrupted)
    Sync {
        /// Path to config file
        config: PathBuf,
        /// Show bloom estimate only — do not fetch receipts
        #[arg(long)]
        dry_run: bool,
    },
    /// Show indexed contracts and sync state
    Status,
    /// Query indexed events
    Query {
        /// Filter by contract address
        #[arg(long)]
        contract: Option<String>,
        /// Filter by event name
        #[arg(long)]
        event: Option<String>,
        /// Maximum number of events to return
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Output format: table or json
        #[arg(long, default_value = "table")]
        output: String,
    },
}
