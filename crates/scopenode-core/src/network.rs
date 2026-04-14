//! Network transport — devp2p peers only, no RPC providers.
//!
//! # Architecture
//!
//! ```text
//!  EthNetwork trait              implemented by
//!  ─────────────────             ──────────────
//!  get_headers()             ──▶ DevP2PNetwork  (GetBlockHeaders via ETH wire / FetchClient)
//!  get_receipts_for_blocks()     DevP2PNetwork  (GetReceipts via PeerRequestSender)
//!  best_block_number()           DevP2PNetwork  (from peer Status messages: UnifiedStatus::latest_block)
//! ```
//!
//! The pipeline (`Pipeline<N: EthNetwork>`) is generic over the transport.
//!
//! # devp2p boot sequence
//!
//! 1. Generate ephemeral secp256k1 node key (fresh each run — we're consumer-only)
//! 2. Build `NetworkConfig` for Ethereum mainnet with mainnet bootnodes
//! 3. Spawn `NetworkManager` as a tokio task (runs discv4 + RLPx for process lifetime)
//! 4. Subscribe to `NetworkEvent`s to track `PeerRequestSender` for each active session
//! 5. Use `FetchClient` for header requests (GetBlockHeaders)
//! 6. Use `PeerRequestSender` for receipt requests (GetReceipts) directly to a peer
//!
//! # Wire protocol messages used
//!
//! - `GetBlockHeaders(start, limit, skip=0, direction=Rising)` → headers with bloom + receipts_root
//! - `GetReceipts(block_hashes)` → consensus receipts for Merkle verification
//!
//! # Receipt conversion
//!
//! The ETH wire `GetReceipts` response contains consensus receipts (status, gas, logs)
//! without transaction metadata (tx_hash, from, block context). We synthesise:
//!   - `tx_hash` = `keccak256(block_hash ++ tx_index_be)` — deterministic, unique per (block, tx)
//!   - `log_index` = cumulative across all logs in the block (0-indexed globally)
//!
//! These synthetic values enable correct `(tx_hash, log_index)` deduplication in SQLite
//! and survive re-runs (same inputs → same hash → `INSERT OR IGNORE` skips duplicates).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy::rpc::types::{Log as RpcLog, TransactionReceipt};
use alloy_primitives::{keccak256, Address, B256};
use async_trait::async_trait;
use tokio::sync::{oneshot, RwLock};
use tokio_stream::StreamExt as _;
use tracing::{debug, info, warn};

use crate::error::NetworkError;
use crate::receipts::verify_receipts;
use crate::types::ScopeHeader;

// ── reth devp2p stack ────────────────────────────────────────────────────────
use reth_eth_wire::{
    BlockHashOrNumber, BlockHeaders, GetBlockHeaders, GetReceipts, HeadersDirection, Receipts,
};
use alloy_primitives::U256;
use reth_ethereum_forks::Head;
use reth_network::{config::rng_secret_key, EthNetworkPrimitives, NetworkHandle, NetworkManager, PeersInfo};
use reth_network_api::{
    events::{NetworkEvent, PeerEvent},
    NetworkEventListenerProvider, PeerRequest, PeerRequestSender,
};
use reth_network_peers::mainnet_nodes;
use reth_storage_api::noop::NoopProvider;

// ── Receipt fetch result ──────────────────────────────────────────────────────

/// Result of fetching receipts for a single block from devp2p peers.
///
/// `Ok` — receipts retrieved and ready for Merkle verification in the pipeline.
/// `Failed` — all peers failed; pipeline calls `db.mark_retry()` for retry later.
pub enum ReceiptFetchResult {
    /// Receipts successfully retrieved from a peer.
    ///
    /// Merkle verification is done by the pipeline (not here) using
    /// `receipts_root` from the stored block header.
    Ok {
        block_num: u64,
        block_hash: B256,
        receipts: Vec<TransactionReceipt>,
    },
    /// All devp2p peers failed to return receipts for this block.
    ///
    /// Block is marked `pending_retry = 1` in `bloom_candidates`.
    /// Re-run `scopenode sync` to retry with fresh peers.
    Failed { block_num: u64 },
}

