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
        /// Override block range for all contracts in the config.
        ///
        /// Formats:
        ///   `16M:17M`    — from 16,000,000 to 17,000,000 (inclusive)
        ///   `16M:+1000`  — from 16,000,000 to 16,001,000 (relative offset)
        ///
        /// Shorthand suffixes: `M` = 1,000,000 · `K` = 1,000
        #[arg(long, value_name = "RANGE")]
        blocks: Option<String>,
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
        /// Filter by raw keccak256 topic0 hash (0x-prefixed hex)
        #[arg(long)]
        topic0: Option<String>,
        /// Only return events at or after this block number
        #[arg(long)]
        from_block: Option<u64>,
        /// Only return events at or before this block number
        #[arg(long)]
        to_block: Option<u64>,
        /// Maximum number of events to return
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Output format: table or json
        #[arg(long, default_value = "table")]
        output: String,
    },
    /// Fetch and display a contract's ABI from Sourcify
    Abi {
        /// Contract address (0x...)
        address: String,
    },
    /// Check a config file before syncing — validates addresses, ABIs, and event names
    Validate {
        /// Path to config file
        config: PathBuf,
    },
    /// Re-fetch all blocks that failed receipt verification
    Retry {
        /// Path to config file
        config: PathBuf,
    },
    /// Save a snapshot of the database
    Snapshot {
        /// Optional label for this snapshot (default: timestamp)
        #[arg(long)]
        label: Option<String>,
    },
    /// Restore the database from a snapshot
    Restore {
        /// Label of the snapshot to restore (default: most recent)
        #[arg(long)]
        label: Option<String>,
    },
    /// Check node health: peers, beacon head, DB integrity, retry queue
    Doctor,
    /// Export all matching events to stdout as CSV or JSON (no row cap)
    Export {
        /// Filter by contract address
        #[arg(long)]
        contract: Option<String>,
        /// Filter by event name (e.g. "Swap")
        #[arg(long)]
        event: Option<String>,
        /// Filter by raw keccak256 topic0 hash (0x-prefixed hex)
        #[arg(long)]
        topic0: Option<String>,
        /// Only export events at or after this block number
        #[arg(long)]
        from_block: Option<u64>,
        /// Only export events at or before this block number
        #[arg(long)]
        to_block: Option<u64>,
        /// Output format: csv (default) or json
        #[arg(long, default_value = "csv")]
        format: String,
    },
}
