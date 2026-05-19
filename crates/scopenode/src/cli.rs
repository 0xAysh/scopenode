use clap::{ArgAction, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "scopenode",
    version,
    about = "Scoped Ethereum node — sync and serve specific contract events"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Increase logging verbosity (repeat for more: -v, -vv, -vvv)
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Read ERA1 files and index contract events into SQLite
    Sync {
        /// Path to config file
        #[arg(long, default_value = "./config.toml")]
        config: PathBuf,
        /// Validate config and report ERA1 source path without indexing
        #[arg(long)]
        dry_run: bool,
    },
    /// Start the JSON-RPC (:8545) and REST (:8546) servers
    Serve {
        /// Path to config file
        #[arg(long, default_value = "./config.toml")]
        config: PathBuf,
    },
    /// Show indexed contracts, event counts, block range, and serving URLs
    Status {
        /// Path to config file
        #[arg(long, default_value = "./config.toml")]
        config: PathBuf,
    },
}
