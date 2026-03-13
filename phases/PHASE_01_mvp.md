# Phase 1 — Working MVP (devp2p, no providers)

## Goal

Build a fully working, trustless end-to-end pipeline. scopenode connects
directly to the Ethereum P2P network, fetches headers and receipts from peers,
verifies them cryptographically with Merkle Patricia Tries, and serves the
results at `localhost:8545`.

**No RPC provider. No API key. No trusted third party. Ever.**

Phase 1 is simpler than Phase 2 in degree, not in kind:
- No multi-peer header agreement (Merkle verification catches tampered receipts)
- No ERA1 archive fallback (devp2p only)
- Historical sync only (no live sync — Phase 3a)
- No proxy detection (use `abi_override` if needed)
- No Helios beacon bootstrap (sync headers from devp2p from `from_block`)

**What "done" looks like:**

```bash
scopenode sync config.toml
# Connects to Ethereum mainnet P2P peers via devp2p.
# Fetches headers and receipts directly from peers — no RPC.
# Verifies every receipt with Merkle Patricia Trie.
# Stores verified events in SQLite.

# In another terminal:
cast logs --rpc-url http://localhost:8545 \
  --address 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8 \
  --from-block 17000000 --to-block 17000100 \
  "Swap(address,address,int256,int256,uint160,uint128,int24)"
# Returns verified events from local SQLite instantly.
```

---

## The core trust model

```
                                     ┌─────────────────────────────────┐
                                     │  What we trust (hardcoded)      │
                                     │  Genesis hash: 0xd4e5...        │
                                     │  (universally agreed-upon fact) │
                                     └──────────────┬──────────────────┘
                                                    │
                    ┌───────────────────────────────▼───────────────────────────────┐
                    │                      devp2p peer N                            │
                    │   GetBlockHeaders ──▶ [header_A, header_B, header_C, ...]    │
                    │                       each header has:                        │
                    │                         parent_hash → chains to genesis       │
                    │                         receipts_root → commits to receipts   │
                    │                         logs_bloom → bloom filter             │
                    └───────────────────────────────────────────────────────────────┘
                                                    │
                    ┌───────────────────────────────▼───────────────────────────────┐
                    │                   Bloom filter scan (local, CPU)              │
                    │   logs_bloom.contains(contract_address + topic0)?             │
                    │   YES → candidate block                                        │
                    │   NO  → skip (no network call needed)                         │
                    └───────────────────────────────────────────────────────────────┘
                                                    │
                    ┌───────────────────────────────▼───────────────────────────────┐
                    │                      devp2p peer N                            │
                    │   GetReceipts(block_hashes) ──▶ [receipt_0, receipt_1, ...]  │
                    └───────────────────────────────────────────────────────────────┘
                                                    │
                    ┌───────────────────────────────▼───────────────────────────────┐
                    │              Merkle Patricia Trie verification                 │
                    │   build_trie(receipts).root == header.receipts_root?          │
                    │   MATCH  → receipts are authentic, decode events              │
                    │   MISMATCH → peer sent tampered data, mark_retry, skip        │
                    └───────────────────────────────────────────────────────────────┘
                                                    │
                                              store in SQLite
                                              serve via JSON-RPC
```

A peer cannot fake events. To fool us, it would need to:
1. Construct a fake receipt that hashes into a valid Merkle tree
2. That Merkle tree root matches the `receipts_root` in the header
3. Which means knowing the preimage of a keccak256 hash → computationally infeasible

---

## The pipeline (five stages)

```
Config
  │
  ▼
┌─────────────────────────────────────────────────────────────┐
│ Stage 1: ABI Fetch                                          │
│   Sourcify → EventAbi list → topic0 hashes + bloom targets │
│   Cached in SQLite after first fetch                        │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ Stage 2: Header Sync (devp2p)                               │
│   GetBlockHeaders(from, count=64) from peer                 │
│   Repeat in batches until `to_block`                        │
│   Store: number, hash, parent_hash, receipts_root,          │
│          logs_bloom(256B), timestamp, gas_used, base_fee    │
│   Resumable via sync_cursor.headers_done_to                 │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ Stage 3: Bloom Scan (local CPU, no network)                 │
│   For each stored header in range:                          │
│     logs_bloom.contains_input(contract_addr)?               │
│     AND logs_bloom.contains_input(topic0)?                  │
│     YES → bloom_candidates table                            │
│   Skips ~85–90% of blocks. Zero false negatives.           │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ Stage 4: Receipt Fetch + Merkle Verify (devp2p)             │
│   GetReceipts(batch of 16 block hashes) from peer          │
│   For each block:                                           │
│     trie = build_receipt_trie(receipts)                     │
│     trie.root == header.receipts_root? → verified           │
│     mismatch → mark_retry, try different peer next run      │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ Stage 5: Decode + Store                                     │
│   For each verified receipt → for each log:                 │
│     log.address == contract AND log.topics[0] == topic0?    │
│     → ABI decode (alloy-dyn-abi)                            │
│     → INSERT OR IGNORE INTO events                          │
│   source = 'devp2p'                                         │
└──────────────────────────────────────────────────────────────┘
```

