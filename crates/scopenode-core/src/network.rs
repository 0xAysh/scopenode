//! Network transport abstraction — devp2p peers only, no RPC providers.
//!
//! # Architecture
//!
//! ```text
//!  EthNetwork trait          implemented by
//!  ─────────────────         ──────────────
//!  get_headers()         ──▶ DevP2PNetwork  (GetBlockHeaders via ETH wire)
//!  get_receipts_for_blocks() DevP2PNetwork  (GetReceipts via ETH wire)
//!  best_block_number()       DevP2PNetwork  (from peers' Status message)
//! ```
//!
//! The pipeline (`Pipeline<N: EthNetwork>`) is generic over the transport.
//! Swapping transports in Phase 2 (multi-peer agreement, ERA1 fallback)
//! requires only changes in this file — the pipeline is untouched.
//!
//! # Phase 1 status
//!
//! `DevP2PNetwork` is stubbed out. Implement it using:
//! - `reth-discv4`    — peer discovery (Kademlia DHT, mainnet bootnodes)
//! - `reth-network`   — RLPx transport + peer session management
//! - `reth-eth-wire`  — ETH sub-protocol messages (GetBlockHeaders, GetReceipts)
//! - `reth-network-api` — FetchClient trait for making requests
//!
//! See `phases/PHASE_01_mvp.md` for the full devp2p boot sequence and API notes.

use crate::error::NetworkError;
use crate::types::ScopeHeader;
use alloy::rpc::types::TransactionReceipt;
use alloy_primitives::B256;
use async_trait::async_trait;

/// Result of fetching receipts for a single block from devp2p peers.
///
/// `Ok` — receipts retrieved, ready for Merkle verification in the pipeline.
/// `Failed` — all peers failed; pipeline will call `db.mark_retry()`.
pub enum ReceiptFetchResult {
    /// Receipts successfully retrieved from a peer.
    ///
    /// Merkle verification is done by the pipeline (not here) using
    /// `receipts_root` from the stored header.
    Ok {
        block_num: u64,
        block_hash: B256,
        receipts: Vec<TransactionReceipt>,
    },
    /// All devp2p peers failed to return receipts for this block.
    ///
    /// Block is marked `pending_retry = 1` in `bloom_candidates`.
    /// `scopenode retry` (Phase 3a) will re-attempt with fresh peers.
    Failed { block_num: u64 },
}

/// Transport abstraction over the Ethereum data source.
///
/// The pipeline is generic over `N: EthNetwork` — the compiler monomorphises
/// the pipeline for the concrete type, giving zero runtime dispatch overhead.
///
/// # Contract
///
/// - `get_headers`: may return fewer headers than requested if peers lack them.
///   Callers must handle short responses (partial ranges are safe to store).
/// - `get_receipts_for_blocks`: returns exactly one `ReceiptFetchResult` per
///   input block. Never fewer. Never panics on peer failure — always `Failed`.
/// - `best_block_number`: returns the chain tip as reported by connected peers.
///   Used when `to_block` is omitted from config.
#[async_trait]
pub trait EthNetwork: Send + Sync {
    /// Fetch block headers for the inclusive range `[from, to]`.
    ///
    /// Headers contain `logs_bloom` (for bloom scan) and `receipts_root`
    /// (for Merkle verification). Returned vec may be shorter than
    /// `to - from + 1` if some peers don't have the requested range.
    async fn get_headers(&self, from: u64, to: u64) -> Result<Vec<ScopeHeader>, NetworkError>;

    /// Fetch receipts for a batch of blocks (up to 16 — ETH wire protocol limit).
    ///
    /// Each element: `(block_num, block_hash, receipts_root)`.
    /// Merkle verification is NOT done here — the pipeline calls
    /// `verify_receipts()` after this returns.
    async fn get_receipts_for_blocks(
        &self,
        blocks: &[(u64, B256, B256)], // (block_num, block_hash, receipts_root)
    ) -> Vec<ReceiptFetchResult>;

    /// Return the highest block number known to connected peers.
    ///
    /// Derived from the `Status` message peers send during the ETH handshake.
    /// Used as `to_block` when the config omits it (live-tip sync).
    async fn best_block_number(&self) -> Result<u64, NetworkError>;
}

/// devp2p-backed implementation of [`EthNetwork`].
///
/// # Boot sequence
///
/// ```text
/// 1. Generate ephemeral secp256k1 node key
/// 2. Configure discv4 with Ethereum mainnet bootnodes
/// 3. Spawn NetworkManager (tokio task — runs for process lifetime)
/// 4. Wait for ≥3 connected peers (discv4 takes ~5s)
/// 5. Use NetworkHandle / FetchClient for GetBlockHeaders + GetReceipts
/// ```
///
/// # Not yet implemented
///
/// Add to `Cargo.toml` before implementing:
/// ```toml
/// reth-discv4      = { version = "1", default-features = false }
/// reth-network     = { version = "1", default-features = false }
/// reth-eth-wire    = { version = "1", default-features = false }
/// reth-network-api = { version = "1", default-features = false }
/// secp256k1        = { version = "0.29", features = ["rand", "global-context"] }
/// ```
///
/// Read `reth-network-api` source to map the exact `FetchClient` method names
/// before writing a single line — the API evolves with each reth release.
pub struct DevP2PNetwork;

impl DevP2PNetwork {
    /// Boot the devp2p stack and wait for initial peer connections.
    ///
    /// Blocks until at least 3 peers are connected or 30s timeout elapses.
    /// Spawns a `NetworkManager` tokio task that runs for the process lifetime.
    pub async fn start() -> Result<Self, NetworkError> {
        unimplemented!(
            "DevP2PNetwork not yet implemented. \
             See phases/PHASE_01_mvp.md for the full implementation plan. \
             Requires: reth-discv4, reth-network, reth-eth-wire, reth-network-api."
        )
    }
}

#[async_trait]
impl EthNetwork for DevP2PNetwork {
    async fn get_headers(&self, _from: u64, _to: u64) -> Result<Vec<ScopeHeader>, NetworkError> {
        unimplemented!(
            "DevP2PNetwork::get_headers — \
             implement via reth-network FetchClient::get_block_headers()"
        )
    }

    async fn get_receipts_for_blocks(
        &self,
        _blocks: &[(u64, B256, B256)],
    ) -> Vec<ReceiptFetchResult> {
        unimplemented!(
            "DevP2PNetwork::get_receipts_for_blocks — \
             implement via reth-network FetchClient::get_receipts()"
        )
    }

    async fn best_block_number(&self) -> Result<u64, NetworkError> {
        unimplemented!(
            "DevP2PNetwork::best_block_number — \
             derive from peers' Status message via NetworkHandle"
        )
    }
}
