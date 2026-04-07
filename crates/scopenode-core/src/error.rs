//! Error types for the core pipeline.
//!
//! Each pipeline stage has its own error type; [`CoreError`] wraps them all via
//! `From` implementations so `?` can be used throughout the pipeline without
//! manual error conversion at each call site.
//!
//! Error hierarchy:
//! - [`CoreError`] — top-level, returned by [`crate::pipeline::Pipeline::run`]
//!   - [`NetworkError`] — transport failures (headers, receipts)
//!   - [`AbiError`] — Sourcify fetch or ABI decode failures
//!   - [`VerifyError`] — Merkle Patricia Trie root mismatch
//!   - [`ConfigError`] — TOML parse or validation failures
//!   - `DbError` — SQLite errors (re-exported from `scopenode-storage`)

use alloy_primitives::Address;
use thiserror::Error;

/// Top-level error for pipeline operations.
///
/// Returned by [`crate::pipeline::Pipeline::run`] and its sub-methods.
/// Each variant wraps a stage-specific error type, allowing callers to
/// match on the stage that failed without losing the original detail.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("Network error: {0}")]
    Network(#[from] NetworkError),

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

/// Errors from the devp2p network transport layer.
#[derive(Debug, Error)]
pub enum NetworkError {
    /// devp2p stack failed to start (NetworkManager init, discv4 bind, etc.).
    #[error("devp2p boot failed: {0}")]
    Boot(String),

    /// Timed out waiting for enough peers to connect after boot.
    ///
    /// Check internet connectivity and firewall (UDP port must be open for discv4).
    #[error("devp2p found {found} peer(s) after timeout, need at least {wanted}")]
    NoPeers { wanted: usize, found: usize },

    /// All connected peers failed to return headers for this block range.
    ///
    /// Peers may not have this range (pruned node) or the range may be
    /// too far behind for non-archive peers.
    #[error("Failed to fetch headers for blocks {0}..{1} from any peer")]
    HeadersFailed(u64, u64),

    /// All connected peers failed to return receipts for one or more blocks.
    #[error("Failed to fetch receipts: {0}")]
    ReceiptsFailed(String),

    /// A peer request failed (session error, disconnect, protocol violation).
    #[error("Peer request failed: {0}")]
    Peer(String),
}

/// Errors from ABI fetching or decoding.
#[derive(Debug, Error)]
pub enum AbiError {
    /// The contract has no verified source on sourcify.dev.
    ///
    /// The user must set `abi_override` in their config to provide a local ABI file.
    /// Sourcify is the Ethereum Foundation's open verification platform; not all
    /// contracts are verified there.
    #[error("Contract {0} is not verified on Sourcify. Set `abi_override` in your config to provide a local ABI.")]
    NotOnSourcify(Address),

    /// HTTP request to Sourcify failed.
    ///
    /// Could be a network error, DNS failure, or Sourcify being temporarily unavailable.
    #[error("Failed to fetch ABI from Sourcify for {0}: {1}")]
    FetchFailed(Address, String),

    /// Response from Sourcify could not be parsed as a valid ABI.
    ///
    /// This usually means the Sourcify API response format has changed, or the
    /// metadata.json file is malformed for this particular contract.
    #[error("ABI parse error for {0}: {1}")]
    ParseFailed(Address, String),

    /// The event name specified in config does not exist in the ABI.
    ///
    /// Check that the event name in your config exactly matches the Solidity event
    /// declaration (case-sensitive). The ABI uses the canonical name, not an alias.
    #[error("Event '{0}' not found in ABI for contract {1}")]
    EventNotFound(String, Address),

    /// `alloy-dyn-abi` failed to decode a log's data bytes.
    ///
    /// Could indicate a corrupt log, a mismatch between the ABI and the actual
    /// encoding used by the contract, or a proxy pattern that changes the ABI.
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
    /// The config file could not be read from disk.
    #[error("Failed to read config file {0}: {1}")]
    Io(std::path::PathBuf, std::io::Error),

    /// The TOML content could not be parsed into a [`crate::config::Config`].
    ///
    /// Common causes: syntax error, unknown field name (caught by `deny_unknown_fields`),
    /// or a field with the wrong type (e.g. `port = "8545"` instead of `port = 8545`).
    #[error("Failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),

    /// `to_block` is less than `from_block` for a contract — syncing backwards is invalid.
    #[error("Invalid block range for contract {address}: from_block {from} > to_block {to}")]
    InvalidRange { from: u64, to: u64, address: Address },

    /// A contract config lists no events to watch — at least one event is required.
    #[error("Contract {0} has no events configured")]
    NoEvents(Address),
}