---

## Stack decisions

| Concern | Choice | Why |
|---|---|---|
| Ethereum types | `alloy` 1.x | Standard across reth/lighthouse ecosystem |
| P2P networking | `reth-discv4` + `reth-network` + `reth-eth-wire` | Production-grade devp2p stack |
| ABI source | Sourcify (no API key) | EF-run, open, reliable |
| ABI decoding | `alloy-dyn-abi` | Runtime decoding for arbitrary ABIs |
| Merkle verification | `alloy-trie` | Same as reth — well-tested |
| Storage | `sqlx` + SQLite (WAL mode) | Zero setup, excellent for local workloads |
| JSON-RPC server | `jsonrpsee` | Standard in Rust Ethereum ecosystem |
| Async | `tokio` | The standard |
| CLI | `clap` v4 (derive) | Clean, ergonomic |
| Progress | `indicatif` | Best Rust progress bar library |
| HTTP client | `reqwest` (rustls) | For Sourcify ABI fetching |
| Error handling | `thiserror` (libs) + `anyhow` (binary) | Standard pattern |
| Logging | `tracing` + `tracing-subscriber` | Async-aware, structured |
| Config | `serde` + `toml` | Standard |

---

## Workspace Cargo.toml

```toml
[workspace]
members = [
    "crates/scopenode",
    "crates/scopenode-core",
    "crates/scopenode-storage",
    "crates/scopenode-rpc",
]
resolver = "2"

[workspace.dependencies]
# Ethereum types — use 1.x throughout
alloy             = { version = "1.7",  features = ["full"] }
alloy-primitives  = "1.5"
alloy-dyn-abi     = "1.5"
alloy-trie        = "0.9"

# devp2p — pin to reth 1.x compatible versions
# NOTE: reth crates evolve rapidly. Check crates.io for latest 1.x.
reth-discv4       = { version = "1",  default-features = false }
reth-network      = { version = "1",  default-features = false }
reth-eth-wire     = { version = "1",  default-features = false }
reth-network-api  = { version = "1",  default-features = false }
secp256k1         = { version = "0.29", features = ["rand", "global-context"] }

# Async + concurrency
tokio             = { version = "1", features = ["full"] }
futures           = "0.3"

# Storage
sqlx              = { version = "0.8", features = ["sqlite", "runtime-tokio", "migrate"] }

# JSON-RPC server
jsonrpsee         = { version = "0.26", features = ["server", "macros"] }

# HTTP (Sourcify ABI fetch only)
reqwest           = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }

# Error handling
thiserror         = "2"
anyhow            = "1"

# Serialization
serde             = { version = "1", features = ["derive"] }
serde_json        = "1"
toml              = "0.8"

# CLI + logging + misc
clap              = { version = "4", features = ["derive", "env"] }
indicatif         = "0.17"
tracing           = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
async-trait       = "0.1"
dirs              = "5"
url               = { version = "2", features = ["serde"] }
rand              = "0.8"

# Internal crates
scopenode-core    = { path = "crates/scopenode-core" }
scopenode-storage = { path = "crates/scopenode-storage" }
scopenode-rpc     = { path = "crates/scopenode-rpc" }
```

---

## Project structure

```
scopenode/
├── Cargo.toml              # workspace
├── Cargo.lock
├── VISION.md
├── phases/
├── config.example.toml
├── config.test.toml        # gitignored — small range for dev
└── crates/
    ├── scopenode/          # binary
    │   └── src/
    │       ├── main.rs
    │       ├── cli.rs
    │       └── commands/
    │           ├── sync.rs
    │           ├── status.rs
    │           └── query.rs
    ├── scopenode-core/     # pipeline logic
    │   └── src/
    │       ├── lib.rs
    │       ├── config.rs
    │       ├── error.rs
    │       ├── pipeline.rs     # orchestrates all stages
    │       ├── network.rs      # EthNetwork trait + DevP2PNetwork impl
    │       ├── headers.rs      # BloomScanner
    │       ├── receipts.rs     # verify_receipts() using alloy-trie
    │       ├── abi.rs          # Sourcify + EventDecoder
    │       └── types.rs
    ├── scopenode-storage/  # SQLite
    │   └── src/
    │       ├── lib.rs
    │       ├── db.rs
    │       ├── models.rs
    │       ├── types.rs
    │       └── migrations/
    │           └── 001_init.sql
    └── scopenode-rpc/      # JSON-RPC
        └── src/
            ├── lib.rs
            └── server.rs
```

---

## Concepts to deeply understand

### The Ethereum devp2p stack

