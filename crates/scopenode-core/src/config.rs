//! Configuration types loaded from the TOML config file.
//!
//! All fields map 1:1 to the TOML structure. Unknown fields are rejected
//! (`deny_unknown_fields`) to catch typos early and prevent silent misconfigurations.
//!
//! # Example config
//! ```toml
//! [node]
//! port = 8545
//!
//! [[contracts]]
//! name = "Uniswap V3 USDC/ETH"
//! address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
//! events = ["Swap"]
//! from_block = 12376729
//! ```

use crate::error::ConfigError;
use alloy_primitives::Address;
use serde::Deserialize;
use std::path::PathBuf;

/// Root configuration loaded from `config.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Node-level settings (port, data directory, fallback RPC).
    pub node: NodeConfig,

    /// List of contracts and events to sync. At least one contract is required.
    pub contracts: Vec<ContractConfig>,
}

/// Settings that apply to the whole node instance.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// JSON-RPC server port. Default: 8545 (the standard Ethereum RPC port).
    ///
    /// Set this if port 8545 is already occupied on your machine, or if you
    /// want to run multiple scopenode instances simultaneously.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Directory for the SQLite database and other persistent state.
    ///
    /// Defaults to `~/.scopenode`. Can be overridden by `--data-dir` on the
    /// CLI or the `SCOPENODE_DATA_DIR` environment variable. Tilde expansion
    /// is performed so `~/my-data` works as expected.
    pub data_dir: Option<PathBuf>,
}

/// Configuration for a single contract to sync.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractConfig {
    /// Optional human-readable label shown in progress output and `scopenode status`.
    pub name: Option<String>,

    /// The Ethereum contract address to watch.
    ///
    /// Must be a valid EIP-55 checksummed address or lowercase hex.
    /// alloy's `Address` type handles both formats during deserialization.
    pub address: Address,

    /// List of event names to index (e.g. `["Swap", "Mint"]`).
    ///
    /// Names must exactly match the event name in the contract's ABI.
    /// The match is case-sensitive: `"Transfer"` and `"transfer"` are different.
    pub events: Vec<String>,

    /// First block to sync (inclusive).
    ///
    /// Use the contract's deployment block to avoid scanning empty history.
    /// Scanning from block 0 wastes time — the contract didn't exist then.
    pub from_block: u64,

    /// Last block to sync (inclusive).
    ///
    /// Omit for live sync (Phase 3). When present, the pipeline stops after
    /// processing this block and does not poll for new blocks.
    pub to_block: Option<u64>,

    /// Path to a local ABI JSON file. Use when the contract is not verified on Sourcify.
    ///
    /// The file must be a JSON array of ABI entries in the standard Ethereum ABI format
    /// (same as what `solc --abi` or Hardhat/Foundry produce). Only event entries
    /// are used; function and error entries are ignored.
    pub abi_override: Option<PathBuf>,

    /// Implementation address for proxy contracts (EIP-1967 or any proxy pattern).
    ///
    /// When set, the ABI is fetched from this address on Sourcify instead of `address`.
    /// The proxy contract itself emits the events, so `address` is still used for
    /// bloom scanning and log matching. Only the ABI lookup is redirected.
    ///
    /// Use `scopenode validate <config>` to confirm the ABI resolves correctly.
    pub impl_address: Option<Address>,
}

impl Config {
    /// Load and validate a config from a TOML file.
    ///
    /// Returns [`ConfigError`] if:
    /// - The file cannot be read (e.g. path does not exist or insufficient permissions)
    /// - The TOML cannot be parsed (syntax error or unknown field)
    /// - Validation fails (e.g. `to_block < from_block`, or a contract has no events)
    pub fn from_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(path.to_owned(), e))?;
        let config: Self = toml::from_str(&content).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate all contract configs for logical consistency.
    fn validate(&self) -> Result<(), ConfigError> {
        for c in &self.contracts {
            // Catch block range inversion: syncing backwards is nonsensical.
            if let Some(to) = c.to_block {
                if to < c.from_block {
                    return Err(ConfigError::InvalidRange {
                        from: c.from_block,
                        to,
                        address: c.address,
                    });
                }
            }
            // Require at least one event: a contract with no events would bloom-scan
            // every block but never find anything — likely a config mistake.
            if c.events.is_empty() {
                return Err(ConfigError::NoEvents(c.address));
            }
        }
        Ok(())
    }
}

fn default_port() -> u16 {
    8545
}