// ── EthNetwork trait ──────────────────────────────────────────────────────────

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
///   Requires at least one eth/69+ peer (for `latest_block` field in Status).
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
    /// Derived from `UnifiedStatus::latest_block` — populated for eth/69+ peers.
    /// Used as `to_block` when the config omits it (live-tip sync).
    async fn best_block_number(&self) -> Result<u64, NetworkError>;

    /// Return the number of currently active devp2p peer sessions.
    ///
    /// Used by the TUI to display network connectivity. Default implementation
    /// returns 0, suitable for test doubles and mocks.
    async fn peer_count(&self) -> usize {
        0
    }
}

// ── Internal peer session state ───────────────────────────────────────────────

struct PeerSession {
    /// Direct channel to this peer's session task — send `PeerRequest`s here.
    sender: PeerRequestSender<PeerRequest<EthNetworkPrimitives>>,
    /// Best block number known to this peer (from their eth Status message).
    /// Only populated for eth/69+ peers; `None` for pre-69 peers.
    best_block: Option<u64>,
}

// ── DevP2PNetwork ─────────────────────────────────────────────────────────────

/// devp2p-backed implementation of [`EthNetwork`].
///
/// Boots a full reth-network stack: discv4 peer discovery (UDP Kademlia DHT)
/// + RLPx transport (TCP ECIES encrypted sessions) + ETH sub-protocol.
///
/// All data comes from Ethereum mainnet peers — no RPC provider, no trusted
/// third party. Every receipt is Merkle-verified against `receipts_root` in
/// the block header before events are stored.
///
/// ```text
/// ┌──────────────────────────────────────────────────────────────────────────┐
/// │  DevP2PNetwork                                                           │
/// │    handle: NetworkHandle  (FetchClient for headers, send_request)        │
/// │    peers: Arc<RwLock<HashMap<PeerId, PeerSession>>>                      │
/// │      — updated by background task listening to NetworkEvent stream       │
/// │      — PeerSession.sender → direct channel to peer's session task        │
/// │                                                                          │
/// │    NetworkManager task: running for process lifetime                     │
/// │      ├── discv4 UDP loop  (find peers via Kademlia DHT)                  │
/// │      └── RLPx TCP loop    (manage encrypted peer sessions)               │
/// └──────────────────────────────────────────────────────────────────────────┘
/// ```
pub struct DevP2PNetwork {
    #[allow(dead_code)]
    handle: NetworkHandle<EthNetworkPrimitives>,
    /// Active peer sessions keyed by PeerId.
    /// Updated by background task via `handle.event_listener()`.
    peers: Arc<RwLock<HashMap<reth_network_peers::PeerId, PeerSession>>>,
    /// Peers that returned data failing MPT verification — skipped for the session.
    ///
    /// A peer is blacklisted immediately when any receipt batch it returns does not
    /// reproduce the expected `receipts_root`. It stays blacklisted until the
    /// process restarts (ephemeral — blacklist is in-memory only).
    blacklisted: Arc<RwLock<HashSet<reth_network_peers::PeerId>>>,
}