```
┌─────────────────────────────────────────────────────────────────────┐
│                         devp2p network stack                         │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  Discovery layer (UDP)  — reth-discv4                       │   │
│  │  Kademlia DHT: find peers, exchange ENR records             │   │
│  │  Bootstrap from hardcoded mainnet bootnodes                 │   │
│  │  Peers: IP + port + secp256k1 public key                    │   │
│  └──────────────────────────────┬──────────────────────────────┘   │
│                                 │ found peer, try connecting         │
│  ┌──────────────────────────────▼──────────────────────────────┐   │
│  │  RLPx transport (TCP)  — reth-network                       │   │
│  │  ECIES handshake: derive session keys (secp256k1 + AES)     │   │
│  │  Encrypted, authenticated TCP frames                         │   │
│  └──────────────────────────────┬──────────────────────────────┘   │
│                                 │ connection established             │
│  ┌──────────────────────────────▼──────────────────────────────┐   │
│  │  ETH sub-protocol  — reth-eth-wire                          │   │
│  │  Status handshake: networkId, genesis hash, best block      │   │
│  │  GetBlockHeaders / BlockHeaders                              │   │
│  │  GetReceipts / Receipts                                      │   │
│  └─────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

**discv4 (UDP):** Kademlia-based DHT. Peers sign "ENR records" (Ethereum Node
Records) containing IP, port, public key. We bootstrap from ~10 well-known
mainnet bootnodes that reth hardcodes and discover hundreds of peers from there.

**RLPx (TCP):** ECIES handshake — both sides generate ephemeral secp256k1 keys,
derive shared secret, encrypt all frames with AES-256-CTR + HMAC-SHA256.
`reth-network` handles this entirely — we never implement crypto ourselves.

**ETH sub-protocol:** After RLPx connects, both sides exchange a `Status` message
(chain ID, genesis hash, best block hash + difficulty). If they match, the
session is open and we can exchange data messages.

**The messages we use:**
- `GetBlockHeaders(start: BlockHashOrNumber, limit: u64, skip: u64, reverse: bool)`
  → `BlockHeaders(Vec<Header>)` — fetch headers sequentially, 64 at a time
- `GetReceipts(Vec<B256>)` → `Receipts(Vec<Vec<Receipt>>)` — batch of block hashes,
  returns all receipts for each block in one response

### What a block header contains (and why we need it)

```
Header {
    parent_hash:      B256,    // hash of previous block — chains to genesis
    ommers_hash:      B256,    // uncle blocks (not used by us)
    beneficiary:      Address, // miner/validator
    state_root:       B256,    // world state trie (not used by us)
    transactions_root:B256,    // transaction trie (not used by us)
    receipts_root:    B256,    // ← KEY: commits to all receipts in this block
    logs_bloom:       Bloom,   // ← KEY: 256-byte bloom filter of all logs
    difficulty:       U256,    // PoW era, 0 post-Merge
    number:           u64,     // block number
    gas_limit:        u64,
    gas_used:         u64,
    timestamp:        u64,
    extra_data:       Bytes,
    mix_hash:         B256,
    nonce:            u64,
    base_fee_per_gas: Option<u128>, // EIP-1559, post-London
    // ... withdrawals_root etc post-Shanghai (we store in extra_data, not used)
}
```

We store only the fields we need: `number, hash, parent_hash, timestamp,
receipts_root, logs_bloom, gas_used, base_fee`.

### Bloom filters (how 85–90% of blocks get skipped for free)

A Bloom filter is a 2048-bit array. To add an item:
1. `keccak256(item)` → 32 bytes
2. Take pairs `[0,1]`, `[2,3]`, `[4,5]` → each `mod 2048` → 3 bit positions
3. Set those 3 bits to 1

To check membership: compute same 3 positions. All 3 set → might be present.
Any 0 → definitely not present. Zero false negatives.

Ethereum's `logs_bloom` runs this for every log's contract address AND every
topic. So to ask "did 0x8ad...  emit a Swap in block 17000042?", we check if
both the address bits and the Swap topic0 bits are set. If either is 0 → skip.

```rust
// alloy_primitives::Bloom wraps [u8; 256]
use alloy_primitives::{Bloom, BloomInput};

fn bloom_contains_event(bloom: &Bloom, contract: Address, topic0: B256) -> bool {
    bloom.contains_input(BloomInput::Raw(contract.as_slice()))
        && bloom.contains_input(BloomInput::Raw(topic0.as_slice()))
}
```

### Merkle Patricia Trie — the trustless verification primitive

The `receipts_root` in a header is the root hash of a Merkle Patricia Trie:
- Keys: RLP-encoded transaction indices (0, 1, 2, ...)
- Values: RLP-encoded transaction receipts

To verify: reconstruct the trie from the receipts a peer sent, compare the
computed root to `receipts_root` from the header. If they match, the receipts
are authentic — a peer cannot forge this without solving keccak256 preimages.

```
receipts from peer:
  [receipt_0, receipt_1, receipt_2, ...]
         │
         ▼
  trie = alloy_trie::HashBuilder
  trie.add(rlp_encode(0), rlp_encode(receipt_0))
  trie.add(rlp_encode(1), rlp_encode(receipt_1))
  ...   ← must be in key order (ascending index)
         │
         ▼
  computed_root = trie.root()
         │
         ▼
  computed_root == header.receipts_root? → VERIFIED
  computed_root != header.receipts_root? → TAMPERED, reject
