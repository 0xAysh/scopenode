//! Error types for the core pipeline.

use alloy_primitives::Address;
use thiserror::Error;

/// Top-level error for pipeline operations.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("ABI error: {0}")]
    Abi(#[from] AbiError),

    #[error("Verification error: {0}")]
    Verify(#[from] VerifyError),

    #[error("Config error: {0}")]
    Config(#[from] ConfigError),

    #[error("Storage error: {0}")]
    Storage(#[from] scopenode_storage::DbError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

/// Errors from ABI loading or decoding.
#[derive(Debug, Error)]
pub enum AbiError {
    /// `abi_override` is absent — required for ERA1 sync.
    #[error("Contract {0} has no abi_override set. Add abi_override = \"./path/to/abi.json\" to your contract config.")]
    AbiRequired(Address),

    /// ABI parse error.
    #[error("ABI parse error for {0}: {1}")]
    ParseFailed(Address, String),

    /// The event name specified in config does not exist in the ABI.
    #[error("Event '{0}' not found in ABI for contract {1}")]
    EventNotFound(String, Address),

    /// `alloy-dyn-abi` failed to decode a log's data bytes.
    #[error("ABI decode error: {0}")]
    Decode(String),

    /// Error reading a local `abi_override` file.
    #[error("I/O error reading ABI override: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors from Merkle Patricia Trie verification of receipts.
#[derive(Debug, Error)]
pub enum VerifyError {
    /// The trie root computed from fetched receipts does not match `receipts_root`
    /// in the block header.
    ///
    /// This means the peer sent tampered or incorrect receipt data. The block is
    /// marked `pending_retry` and will be retried with a different peer in Phase 3a.
    /// This check provides the core security guarantee: a peer cannot fabricate
    /// events without also forging the block header, which requires PoW or PoS work.
    #[error("Receipts root mismatch for block {block_num}: expected {expected}, got {computed}")]
    RootMismatch {
        block_num: u64,
        expected: alloy_primitives::B256,
        computed: alloy_primitives::B256,
    },
}

/// Errors from loading or validating the TOML config file.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file {0}: {1}")]
    Io(std::path::PathBuf, std::io::Error),

    #[error("Failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("Invalid block range for contract {address}: from_block {from} > to_block {to}")]
    InvalidRange {
        from: u64,
        to: u64,
        address: Address,
    },

    #[error("Contract {0} has no events configured")]
    NoEvents(Address),

    #[error("Contract {0} requires abi_override — add abi_override = \"./path/to/abi.json\" to your contract config")]
    AbiOverrideRequired(Address),

    #[error("Contract {0} requires to_block — ERA1 sync requires a bounded block range")]
    ToBlockRequired(Address),
}