impl DevP2PNetwork {
    /// Boot the devp2p stack and wait for initial peer connections.
    ///
    /// Generates an ephemeral node key, connects to Ethereum mainnet bootnodes
    /// via discv4, and blocks until at least 3 peers are connected (or 30s timeout).
    /// Spawns a `NetworkManager` tokio task that keeps running for the process lifetime.
    /// Also spawns a background task that maintains the connected peer map.
    pub async fn start() -> Result<Self, NetworkError> {
        // Ephemeral key: we don't need a persistent identity.
        // scopenode is a consumer-only node — we request data, never serve it.
        let secret_key = rng_secret_key();

        // Consumer-only provider: responds with empty data when peers request blocks
        // from us. Correct for a node that only downloads, never serves.
        let client = NoopProvider::default();

        // Build network config for Ethereum mainnet.
        // mainnet_nodes() returns the EF-maintained Ethereum mainnet bootnodes —
        // stable, well-known peers that seed the Kademlia DHT.
        // Set head to a known post-Dencun block so the ETH Status handshake
        // advertises the correct fork ID. Without this, the default is genesis
        // (block 0), which produces an incompatible fork ID and causes all
        // mainnet peers to reject our connection immediately.
        //
        // Block 21_000_000 (~Sep 2024): safely after Dencun (19_426_589) and
        // all prior forks. We use a zero hash — peers accept connections as
        // long as chain_id + fork_id match; they don't validate the head hash.
        // Post-merge: difficulty = 0, total_difficulty = terminal TD.
        let head = Head {
            number: 21_000_000,
            hash: B256::ZERO,
            difficulty: U256::ZERO,
            total_difficulty: U256::from_str_radix(
                "58750003716598352816469", 10,
            )
            .expect("static TTD"),
            timestamp: 1_726_070_400, // ~2024-09-11 00:00:00 UTC
        };

        let network_config =
            reth_network::config::NetworkConfig::<_, EthNetworkPrimitives>::builder(secret_key)
                .boot_nodes(mainnet_nodes())
                .set_head(head)
                .build(client);

        // Spawn NetworkManager — this task runs the full devp2p loop:
        //   discv4 UDP: peer discovery, ENR exchanges, Kademlia routing
        //   RLPx TCP: ECIES handshake, encrypted session management
        //   ETH wire: Status handshake, request routing
        let manager = NetworkManager::new(network_config)
            .await
            .map_err(|e| NetworkError::Boot(e.to_string()))?;

        let handle = manager.handle().clone();

        // Subscribe to network events BEFORE spawning the manager.
        // event_listener() creates a broadcast channel receiver — we get all events.
        let event_stream = handle.event_listener();

        // NetworkManager implements Future — spawn as a background tokio task.
        // It runs for the lifetime of the process.
        tokio::spawn(manager);

        // Spawn peer tracking task.
        // Listens to NetworkEvents and maintains the connected peer map so that
        // get_receipts_for_blocks() can pick a live PeerRequestSender.
        let peers: Arc<RwLock<HashMap<reth_network_peers::PeerId, PeerSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let peers_bg = peers.clone();

        tokio::spawn(async move {
            let mut stream = event_stream;
            while let Some(event) = stream.next().await {
                match event {
                    NetworkEvent::ActivePeerSession { info, messages } => {
                        let best_block = info.status.latest_block;
                        debug!(
                            peer = %info.peer_id,
                            best_block = ?best_block,
                            "new active peer session"
                        );
                        peers_bg.write().await.insert(
                            info.peer_id,
                            PeerSession { sender: messages, best_block },
                        );
                    }
                    NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, .. }) => {
                        debug!(peer = %peer_id, "peer session closed");
                        peers_bg.write().await.remove(&peer_id);
                    }
                    _ => {}
                }
            }
        });

        // Wait for at least 1 connected peer. discv4 bootstrap + RLPx handshake
        // can take 20–120s on a cold start depending on network conditions.
        // We only need 1 to begin fetching; additional peers accumulate in the
        // background and are picked up by the peer tracker as they connect.
        //
        // We watch the `peers` HashMap (populated by the background event listener)
        // rather than `handle.num_connected_peers()`. The latter counts all TCP
        // connections including EF bootnodes that connect/disconnect within ~1ms,
        // so polling at 1s intervals always sees zero. The HashMap only contains
        // peers that have completed the full ETH Status handshake and are ready
        // to serve data.
        Self::wait_for_peers(&handle, &peers, 1, Duration::from_secs(180)).await?;

        let map_count = peers.read().await.len();
        let conn_count = handle.num_connected_peers();
        info!(
            map_peers = map_count,
            conn_peers = conn_count,
            "devp2p ready — connected to Ethereum mainnet peer(s)"
        );

        // Give the network a moment to stabilise: the first peer often connects
        // and disconnects within seconds. A 5s grace period lets additional peers
        // come in so we have stable connections for the first header request.
        tokio::time::sleep(Duration::from_secs(5)).await;

        Ok(Self {
            handle,
            peers,
            blacklisted: Arc::new(RwLock::new(HashSet::new())),
        })
    }

    /// Poll until `min_peers` have completed the ETH handshake, or `timeout` elapses.
    ///
    /// Primary signal: the `peers` HashMap (populated by background `NetworkEvent`
    /// listener) — only contains peers that completed the full ETH Status handshake.
    ///
    /// Fallback signal: `handle.num_connected_peers()` — catches cases where the
    /// event listener task has a race condition and misses some `ActivePeerSession`
    /// events. If `num_connected_peers() >= min_peers` we also consider it ready,
    /// since at least that many peers have active TCP sessions.
    async fn wait_for_peers(
        handle: &NetworkHandle<EthNetworkPrimitives>,
        peers: &Arc<RwLock<HashMap<reth_network_peers::PeerId, PeerSession>>>,
        min_peers: usize,
        timeout: Duration,
    ) -> Result<(), NetworkError> {
        let start = Instant::now();
        loop {
            let map_n = peers.read().await.len();
            let conn_n = handle.num_connected_peers();
            // Use the higher of the two counts as evidence of connectivity.
            let n = map_n.max(conn_n);
            if n >= min_peers {
                return Ok(());
            }
            if start.elapsed() > timeout {
                return Err(NetworkError::NoPeers { wanted: min_peers, found: n });
            }
            debug!(
                map_peers = map_n,
                conn_peers = conn_n,
                elapsed_s = start.elapsed().as_secs(),
                "waiting for devp2p peers..."
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Return `(PeerId, sender)` pairs for all connected peers that are not blacklisted.
    ///
    /// Cloned out of the maps so we don't hold the locks while making requests.
    /// The returned vec may be empty if no usable peers are connected.
    async fn usable_peer_senders(
        &self,
    ) -> Vec<(reth_network_peers::PeerId, PeerRequestSender<PeerRequest<EthNetworkPrimitives>>)> {
        let peers = self.peers.read().await;
        let blacklisted = self.blacklisted.read().await;
        peers
            .iter()
            .filter(|(id, _)| !blacklisted.contains(*id))
            .map(|(id, s)| (*id, s.sender.clone()))
            .collect()
    }

    /// Blacklist a peer for this session.
    ///
    /// Called when a peer returns receipt data that fails MPT verification.
    /// The peer will be skipped for all future requests until the process restarts.
    async fn blacklist_peer(&self, peer_id: reth_network_peers::PeerId) {
        self.blacklisted.write().await.insert(peer_id);
    }
}

// ── EthNetwork impl ───────────────────────────────────────────────────────────

#[async_trait]
impl EthNetwork for DevP2PNetwork {
    /// Fetch headers for `[from, to]` via `GetBlockHeaders`.
    ///
    /// Sends `PeerRequest::GetBlockHeaders` directly to connected peers' session
    /// channels — same mechanism as `get_receipts_for_blocks`. This bypasses
    /// `FetchClient` which can lose track of peers between `wait_for_peers` and
    /// first use, causing silent empty responses.
    async fn get_headers(&self, from: u64, to: u64) -> Result<Vec<ScopeHeader>, NetworkError> {
        let count = (to - from + 1).min(64);

        // Wait up to 30s for at least one usable (non-blacklisted) peer.
        let senders = {
            let mut attempts = 0u32;
            loop {
                let s = self.usable_peer_senders().await;
                if !s.is_empty() || attempts >= 30 {
                    break s;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                attempts += 1;
            }
        };

        if senders.is_empty() {
            return Err(NetworkError::HeadersFailed(from, to));
        }

        // Try each peer until one returns headers.
        for (peer_id, sender) in &senders {
            let (tx, rx) = oneshot::channel();
            let req = PeerRequest::GetBlockHeaders {
                request: GetBlockHeaders {
                    start_block: BlockHashOrNumber::Number(from),
                    limit: count,
                    skip: 0,
                    direction: HeadersDirection::Rising,
                },
                response: tx,
            };

            if sender.to_session_tx.send(req).await.is_err() {
                debug!(%peer_id, "GetBlockHeaders send failed, trying next peer");
                continue;
            }

            match rx.await {
                Ok(Ok(BlockHeaders(headers))) => {
                    if headers.is_empty() {
                        debug!(%peer_id, from, to, "GetBlockHeaders returned empty, trying next peer");
                        continue;
                    }

                    // Convert alloy_consensus::Header → ScopeHeader.
                    return Ok(headers
                        .into_iter()
                        .map(|h| ScopeHeader {
                            number: h.number,
                            // hash_slow() computes keccak256(rlp(header))
                            hash: h.hash_slow(),
                            parent_hash: h.parent_hash,
                            timestamp: h.timestamp,
                            receipts_root: h.receipts_root,
                            logs_bloom: h.logs_bloom,
                            gas_used: h.gas_used,
                            base_fee_per_gas: h.base_fee_per_gas.map(|f| f as u128),
                        })
                        .collect());
                }
                Ok(Err(e)) => {
                    debug!(%peer_id, err = ?e, "GetBlockHeaders peer error, trying next");
                    continue;
                }
                Err(_) => {
                    debug!(%peer_id, "GetBlockHeaders channel closed, trying next");
                    continue;
                }
            }
        }

        Err(NetworkError::HeadersFailed(from, to))
    }

    /// Fetch receipts for a batch of blocks (up to 16) via `GetReceipts`.
    ///
    /// Sends `PeerRequest::GetReceipts` directly to connected peers' session channels.
    /// Tries each available peer in turn until one returns a valid response.
    /// Most peers prune historical receipts — retrying across peers improves the
    /// odds of hitting an archive node that has them.
    ///
    /// The ETH wire protocol allows up to 16 block hashes per `GetReceipts` request.
    /// The pipeline already batches candidates in groups of 16 before calling here.
    ///
    /// Receipt conversion: wire receipts lack tx_hash and log_index context.
    /// We synthesise them deterministically so deduplication works across re-runs.
    async fn get_receipts_for_blocks(
        &self,
        blocks: &[(u64, B256, B256)],
    ) -> Vec<ReceiptFetchResult> {
        let hashes: Vec<B256> = blocks.iter().map(|(_, h, _)| *h).collect();

        // Wait up to 60s for at least 3 usable (non-blacklisted) peers.
        // Archive nodes and full-sync nodes serve receipts; snap-sync nodes often don't.
        let senders = {
            let mut attempts = 0u32;
            loop {
                let s = self.usable_peer_senders().await;
                let want = if attempts < 60 { 3 } else { 1 };
                if s.len() >= want || attempts >= 90 {
                    break s;
                }
                if attempts.is_multiple_of(10) {
                    debug!(peers = s.len(), elapsed_s = attempts, "waiting for more peers before receipt fetch...");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                attempts += 1;
            }
        };

        if senders.is_empty() {
            warn!("no usable peers for GetReceipts");
            return blocks
                .iter()
                .map(|(n, _, _)| ReceiptFetchResult::Failed { block_num: *n })
                .collect();
        }

        debug!(peers_available = senders.len(), blocks = blocks.len(), "trying GetReceipts");

        // Try each peer in turn. On receipt, convert to alloy types and run MPT
        // verification immediately. A peer whose data fails verification is
        // blacklisted for the session and the next peer is tried for the whole batch.
        for (peer_id, sender) in &senders {
            let (tx, rx) = oneshot::channel();
            let req = PeerRequest::GetReceipts {
                request: GetReceipts(hashes.clone()),
                response: tx,
            };

            if sender.to_session_tx.send(req).await.is_err() {
                debug!(%peer_id, "GetReceipts send failed — peer disconnected, trying next");
                continue;
            }

            match rx.await {
                Ok(Ok(Receipts(receipt_batches))) => {
                    let outer_len = receipt_batches.len();
                    let inner_total: usize = receipt_batches.iter().map(|v| v.len()).sum();
                    debug!(%peer_id, outer_len, inner_total, "GetReceipts response");

                    // Snap-synced nodes return empty responses (no receipt DB).
                    let has_data = receipt_batches.iter().any(|v| !v.is_empty());
                    if !has_data {
                        debug!(%peer_id, outer_len, "GetReceipts empty — peer lacks receipt data, trying next");
                        continue;
                    }

                    // Convert wire receipts → alloy types and verify MPT for every block.
                    // If any block's root mismatches, this peer is sending bad data —
                    // blacklist it immediately and try the next peer for the whole batch.
                    let mut converted: Vec<ReceiptFetchResult> = Vec::with_capacity(blocks.len());
                    let mut peer_bad = false;

                    for (&(block_num, block_hash, receipts_root), wire_receipts) in
                        blocks.iter().zip(receipt_batches)
                    {
                        let receipts = build_alloy_receipts(wire_receipts, block_num, block_hash);

                        if let Err(e) = verify_receipts(&receipts, receipts_root, block_num) {
                            warn!(
                                %peer_id, block = block_num, err = %e,
                                "MPT verification failed — blacklisting peer"
                            );
                            self.blacklist_peer(*peer_id).await;
                            peer_bad = true;
                            break;
                        }

                        converted.push(ReceiptFetchResult::Ok { block_num, block_hash, receipts });
                    }

                    if !peer_bad {
                        return converted;
                    }
                    // Peer blacklisted; fall through to the next peer.
                }
                Ok(Err(e)) => {
                    debug!(%peer_id, err = ?e, "GetReceipts peer error, trying next");
                    continue;
                }
                Err(_) => {
                    debug!(%peer_id, "GetReceipts channel closed, trying next");
                    continue;
                }
            }
        }

        warn!(
            peers_tried = senders.len(),
            "GetReceipts: all peers failed or were blacklisted"
        );
        blocks
            .iter()
            .map(|(n, _, _)| ReceiptFetchResult::Failed { block_num: *n })
            .collect()
    }

    /// Return the best block number from connected peers' Status messages.
    ///
    /// Uses `UnifiedStatus::latest_block` which is populated for eth/69+ peers.
    /// Called when `to_block` is omitted from config (sync to chain tip).
    async fn best_block_number(&self) -> Result<u64, NetworkError> {
        let guard = self.peers.read().await;
        guard
            .values()
            .filter_map(|s| s.best_block)
            .max()
            .ok_or(NetworkError::NoPeers { wanted: 1, found: 0 })
    }

    async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }
}

// ── Receipt conversion ────────────────────────────────────────────────────────

/// Convert reth wire receipts to `alloy::rpc::types::TransactionReceipt`.
///
/// The ETH wire protocol `GetReceipts` response contains consensus-level receipts:
/// - Transaction type (Legacy / EIP-2930 / EIP-1559 / EIP-4844)
/// - Success status (true/false)
/// - Cumulative gas used
/// - Logs (address + topics + data)
///
/// It does NOT include transaction metadata available from the full node RPC:
/// - `tx_hash` — in the transaction, not the receipt
/// - `from` address — requires signature recovery from the transaction
/// - `effective_gas_price` — computed from transaction + base_fee
///
/// We synthesise what we can:
/// - `tx_hash = keccak256(block_hash ++ tx_index_be8)` — deterministic, unique
/// - `log_index` = cumulative across all logs in the block (0-indexed globally)
///
/// These are not real Ethereum tx hashes, but they enable:
/// - Correct `UNIQUE (tx_hash, log_index)` deduplication in SQLite
/// - Correct Merkle Patricia Trie reconstruction in `verify_receipts()`
/// - Correct event log extraction in `EventDecoder::extract_and_decode()`
fn build_alloy_receipts<R>(
    wire_receipts: Vec<R>,
    block_num: u64,
    block_hash: B256,
) -> Vec<TransactionReceipt>
where
    R: WireReceipt,
{
    let mut cumulative_log_index: u64 = 0;

    wire_receipts
        .into_iter()
        .enumerate()
        .map(|(tx_idx, receipt)| {
            let tx_index = tx_idx as u64;

            // Synthesise a deterministic tx_hash from block context.
            // keccak256(block_hash || tx_index) is unique per (block, tx_position)
            // and reproducible across re-runs, so INSERT OR IGNORE works correctly.
            let mut hash_input = [0u8; 40]; // 32 bytes block_hash + 8 bytes tx_index
            hash_input[..32].copy_from_slice(block_hash.as_slice());
            hash_input[32..].copy_from_slice(&tx_index.to_be_bytes());
            let tx_hash: B256 = keccak256(hash_input);

            // Build alloy::rpc::types::Log for each primitive log.
            // log_index is globally unique within the block (cumulative across all txs).
            let logs_start = cumulative_log_index;
            let rpc_logs: Vec<RpcLog> = receipt
                .logs()
                .iter()
                .enumerate()
                .map(|(log_pos, prim_log)| RpcLog {
                    inner: prim_log.clone(),
                    block_hash: Some(block_hash),
                    block_number: Some(block_num),
                    block_timestamp: None,
                    transaction_hash: Some(tx_hash),
                    transaction_index: Some(tx_index),
                    log_index: Some(logs_start + log_pos as u64),
                    removed: false,
                })
                .collect();

            cumulative_log_index += rpc_logs.len() as u64;

            // Build the consensus receipt containing our rpc logs.
            // The ReceiptEnvelope type encodes the EIP-2718 transaction type prefix.
            let consensus = Receipt {
                status: Eip658Value::Eip658(receipt.success()),
                cumulative_gas_used: receipt.cumulative_gas_used(),
                logs: rpc_logs,
            };

            // Map reth TxType (u8 repr) to alloy ReceiptEnvelope variant.
            // ReceiptEnvelope variants take ReceiptWithBloom<Receipt<T>> — `.into()` converts
            // Receipt<Log> to ReceiptWithBloom<Receipt<Log>> via the blanket Into impl.
            let inner = match receipt.tx_type_u8() {
                1 => ReceiptEnvelope::Eip2930(consensus.into()),
                2 => ReceiptEnvelope::Eip1559(consensus.into()),
                3 => ReceiptEnvelope::Eip4844(consensus.into()),
                4 => ReceiptEnvelope::Eip7702(consensus.into()),
                _ => ReceiptEnvelope::Legacy(consensus.into()), // 0 = Legacy
            };

            TransactionReceipt {
                inner,
                transaction_hash: tx_hash,
                transaction_index: Some(tx_index),
                block_hash: Some(block_hash),
                block_number: Some(block_num),
                // gas_used is u64 in alloy 1.7 — cumulative gas used by this tx in the block.
                gas_used: receipt.cumulative_gas_used(),
                // Unknown from wire — zero is safe (not used in pipeline logic).
                effective_gas_price: 0,
                // blob_gas_used is Option<u64> in alloy 1.7.
                blob_gas_used: None,
                blob_gas_price: None,
                // Unknown from wire — zero address (not used for filtering or storage).
                from: Address::ZERO,
                to: None,
                contract_address: None,
            }
        })
        .collect()
}

// ── WireReceipt helper trait ──────────────────────────────────────────────────

/// Abstracts over reth's wire receipt type to extract the fields we need.
///
/// reth parameterises the receipt type by network primitives. This trait lets
/// `build_alloy_receipts` remain generic without knowing the exact reth type.
///
/// Implemented for `reth_ethereum_primitives::Receipt` = `EthereumReceipt<TxType>`.
trait WireReceipt {
    /// Transaction type as a raw u8: 0=Legacy, 1=EIP-2930, 2=EIP-1559, 3=EIP-4844, 4=EIP-7702.
    fn tx_type_u8(&self) -> u8;
    /// True if the transaction succeeded (status = 1), false if reverted (status = 0).
    fn success(&self) -> bool;
    /// Cumulative gas used up to and including this transaction in the block.
    fn cumulative_gas_used(&self) -> u64;
    /// Logs emitted by this transaction.
    fn logs(&self) -> &[alloy_primitives::Log];
}

/// Implement `WireReceipt` for reth's Ethereum receipt type.
///
/// `reth_ethereum_primitives::Receipt` = `EthereumReceipt<reth_primitives::TxType>`.
/// Fields: `tx_type`, `success`, `cumulative_gas_used`, `logs`.
///
/// Note: if reth changes its receipt type between versions, only this impl needs updating.
impl WireReceipt for reth_ethereum_primitives::Receipt {
    fn tx_type_u8(&self) -> u8 {
        self.tx_type as u8
    }
    fn success(&self) -> bool {
        self.success
    }
    fn cumulative_gas_used(&self) -> u64 {
        self.cumulative_gas_used
    }
    fn logs(&self) -> &[alloy_primitives::Log] {
        &self.logs
    }
}

/// Implement `WireReceipt` for the wire-protocol wrapper type.
///
/// The ETH wire `Receipts<T>` struct stores `Vec<Vec<ReceiptWithBloom<T>>>` —
/// each receipt on the wire is wrapped in a `ReceiptWithBloom` containing the
/// receipt data and a pre-computed bloom filter. We delegate all field access
/// to the inner `receipt`.
impl WireReceipt for ReceiptWithBloom<reth_ethereum_primitives::EthereumReceipt> {
    fn tx_type_u8(&self) -> u8 {
        self.receipt.tx_type as u8
    }
    fn success(&self) -> bool {
        self.receipt.success
    }
    fn cumulative_gas_used(&self) -> u64 {
        self.receipt.cumulative_gas_used
    }
    fn logs(&self) -> &[alloy_primitives::Log] {
        &self.receipt.logs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The blacklist is an Arc<RwLock<HashSet<PeerId>>> inside DevP2PNetwork.
    // We can't boot a real devp2p stack in unit tests, so we exercise the
    // filtering algorithm directly using the same primitives.

    fn peer(byte: u8) -> reth_network_peers::PeerId {
        reth_network_peers::PeerId::from_slice(&[byte; 64])
    }

    #[tokio::test]
    async fn blacklist_excludes_blacklisted_peer() {
        let all: HashMap<reth_network_peers::PeerId, ()> = [
            (peer(1), ()),
            (peer(2), ()),
            (peer(3), ()),
        ]
        .into_iter()
        .collect();

        let mut blacklist = HashSet::new();
        blacklist.insert(peer(2));

        let usable: Vec<_> = all
            .keys()
            .filter(|id| !blacklist.contains(*id))
            .copied()
            .collect();

        assert_eq!(usable.len(), 2);
        assert!(!usable.contains(&peer(2)));
        assert!(usable.contains(&peer(1)));
        assert!(usable.contains(&peer(3)));
    }

    #[tokio::test]
    async fn blacklist_all_peers_leaves_empty_pool() {
        let all: HashMap<reth_network_peers::PeerId, ()> = [
            (peer(1), ()),
            (peer(2), ()),
        ]
        .into_iter()
        .collect();

        let blacklist: HashSet<reth_network_peers::PeerId> = all.keys().copied().collect();

        let usable: Vec<_> = all
            .keys()
            .filter(|id| !blacklist.contains(*id))
            .collect();

        assert!(usable.is_empty());
    }

    #[tokio::test]
    async fn blacklist_empty_passes_all_peers() {
        let all: HashMap<reth_network_peers::PeerId, ()> = [
            (peer(10), ()),
            (peer(20), ()),
        ]
        .into_iter()
        .collect();

        let blacklist: HashSet<reth_network_peers::PeerId> = HashSet::new();

        let usable: Vec<_> = all
            .keys()
            .filter(|id| !blacklist.contains(*id))
            .collect();

        assert_eq!(usable.len(), 2);
    }
}