```

`alloy-trie::HashBuilder` builds this correctly. `reth-primitives` has
`calculate_receipt_root` which wraps `HashBuilder` for receipts specifically.

---

## The `EthNetwork` trait (keep this, just swap the implementation)

```rust
// crates/scopenode-core/src/network.rs

use crate::error::NetworkError;
use crate::types::ScopeHeader;
use alloy_primitives::B256;
use async_trait::async_trait;

/// Result of fetching receipts for a single block.
pub enum ReceiptFetchResult {
    Ok {
        block_num: u64,
        block_hash: B256,
        receipts: Vec<alloy::rpc::types::TransactionReceipt>,
    },
    Failed { block_num: u64 },
}

/// Transport abstraction. Phase 1: DevP2PNetwork. Phase 2: adds multi-peer
/// agreement and ERA1 fallback behind the same trait.
///
/// The pipeline is generic over N: EthNetwork — no runtime dispatch overhead.
#[async_trait]
pub trait EthNetwork: Send + Sync {
    /// Fetch block headers for the inclusive range [from, to].
    /// May return fewer headers than requested if some peers don't have them.
    async fn get_headers(&self, from: u64, to: u64) -> Result<Vec<ScopeHeader>, NetworkError>;

    /// Fetch receipts for a batch of blocks (up to 16 per call — ETH wire limit).
    /// Each element: (block_num, block_hash, receipts_root).
    /// Merkle verification happens in the pipeline, not here.
    async fn get_receipts_for_blocks(
        &self,
        blocks: &[(u64, B256, B256)],
    ) -> Vec<ReceiptFetchResult>;

    /// Best block number — from peers' Status message during handshake.
    /// Used when to_block is omitted from config.
    async fn best_block_number(&self) -> Result<u64, NetworkError>;
}
```

---

## `DevP2PNetwork` — replacing `RpcNetwork` entirely

```
//! crates/scopenode-core/src/network.rs
//!
//! devp2p stack boot sequence:
//!
//!   1. Generate ephemeral secp256k1 node key (new each run — we don't need
//!      a persistent identity since we're a light consumer, not a server)
//!
//!   2. Build NetworkConfig:
//!        - mainnet chain spec
//!        - discv4 with mainnet bootnodes
//!        - listen address (pick a random port, or 0 for OS-assigned)
//!        - no block body/state serving (we're consumer-only)
//!
//!   3. Build + spawn NetworkManager as a tokio task.
//!      NetworkManager runs the discv4 discovery + RLPx peer management loop.
//!
//!   4. Get NetworkHandle from the manager — this is our control interface.
//!
//!   5. Wait for initial peer connections (discv4 takes ~5s to find peers).
//!      Target: 5+ connected peers before starting sync.
//!
//!   6. Use NetworkHandle / peer sessions to send GetBlockHeaders + GetReceipts.
```

```rust
use reth_discv4::{Discv4Config, DEFAULT_DISCOVERY_PORT};
use reth_network::{
    config::NetworkConfig,
    NetworkHandle, NetworkManager,
};
use reth_network_api::NetworkInfo;
use reth_eth_wire::capability::Capability;
use secp256k1::{SecretKey, SECP256K1};
use rand::thread_rng;

/// devp2p-backed implementation of EthNetwork.
///
/// Boots a full reth-network stack on construction. All data comes from
/// Ethereum mainnet peers — no RPC provider, no trusted third party.
///
/// ┌──────────────────────────────────────────────────────────────┐
/// │  DevP2PNetwork                                               │
/// │    handle: NetworkHandle  (send requests, get peer info)     │
/// │    peers connected: ~10 (maintained by NetworkManager task)  │
/// └──────────────────────────────────────────────────────────────┘
pub struct DevP2PNetwork {
    handle: NetworkHandle,
}

impl DevP2PNetwork {
    /// Boot the devp2p stack and wait for initial peer connections.
    ///
    /// This is the only async constructor — call once at startup.
    /// The NetworkManager tokio task keeps running for the process lifetime.
    pub async fn start() -> Result<Self, NetworkError> {
        // Generate a fresh node key each run. We don't need a persistent
        // identity — scopenode is a consumer, not a server.
        let secret_key = SecretKey::new(&mut thread_rng());

        // Configure discv4 with Ethereum mainnet bootnodes.
        // These are stable, well-known nodes maintained by EF and client teams.
        let discv4_config = Discv4Config::builder()
            .external_ip_resolver(None)  // let discv4 figure out our IP
            .build();

        // Build network config for Ethereum mainnet.
        // We use reth's mainnet chain spec (genesis hash, bootnodes, etc.)
        let network_config = NetworkConfig::builder(secret_key)
            .mainnet_boot_nodes()   // EF-maintained mainnet bootnodes
            .discovery(discv4_config)
            .build(/* mainnet chain spec */);

        // Spawn the NetworkManager. This task runs for the process lifetime:
        //   - discv4 UDP discovery loop
        //   - RLPx TCP connection management
        //   - ETH sub-protocol session management
        let manager = NetworkManager::new(network_config)
            .await
            .map_err(|e| NetworkError::Boot(e.to_string()))?;

        let handle = manager.handle().clone();

        tokio::spawn(manager);

        // Wait for initial peer connections. discv4 takes ~5s to find peers.
        // We need at least 3 before we can usefully fetch data.
        Self::wait_for_peers(&handle, 3, std::time::Duration::from_secs(30)).await?;

        tracing::info!(
            peers = handle.num_connected_peers(),
            "devp2p ready"
        );

        Ok(Self { handle })
    }

    /// Poll until we have at least `min_peers` connected, or timeout.
    async fn wait_for_peers(
        handle: &NetworkHandle,
        min_peers: usize,
        timeout: std::time::Duration,
    ) -> Result<(), NetworkError> {
        let start = std::time::Instant::now();
        loop {
            let n = handle.num_connected_peers();
            if n >= min_peers {
                return Ok(());
            }
            if start.elapsed() > timeout {
                return Err(NetworkError::NoPeers {
                    wanted: min_peers,
                    found: n,
                });
            }
            tracing::debug!(peers = n, "Waiting for devp2p peers...");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

// NOTE ON reth-network REQUEST API:
//
// reth-network manages peer sessions internally. To send GetBlockHeaders or
// GetReceipts, use the fetch client exposed through the NetworkHandle:
//
//   let fetch = handle.fetch_client().await?;
//   fetch.get_block_headers(start, limit, skip, reverse).await?
//   fetch.get_receipts(block_hashes).await?
//
// The fetch client handles:
//   - Selecting a suitable peer for the request
//   - Retry on peer failure (tries another peer)
//   - Timeout handling
//
// Verify exact method names against reth-network-api source at implementation
// time — the API evolves with each reth release. Pin to a specific version.

#[async_trait]
impl EthNetwork for DevP2PNetwork {
    async fn get_headers(&self, from: u64, to: u64) -> Result<Vec<ScopeHeader>, NetworkError> {
        let count = (to - from + 1) as u64;

        // GetBlockHeaders: fetch `count` headers starting at block `from`,
        // no skip, ascending order.
        // Uses the reth-network fetch client which manages peer selection.
        let fetch = self.handle.fetch_client().await
            .map_err(|e| NetworkError::Rpc(e.to_string()))?;

        let headers = fetch
            .get_block_headers(
                from.into(),  // start: BlockHashOrNumber
                count,        // limit (max 64 per ETH wire spec)
                0,            // skip
                false,        // reverse = false → ascending
            )
            .await
            .map_err(|e| NetworkError::HeadersFailed(from, to))?;

        if headers.is_empty() {
            return Err(NetworkError::HeadersFailed(from, to));
        }

        Ok(headers
            .into_iter()
            .map(|h| ScopeHeader {
                number:            h.number,
                hash:              h.hash_slow(), // compute hash from header fields
                parent_hash:       h.parent_hash,
                timestamp:         h.timestamp,
                receipts_root:     h.receipts_root,
                logs_bloom:        h.logs_bloom,
                gas_used:          h.gas_used,
                base_fee_per_gas:  h.base_fee_per_gas.map(|f| f as u128),
            })
            .collect())
    }

    async fn get_receipts_for_blocks(
        &self,
        blocks: &[(u64, B256, B256)],
    ) -> Vec<ReceiptFetchResult> {
        let hashes: Vec<B256> = blocks.iter().map(|(_, hash, _)| *hash).collect();

        let fetch = match self.handle.fetch_client().await {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(err = %e, "Failed to get fetch client");
                return blocks.iter()
                    .map(|(n, _, _)| ReceiptFetchResult::Failed { block_num: *n })
                    .collect();
            }
        };

        // GetReceipts: batch of block hashes → batch of receipt lists.
        // ETH wire protocol: up to 16 block hashes per request (caller's job
        // to chunk — pipeline already does this in batches of 16).
        match fetch.get_receipts(hashes).await {
            Ok(receipt_batches) => {
                // Response is Vec<Vec<Receipt>>, one inner Vec per block.
                // Zip with our blocks slice to pair (num, hash, root) with receipts.
                blocks
                    .iter()
                    .zip(receipt_batches.into_iter())
                    .map(|(&(block_num, block_hash, _receipts_root), receipts)| {
                        // Convert reth receipt types to alloy receipt types.
                        // (exact conversion depends on reth version — may need
                        //  a conversion helper function)
                        ReceiptFetchResult::Ok {
                            block_num,
                            block_hash,
                            receipts: convert_receipts(receipts),
                        }
                    })
                    .collect()
            }
            Err(e) => {
                tracing::warn!(err = %e, "GetReceipts request failed for batch");
                blocks
                    .iter()
                    .map(|(n, _, _)| ReceiptFetchResult::Failed { block_num: *n })
                    .collect()
            }
        }
    }

    async fn best_block_number(&self) -> Result<u64, NetworkError> {
        // The network tracks best block from peers' Status messages.
        // NetworkHandle exposes this via network_info() or similar.
        // Exact method: verify against reth-network-api at implementation time.
        let best = self.handle.best_block_number();
        Ok(best)
    }
}

/// Convert reth receipt type to alloy TransactionReceipt.
///
/// reth and alloy use similar but not identical receipt types.
/// This function bridges the gap. Exact field mapping depends on
/// the reth version — verify at implementation time.
fn convert_receipts(
    reth_receipts: Vec<reth_primitives::TransactionReceipt>,
) -> Vec<alloy::rpc::types::TransactionReceipt> {
    // Implementation: map each field from reth_primitives::Receipt
    // to alloy::rpc::types::TransactionReceipt.
    // Key fields: status, cumulative_gas_used, logs (address, topics, data),
    // bloom, tx_hash, tx_index, block_hash, block_number.
    todo!("implement receipt type conversion — verify field names at implementation time")
}
```

**Implementation note:** The exact method names on `FetchClient` (`get_block_headers`,
`get_receipts`) should be verified against `reth-network-api` source at
implementation time. The above reflects the intended API shape. Pin reth crates
to a specific version and read the source. The abstraction (`EthNetwork` trait)
ensures changes here don't ripple into the pipeline.

---

## Staging environment

Before writing pipeline logic, wire in the staging environment so every
feature can be tested safely.

```
~/.scopenode/          ← production (never touch during development)
~/.scopenode-staging/  ← staging (blow away freely)
```

```bash
# Staging — explicit flag
scopenode sync config.test.toml --data-dir ~/.scopenode-staging

# Staging — env var (useful in scripts and CI)
SCOPENODE_DATA_DIR=~/.scopenode-staging scopenode sync config.test.toml
```

**`config.test.toml`** (gitignored — keep in repo root):

```toml
[node]
port = 8545
data_dir = "~/.scopenode-staging"

[[contracts]]
name = "Uniswap V3 ETH/USDC (test)"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events  = ["Swap", "Mint", "Burn"]
from_block = 17000000
to_block   = 17000100   # 100 blocks — completes in ~1 minute via devp2p
```

**Snapshot workflow:**

```bash
cp ~/.scopenode-staging/scopenode.db ~/.scopenode-staging/scopenode.db.snap  # save
cp ~/.scopenode-staging/scopenode.db.snap ~/.scopenode-staging/scopenode.db  # restore
rm ~/.scopenode-staging/scopenode.db                                          # clean slate
```

**.gitignore additions:**

```
config.test.toml
*.db
*.db.snap
.env
```

---

## SQLite schema

```sql
-- migrations/001_init.sql

CREATE TABLE IF NOT EXISTS headers (
    number          INTEGER PRIMARY KEY,
    hash            TEXT    NOT NULL UNIQUE,
    parent_hash     TEXT    NOT NULL,
    timestamp       INTEGER NOT NULL,
    receipts_root   TEXT    NOT NULL,
    logs_bloom      BLOB    NOT NULL,   -- 256 bytes
    gas_used        INTEGER NOT NULL,
    base_fee        INTEGER             -- NULL before EIP-1559 (London fork, block 12965000)
);

-- Indexed by (contract, fetched) for fast "what's left to fetch?" queries.
CREATE TABLE IF NOT EXISTS bloom_candidates (
    block_number    INTEGER NOT NULL,
    block_hash      TEXT    NOT NULL,
    contract        TEXT    NOT NULL,
    fetched         INTEGER NOT NULL DEFAULT 0,    -- 0: pending, 1: done
    pending_retry   INTEGER NOT NULL DEFAULT 0,    -- 1: Merkle mismatch or peer failure
    PRIMARY KEY (block_number, contract)
);
CREATE INDEX IF NOT EXISTS idx_bloom_pending
    ON bloom_candidates(contract, fetched) WHERE fetched = 0;

CREATE TABLE IF NOT EXISTS events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    contract        TEXT    NOT NULL,
    event_name      TEXT    NOT NULL,
    topic0          TEXT    NOT NULL,
    block_number    INTEGER NOT NULL,
    block_hash      TEXT    NOT NULL,
    tx_hash         TEXT    NOT NULL,
    tx_index        INTEGER NOT NULL,
    log_index       INTEGER NOT NULL,
    raw_topics      TEXT    NOT NULL,   -- JSON array: ["0xddf252...", "0x0000..."]
    raw_data        TEXT    NOT NULL,   -- hex-encoded bytes (no 0x prefix)
    decoded         TEXT    NOT NULL,   -- JSON: {"from":"0x...","to":"0x...","value":"1000"}
    source          TEXT    NOT NULL,   -- 'devp2p' | 'era1' (Phase 2) | 'rpc' (Phase 2 fallback)
    reorged         INTEGER NOT NULL DEFAULT 0,   -- Phase 3a: soft-delete on reorg
    UNIQUE (tx_hash, log_index)
);
-- Optimized for eth_getLogs: contract + topic0 + block range
CREATE INDEX IF NOT EXISTS idx_events_lookup
    ON events(contract, topic0, block_number) WHERE reorged = 0;

-- One row per contract, tracks progress of each pipeline stage.
CREATE TABLE IF NOT EXISTS sync_cursor (
    contract            TEXT    PRIMARY KEY,
    from_block          INTEGER NOT NULL,
    to_block            INTEGER,            -- NULL if syncing to chain tip
    headers_done_to     INTEGER,            -- last header successfully stored
    bloom_done_to       INTEGER,            -- last block bloom-scanned
    receipts_done_to    INTEGER             -- last receipt batch completed
);

-- ABI cache + contract registry.
CREATE TABLE IF NOT EXISTS contracts (
    address         TEXT    PRIMARY KEY,
    name            TEXT,
    abi_json        TEXT,                   -- JSON array of EventAbi objects
    impl_address    TEXT                    -- Phase 2: non-null if EIP-1967 proxy
);
```

---

## JSON-RPC server

```rust
// crates/scopenode-rpc/src/server.rs
//
// Only serves contracts that have been indexed (present in `contracts` table).
// Requests for unindexed contracts get a clear, actionable error.
//
// eth_getLogs request flow:
//
//  client ──▶ eth_getLogs(filter) ──▶ is_contract_indexed?
//                                           │ NO → -32000 "Contract not indexed"
//                                           │ YES
//                                           ▼
//                                     query_events_for_filter()
//                                           │
//                                           ▼
//                                     row_to_log() × N
//                                           │
//                                           ▼
//                                     Vec<Log> ──▶ client

#[rpc(server, namespace = "eth")]
pub trait EthApi {
    #[method(name = "getLogs")]
    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>>;

    #[method(name = "blockNumber")]
    async fn block_number(&self) -> RpcResult<String>;

    #[method(name = "chainId")]
    async fn chain_id(&self) -> RpcResult<String>;
}
```

The server binds to `127.0.0.1:8545` by default. A `--bind <addr>` flag is
available for users who want LAN sharing. Never bind to `0.0.0.0` by default
— scopenode has no authentication.

---

## Progress display

```
scopenode — Uniswap V3 ETH/USDC (0x8ad599c3...)

Stage 1/3  Header sync      ████████████████████  100 / 100     done
Stage 2/3  Bloom scan       ████████████████████  100 / 100     done (8 hits)
Stage 3/3  Receipt fetch    ████████░░░░░░░░░░░░    4 / 8       fetching...

devp2p peers: 6 active  |  Verified: 4 blocks  |  Events found: 3 Swap  1 Mint
```

Fields tracked in real-time: active peer count (from `handle.num_connected_peers()`),
verified block count, events found by type.

---

## Dry run output

```
Dry run complete for Uniswap V3 ETH/USDC (0x8ad599c3...)
  Block range:     17000000 → 17000100 (101 blocks)
  devp2p peers:    6 connected
  Bloom matches:   8 blocks (7.9% hit rate)
  Estimated time:  < 2 minutes (depends on peer response time)
```

---

## Error messages (user-facing)

Errors must be human-readable. No Rust panic dumps, no hex dumps.

| Situation | Message |
|---|---|
| No peers found in 30s | `Error: devp2p found 0 peers after 30s. Check internet connection and firewall (UDP port open?).` |
| Contract not on Sourcify | `Error: ABI not found for 0x8ad... on Sourcify. Add 'abi_override = "./myabi.json"' to config.` |
| Merkle mismatch | `Warning: Peer sent tampered receipts for block 17000042. Marked for retry. Run 'scopenode retry' to re-attempt.` |
| All peers failed for block | `Warning: All peers failed to return receipts for block 17000055. Marked for retry.` |
| DB schema newer than binary | `Error: Database schema version 3 is newer than this binary (expects ≤2). Upgrade scopenode or delete ~/.scopenode/scopenode.db.` |

---

## Tests to write

### Unit tests (no network, no DB)

```
bloom/
  - bloom_contains_event() → true when address + topic0 set
  - bloom_contains_event() → false when address not set
  - bloom_contains_event() → false when topic0 not set
  - bloom_contains_event() → false on zero bloom

receipts/
  - verify_receipts() → Ok on known block (fixture data)
  - verify_receipts() → Err on tampered receipt (flip one byte in data)
  - verify_receipts() → Ok on empty block (no transactions, empty MPT root)

abi/
  - EventAbi::signature() → correct canonical form for Transfer, Swap
  - EventAbi::topic0() → correct keccak256 for known events
  - SourcifyClient parses ABI from fixture metadata.json correctly
  - SourcifyClient returns NotOnSourcify error (mocked HTTP 404)
  - EventDecoder decodes indexed address param correctly
  - EventDecoder decodes non-indexed uint256 as decimal string
  - EventDecoder stores raw + decoded when abi-dyn-abi fails (decode error)

config/
  - Config::from_file() → parses valid TOML
  - Config::from_file() → rejects unknown fields (deny_unknown_fields)
  - Config::validate() → catches to_block < from_block
  - Config::validate() → catches empty events list
```

### Fixture-based integration tests (no live network)

Use serialized real Ethereum data from blocks 17000042–17000044 (blocks
known to contain Uniswap V3 Swap events). Store as `tests/fixtures/`:

```
tests/fixtures/
  block_17000042_header.json    ← ScopeHeader (hash, receipts_root, logs_bloom)
  block_17000042_receipts.json  ← Vec<TransactionReceipt>
  block_17000042_events.json    ← expected decoded events (ground truth)
```

```
fixture tests:
  - verify_receipts(fixture_receipts, fixture_header.receipts_root) → Ok
  - bloom scan on fixture headers finds block 17000042 as candidate
  - full pipeline on fixture data produces expected events (count + fields)
  - resume: process blocks 0–1, interrupt, process 1–2, no duplicates in events
  - INSERT OR IGNORE: inserting same event twice → 1 row, not 2
```

### Staging smoke tests (require devp2p, run manually)

```bash
# Connect and verify devp2p works
scopenode sync config.test.toml --dry-run
# → should find devp2p peers and show bloom estimate

# Full sync
scopenode sync config.test.toml
# → should complete, store events, no errors

# Verify resumability
scopenode sync config.test.toml  # interrupt with Ctrl+C at Stage 3
scopenode sync config.test.toml  # must resume from cursor, not restart

# Verify JSON-RPC
cast logs --rpc-url http://localhost:8545 \
  --address 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8 \
  --from-block 17000000 --to-block 17000100
# → must return the same events as direct cast call to a real RPC
```

---

## Definition of done

- `cargo build` — zero warnings (`#![deny(warnings)]`)
- `cargo test` — all unit + fixture tests pass (no live network needed)
- `cargo clippy -- -D warnings` — zero warnings
- `scopenode sync config.test.toml` connects to **devp2p peers** (no RPC URL in config)
- Headers are fetched from devp2p peers via `GetBlockHeaders`
- Receipts are fetched from devp2p peers via `GetReceipts`
- Every receipt passes Merkle verification against `receipts_root` in header
- Events stored with `source = 'devp2p'`
- `source = 'rpc'` NEVER appears in the events table (would mean provider was used)
- ABI fetched from Sourcify — no API key in config or env
- Sync is resumable: Ctrl+C + re-run picks up from sync_cursor, no duplicates
- `scopenode status` shows indexed contracts, event counts, pending_retry count
- `eth_getLogs` from `cast` returns verified events from staging node
- `eth_getLogs` for unindexed contract returns clear error, not empty array
- Error messages are human-readable (see error table above)
- `--bind` flag controls JSON-RPC listen address (default: `127.0.0.1:8545`)

---

## What this phase teaches you

**Ethereum data model:** Block header structure (every field), how `receipts_root`
commits to all receipts, how `logs_bloom` summarizes all events, how Solidity
events are encoded in logs (address, topics, data).

**devp2p networking:** discv4 Kademlia discovery, ENR records, RLPx ECIES
transport, the ETH sub-protocol Status handshake, GetBlockHeaders and
GetReceipts message formats, peer session lifecycle.

**Bloom filters:** 2048-bit array, 3-position hashing, zero false negatives,
why ~85% of blocks can be skipped without any network call.

**Merkle Patricia Trie:** leaf/extension/branch nodes, RLP encoding, how
`HashBuilder` builds incrementally in key order, why this is the root of
trustless verification.

**Rust patterns:** Workspace structure, trait objects vs. generics, `thiserror`
+`anyhow` split, `tracing` spans for async context, `sqlx` WAL mode, batched
async requests without unbounded concurrency, `INSERT OR IGNORE` for idempotency.

---

## What changes vs. the current (wrong) code

The current `crates/scopenode-core/src/network.rs` implements `RpcNetwork`
backed by an alloy HTTP provider (calls an Ethereum JSON-RPC endpoint). This
directly contradicts the project's core value proposition.

**Delete:** `RpcNetwork` struct and its `EthNetwork` impl in `network.rs`.
**Delete:** `alloy` provider imports in `network.rs` and `Cargo.toml`.
**Delete:** `futures::future::join_all()` unbounded concurrency (was a perf bug).
**Add:** `DevP2PNetwork` struct implementing `EthNetwork` via reth-network.
**Add:** `reth-discv4`, `reth-network`, `reth-eth-wire`, `reth-network-api` deps.
**Keep:** `EthNetwork` trait, `Pipeline<N>`, bloom scan, ABI layer, storage, RPC server.
**Keep:** `ReceiptFetchResult` enum (same shape, works for devp2p).

The `Pipeline<N: EthNetwork>` generic means the pipeline itself is unchanged.
Only the concrete transport implementation (`network.rs`) changes.
