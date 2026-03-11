# Phase 1 — Working MVP

## Goal

Build a fully working end-to-end pipeline that embodies the vision from day
one. By the end of this phase, scopenode connects directly to the Ethereum
P2P network, fetches headers and receipts from devp2p peers, verifies them
cryptographically, and serves the results at `localhost:8545` — no RPC
provider, no API keys, no trusted third parties.

Phase 1 is simpler than Phase 2 in degree, not in kind:
- No multi-peer agreement (connect to a few peers, Merkle verification catches
  bad data automatically)
- No ERA1 fallback (devp2p only)
- Historical sync only (no live sync — that's Phase 3a)
- No proxy detection (use `abi_override` if needed)

**What "done" looks like:**

```bash
scopenode sync config.toml
# connects to Ethereum devp2p network, syncs Uniswap V3 Swap events,
# verifies Merkle proofs, stores in SQLite — no API key required

# In another terminal:
cast logs --rpc-url http://localhost:8545 \
  --address 0x8ad599c3... \
  --from-block 17000000 --to-block 17000100 \
  "Swap(address,address,int256,int256,uint160,uint128,int24)"
# → returns real events from local SQLite, instantly
```

---

## Staging environment (implement this first, before any pipeline code)

Before writing pipeline logic, wire in the staging environment so every
feature can be tested safely — isolated from any future production data.

### What "staging" means here

SQLite is a single file. A staging environment is just a different directory:

```
~/.scopenode/          ← production (never touch during development)
~/.scopenode-staging/  ← staging (blow away freely)
```

### How to switch environments

Two mechanisms, applied in order (last wins):
1. `data_dir` in config file
2. `--data-dir` CLI flag (overrides config)
3. `SCOPENODE_DATA_DIR` env var (overrides flag)

```bash
# staging — explicit flag
scopenode sync config.test.toml --data-dir ~/.scopenode-staging

# staging — env var (useful in scripts)
SCOPENODE_DATA_DIR=~/.scopenode-staging scopenode sync config.test.toml

# production (no flag needed — data_dir in config or default)
scopenode sync config.toml
```

### `config.test.toml`

Keep this file in the repo root. Add to `.gitignore`.

```toml
# config.test.toml — small range, fast to sync
[node]
port = 8545
data_dir = "~/.scopenode-staging"

[[contracts]]
name = "Uniswap V3 ETH/USDC (test)"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events = ["Swap", "Mint", "Burn"]
from_block = 17000000
to_block = 17000100   # 100 blocks — completes in ~30 seconds
```

### Snapshot and restore

SQLite is a single file — snapshot = `cp`. Use this before any test that
mutates data or before trying a schema change.

```bash
# Save current state
cp ~/.scopenode-staging/scopenode.db ~/.scopenode-staging/scopenode.db.snap

# Restore to saved state
cp ~/.scopenode-staging/scopenode.db.snap ~/.scopenode-staging/scopenode.db

# Clean slate
rm ~/.scopenode-staging/scopenode.db
```

`scopenode snapshot` / `scopenode restore` are added as proper CLI commands
in Phase 3a. For now, the `cp` workflow is sufficient.

### `.gitignore` additions

```
config.test.toml
*.db
*.db.snap
.env
```

### Typical dev workflow

```bash
# 1. Sync a small test range (fast)
scopenode sync config.test.toml

# 2. Verify events stored correctly
scopenode status
scopenode query --limit 5

# 3. Test the RPC server
scopenode serve config.test.toml &
cast logs --rpc-url http://localhost:8545 \
  --address 0x8ad599c3... \
  --from-block 17000000 --to-block 17000100

# 4. Need a clean slate? Just delete the staging DB
rm ~/.scopenode-staging/scopenode.db
```

---

## Stack decisions

| Concern | Choice | Why |
|---|---|---|
| Ethereum types | `alloy` | Standard across reth, lighthouse ecosystem |
| P2P networking | `reth-network` + `reth-eth-wire` + `reth-discv4` | Modular devp2p — peer discovery + ETH wire protocol |
| ABI source | Sourcify (`sourcify.dev`) | EF-run, no API key, open |
| ABI decoding | `alloy-dyn-abi` | Runtime decoding for arbitrary ABIs |
| Merkle verification | `alloy-trie` | Same as reth — well-tested |
| Storage | `sqlx` + SQLite | Zero setup, excellent for local read-heavy workloads |
| JSON-RPC server | `jsonrpsee` | Standard in the Rust Ethereum ecosystem |
| Async | `tokio` | The standard |
| CLI | `clap` v4 | Derive API, clean |
| Progress | `indicatif` | The standard progress bar library |
| HTTP client | `reqwest` | For Sourcify ABI fetching |
| Error handling | `thiserror` (libs) + `anyhow` (binary) | Standard pattern |
| Logging | `tracing` + `tracing-subscriber` | Async-aware, structured |
| Config | `serde` + `toml` | Standard |

---

## Project structure

```
scopenode/
├── Cargo.toml              # workspace
├── Cargo.lock
├── VISION.md
├── phases/
├── config.example.toml
└── crates/
    ├── scopenode/          # binary — CLI
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
    │       ├── network.rs      # devp2p peer management + GetReceipts/GetBlockHeaders
    │       ├── headers.rs      # header fetching + bloom scan
    │       ├── receipts.rs     # receipt fetching + Merkle verification
    │       ├── abi.rs          # Sourcify fetch + event decoding
    │       └── types.rs        # shared types
    ├── scopenode-storage/  # SQLite
    │   └── src/
    │       ├── lib.rs
    │       ├── db.rs
    │       ├── migrations/
    │       └── queries.rs
    └── scopenode-rpc/      # JSON-RPC server
        └── src/
            ├── lib.rs
            └── server.rs
```

---

## Concepts to deeply understand in this phase

### Workspace Cargo.toml

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
alloy             = { version = "0.9", features = ["full"] }
alloy-primitives  = "0.8"
alloy-dyn-abi     = "0.8"
alloy-trie        = "0.7"
reth-network      = { version = "1", default-features = false }
reth-eth-wire     = { version = "1", default-features = false }
reth-discv4       = { version = "1", default-features = false }
tokio             = { version = "1", features = ["full"] }
thiserror         = "2"
anyhow            = "1"
serde             = { version = "1", features = ["derive"] }
serde_json        = "1"
toml              = "0.8"
tracing           = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap              = { version = "4", features = ["derive"] }
reqwest           = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
sqlx              = { version = "0.8", features = ["sqlite", "runtime-tokio", "migrate"] }
indicatif         = "0.17"
hex               = "0.4"
```

### Config

```toml
# config.example.toml
[node]
port = 8545
rest_port = 8546          # Phase 3b
data_dir = "~/.scopenode" # optional, defaults to this

[[contracts]]
name = "Uniswap V3 ETH/USDC"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events = ["Swap", "Mint", "Burn"]
from_block = 17000000
to_block = 17000100       # omit for live sync (Phase 3a)
# abi_override = "./abis/MyContract.json"  # if not on Sourcify
```

### Config types + validation

```rust
// crates/scopenode-core/src/config.rs

use alloy_primitives::Address;
use serde::Deserialize;
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub node: NodeConfig,
    pub contracts: Vec<ContractConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_rest_port")]
    pub rest_port: u16,
    pub data_dir: Option<PathBuf>,
    pub fallback_rpc: Option<Url>, // Phase 2: last resort + proxy detection
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractConfig {
    pub name: Option<String>,
    pub address: Address,
    pub events: Vec<String>,
    pub from_block: u64,
    pub to_block: Option<u64>,
    pub webhook: Option<Url>,         // Phase 3b
    pub abi_override: Option<PathBuf>, // local ABI JSON — bypasses Sourcify
}

impl Config {
    pub fn from_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(path.to_owned(), e))?;
        let config: Self = toml::from_str(&content).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for c in &self.contracts {
            if let Some(to) = c.to_block {
                if to < c.from_block {
                    return Err(ConfigError::InvalidRange {
                        from: c.from_block,
                        to,
                        address: c.address,
                    });
                }
            }
            if c.events.is_empty() {
                return Err(ConfigError::NoEvents(c.address));
            }
        }
        Ok(())
    }
}

fn default_port() -> u16 { 8545 }
fn default_rest_port() -> u16 { 8546 }
```

### The Ethereum devp2p network

Ethereum's execution layer P2P network (devp2p) is how all full nodes
communicate. It has two layers:

**Discovery (discv4):** UDP-based Kademlia DHT. Nodes sign and broadcast
their identity as ENR records (Ethereum Node Records — IP, port, public key).
We bootstrap from well-known mainnet bootnodes and discover peers from there.

**RLPx (TCP):** Encrypted, authenticated TCP connections. After discovery,
we open a TCP connection, perform an ECIES handshake to establish session keys,
then run the ETH sub-protocol on top.

The ETH sub-protocol messages we care about:
- `GetBlockHeaders` / `BlockHeaders` — request headers by block number or hash
- `GetReceipts` / `Receipts` — request receipts for a list of block hashes

Full nodes serve these as part of their normal operation. Tens of thousands
of them exist on mainnet. We connect to a handful and rotate as needed.

`reth-discv4` handles peer discovery. `reth-network` + `reth-eth-wire` handle
RLPx transport and the ETH sub-protocol. We never implement crypto ourselves.

### What's in a block header (and why we care)

Ethereum block headers are ~500 bytes. The two fields that drive our pipeline:

- `logs_bloom` (256 bytes) — a 2048-bit bloom filter that compactly represents
  every address and topic that emitted an event in this block. We use this to
  skip 85-90% of blocks without fetching anything.
- `receipts_root` (32 bytes) — the Merkle Patricia Trie root of all receipts
  in this block. We verify fetched receipts against this root to prove they
  are real.

Both fields are in the `alloy::rpc::types::Header` type. We store them in
SQLite after fetching them from devp2p peers.

### Why devp2p is trustless for historical data

For finalized blocks (older than ~12 minutes / 64 blocks), the canonical chain
is cryptographically fixed by the beacon chain. Any honest full node returns
the same header for a given block number.

But we don't need to trust peers at all — even for headers. Here's why:

The `receipts_root` in the header commits to all receipts in the block. When
a peer sends us receipts, we rebuild the Merkle Patricia Trie from those
receipts and check that the root matches the `receipts_root` in the header.

If a peer sends a fake header AND fake receipts that match each other — we'd
have a problem. But the header's `parent_hash` chain connects all the way back
to genesis, and headers are tiny (500 bytes) with a well-defined structure.
In Phase 1, we request headers from multiple peers and accept the majority.
In Phase 2, multi-peer agreement is formalized and hardened.

The fundamental invariant: **Merkle verification catches any peer that
tampers with receipts.** A peer can't make fake events look real without
knowing the preimage of the `receipts_root`, which is computationally
infeasible.

### Bloom filters (the math)

A Bloom filter is a 2048-bit array. To add an item:

1. `keccak256(item)` → 32 bytes
2. Take bytes `[0,1]`, `[2,3]`, `[4,5]` → 3 pairs → each `mod 2048` → 3 bit positions
3. Set those 3 bits to 1

To check membership: compute the same 3 positions. If all 3 are 1 → might be
present. If any is 0 → definitely not present.

Ethereum's `logsBloom` in each header is built by running this for every
contract address and every topic of every log in the block.

So we ask "did contract 0x8ad... maybe emit anything in block 17000042?"
by checking if the address bits are set. Zero false negatives. ~15% false
positives. 85-90% of blocks skipped with no network call.

`alloy_primitives::Bloom` implements this. Use
`bloom.contains_input(BloomInput::Raw(bytes))`.

### ABI encoding and event logs

Solidity events are stored in Ethereum logs. Each log has:

- `address` — the contract that emitted it
- `topics` — up to 4 × 32-byte words
  - `topics[0]` is always `keccak256("EventName(type1,type2,...)")` — the event selector
  - `topics[1..3]` are indexed parameters (zero-padded to 32 bytes)
- `data` — ABI-encoded non-indexed parameters

ABI encoding (for non-indexed params in `data`):
- Fixed-size types (uint256, address, bool, bytes32) → 32-byte word, left-padded
- Dynamic types (string, bytes, arrays) → 32-byte offset pointer, then length, then data

`alloy-dyn-abi` decodes this at runtime given the ABI JSON from Sourcify.

### Sourcify — ABI source

Sourcify (sourcify.dev) is the Ethereum Foundation's open contract
verification platform. No API key. API:

```
GET https://sourcify.dev/server/files/any/1/{address}
```

Returns a JSON object with `files`. The ABI lives in `metadata.json` under
`output.abi`. Parse event entries (those with `"type": "event"`).

If a contract is not on Sourcify: scopenode errors with a clear message
telling the user to set `abi_override` in their config.

### Merkle Patricia Trie (receipts_root)

The `receipts_root` is the root hash of a Merkle Patricia Trie where:
- Keys: RLP-encoded transaction indices (0, 1, 2, ...)
- Values: RLP-encoded receipts

MPT nodes are RLP-encoded and hashed with keccak256. The root hash commits to
the entire receipt set. Rebuild the trie from fetched receipts, check root
against header — if they match, the receipts are authentic.

`alloy-trie`'s `HashBuilder` builds this trie incrementally. Items must be
inserted in key order.

---

## SQLite schema

```sql
-- migrations/001_init.sql

CREATE TABLE IF NOT EXISTS headers (
    number          INTEGER PRIMARY KEY,
    hash            TEXT NOT NULL UNIQUE,
    parent_hash     TEXT NOT NULL,
    timestamp       INTEGER NOT NULL,
    receipts_root   TEXT NOT NULL,
    logs_bloom      BLOB NOT NULL,   -- 256 bytes
    gas_used        INTEGER NOT NULL,
    base_fee        INTEGER          -- NULL before EIP-1559
);

CREATE TABLE IF NOT EXISTS bloom_candidates (
    block_number     INTEGER NOT NULL,
    block_hash       TEXT NOT NULL,
    contract         TEXT NOT NULL,
    fetched          INTEGER NOT NULL DEFAULT 0,
    pending_retry    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (block_number, contract)
);

CREATE TABLE IF NOT EXISTS events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    contract        TEXT NOT NULL,
    event_name      TEXT NOT NULL,
    topic0          TEXT NOT NULL,
    block_number    INTEGER NOT NULL,
    block_hash      TEXT NOT NULL,
    tx_hash         TEXT NOT NULL,
    tx_index        INTEGER NOT NULL,
    log_index       INTEGER NOT NULL,
    raw_topics      TEXT NOT NULL,   -- JSON array of hex strings
    raw_data        TEXT NOT NULL,   -- hex string
    decoded         TEXT NOT NULL,   -- JSON object of named fields
    source          TEXT NOT NULL,   -- 'devp2p' | 'era1' | 'rpc'
    reorged         INTEGER NOT NULL DEFAULT 0,  -- Phase 3a
    UNIQUE (tx_hash, log_index)
);

CREATE INDEX IF NOT EXISTS idx_events_lookup
    ON events(contract, event_name, block_number) WHERE reorged = 0;

CREATE TABLE IF NOT EXISTS sync_cursor (
    contract         TEXT PRIMARY KEY,
    from_block       INTEGER NOT NULL,
    to_block         INTEGER,
    headers_done_to  INTEGER,
    bloom_done_to    INTEGER,
    receipts_done_to INTEGER
);

CREATE TABLE IF NOT EXISTS contracts (
    address          TEXT PRIMARY KEY,
    name             TEXT,
    abi_json         TEXT,
    impl_address     TEXT   -- non-null if proxy (Phase 2)
);
```

---

## The pipeline (orchestrated in `pipeline.rs`)

```
Config
  ↓
ABI Fetch (Sourcify)      ← once per contract, cached in SQLite
  ↓
Header Sync               ← devp2p GetBlockHeaders, batch of 64 concurrent
  ↓ (stored in SQLite)
Bloom Scan                ← pure CPU, local
  ↓ (bloom_candidates in SQLite)
Receipt Fetch             ← devp2p GetReceipts (batch up to 16 hashes/request)
  ↓
Merkle Verify             ← alloy-trie, against receipts_root in header
  ↓
Event Extract + Decode    ← alloy-dyn-abi
  ↓
SQLite Store              ← idempotent (INSERT OR IGNORE)
  ↓
JSON-RPC serve            ← eth_getLogs from SQLite
```

### devp2p network setup

```rust
// crates/scopenode-core/src/network.rs

use reth_discv4::{Discv4, Discv4Config};
use reth_network::{NetworkConfig, NetworkManager, NetworkHandle};

pub struct ScopeNetwork {
    handle: NetworkHandle,
}

impl ScopeNetwork {
    pub async fn start() -> Result<Self, NetworkError> {
        let secret_key = secp256k1::SecretKey::new(&mut rand::thread_rng());

        let discv4_config = Discv4Config::builder()
            .add_boot_nodes(reth_discv4::bootnodes::mainnet_nodes())
            .build();

        let network_config = NetworkConfig::builder(secret_key)
            .discovery(discv4_config)
            .build();

        let manager = NetworkManager::new(network_config).await?;
        let handle = manager.handle().clone();
        tokio::spawn(manager);

        // Wait briefly for initial peer discovery
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tracing::info!(peers = handle.num_connected_peers(), "devp2p ready");

        Ok(Self { handle })
    }

    /// Fetch block headers for a range. Tries connected peers; skips failed ones.
    pub async fn get_headers(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<ScopeHeader>, NetworkError> {
        let peers = self.handle.get_peers(3);
        for peer in peers {
            match self.handle.request_block_headers(peer, from, to).await {
                Ok(headers) if !headers.is_empty() => {
                    return Ok(headers.into_iter().map(ScopeHeader::from).collect());
                }
                Ok(_) => continue,
                Err(e) => tracing::warn!(peer = %peer, err = %e, "Header request failed"),
            }
        }
        Err(NetworkError::HeadersFailed(from, to))
    }

    /// Fetch receipts for a batch of block hashes. Tries peers; Merkle-verifies each.
    /// GetReceipts supports batching — up to 16 hashes per request.
    pub async fn get_receipts(
        &self,
        blocks: &[(u64, B256, B256)], // (block_num, block_hash, receipts_root)
    ) -> Vec<ReceiptResult> {
        let hashes: Vec<B256> = blocks.iter().map(|(_, h, _)| *h).collect();
        let peers = self.handle.get_peers(5);

        for peer in peers {
            match self.handle.request_receipts(peer, hashes.clone()).await {
                Ok(batch) => {
                    return batch.into_iter()
                        .zip(blocks.iter())
                        .map(|(receipts, (num, hash, root))| {
                            match verify_receipts(&receipts, *root, *num) {
                                Ok(()) => ReceiptResult::Ok(*num, *hash, receipts),
                                Err(e) => {
                                    tracing::warn!(
                                        block = num, peer = %peer, err = %e,
                                        "Merkle verification failed — peer sent bad data"
                                    );
                                    ReceiptResult::Failed(*num)
                                }
                            }
                        })
                        .collect();
                }
                Err(e) => {
                    tracing::warn!(peer = %peer, err = %e, "Receipt request failed");
                }
            }
        }

        blocks.iter().map(|(num, _, _)| ReceiptResult::Failed(*num)).collect()
    }
}
```

### Pipeline implementation sketch

```rust
// crates/scopenode-core/src/pipeline.rs

pub struct Pipeline {
    config: Config,
    network: Arc<ScopeNetwork>,
    db: Db,
    abi_cache: AbiCache,  // backed by Sourcify
}

impl Pipeline {
    pub async fn run(&mut self, dry_run: bool, progress: &SyncProgress) -> Result<()> {
        for contract in &self.config.contracts.clone() {
            self.sync_contract(contract, dry_run, progress).await?;
        }
        Ok(())
    }

    async fn sync_contract(
        &mut self,
        contract: &ContractConfig,
        dry_run: bool,
        progress: &SyncProgress,
    ) -> Result<()> {
        // 1. ABI: fetch from Sourcify (or abi_override), cache in SQLite
        let events = self.abi_cache.get_or_fetch(contract).await?;
        let targets = build_bloom_targets(&events, &contract.events);

        // Determine to_block: use config value or ask peers for latest
        let to = contract.to_block
            .unwrap_or(self.network.best_block_number().await?);

        // 2. Headers: sync range via devp2p, resumable
        self.sync_headers(contract.from_block, to, progress).await?;

        // 3. Bloom scan: pure CPU, from SQLite
        let candidates = self.bloom_scan(contract.address, &targets,
            contract.from_block, to).await?;

        if dry_run {
            print_dry_run_result(contract.from_block, to, candidates.len());
            return Ok(());
        }

        // 4. Receipts: fetch + verify + decode + store
        self.fetch_and_store(contract, &candidates, &events, progress).await?;

        Ok(())
    }

    async fn sync_headers(&self, from: u64, to: u64, progress: &SyncProgress)
        -> Result<()>
    {
        let cursor = self.db.get_headers_cursor(from).await?
            .unwrap_or(from.saturating_sub(1));

        // Fetch in batches of 64 headers (ETH wire protocol limit per request)
        for chunk_start in (cursor + 1..=to).step_by(64) {
            let chunk_end = (chunk_start + 63).min(to);
            let headers = self.network.get_headers(chunk_start, chunk_end).await?;

            for header in headers {
                self.db.insert_header(&header).await?;
                progress.header_inc();
            }

            self.db.set_headers_cursor(chunk_end).await?;
        }

        Ok(())
    }

    async fn bloom_scan(
        &self,
        address: Address,
        targets: &[BloomTarget],
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, B256, B256)>> { // (block_num, block_hash, receipts_root)
        let headers = self.db.get_headers(from, to).await?;
        let candidates: Vec<_> = headers.iter()
            .filter(|h| targets.iter().any(|t| t.matches(&h.logs_bloom)))
            .map(|h| (h.number, h.hash, h.receipts_root))
            .collect();

        for (num, hash, _) in &candidates {
            self.db.insert_bloom_candidate(*num, *hash,
                &address.to_string()).await?;
        }

        Ok(candidates)
    }

    async fn fetch_and_store(
        &mut self,
        contract: &ContractConfig,
        candidates: &[(u64, B256, B256)],
        events: &[EventAbi],
        progress: &SyncProgress,
    ) -> Result<()> {
        let decoder = EventDecoder::new(events, contract.address)?;
        let addr = contract.address.to_string();

        // Filter already-fetched blocks (resumable)
        let remaining: Vec<_> = candidates.iter()
            .filter(|(num, _, _)| {
                !self.db.is_fetched_sync(*num, &addr)
            })
            .cloned()
            .collect();

        // GetReceipts supports batching — send up to 16 hashes per request
        for chunk in remaining.chunks(16) {
            let results = self.network.get_receipts(chunk).await;

            for result in results {
                match result {
                    ReceiptResult::Ok(block_num, block_hash, receipts) => {
                        // Merkle verification already done in get_receipts()
                        let decoded = decoder.extract_and_decode(
                            &receipts, block_num, block_hash);

                        self.db.insert_events(&decoded, "devp2p").await?;
                        self.db.mark_fetched(block_num, &addr).await?;

                        for e in &decoded {
                            progress.event_found(&e.event_name);
                        }
                        progress.receipt_inc();
                    }
                    ReceiptResult::Failed(block_num) => {
                        tracing::warn!(block = block_num,
                            "All peers failed for this block — marking pending_retry");
                        self.db.mark_retry(block_num, &addr).await?;
                    }
                }
            }
        }

        Ok(())
    }
}
```

---

## JSON-RPC server (`eth_getLogs`)

```rust
// crates/scopenode-rpc/src/server.rs

use jsonrpsee::{proc_macros::rpc, core::RpcResult, server::Server};
use alloy::rpc::types::{Filter, Log};
use scopenode_storage::Db;

#[rpc(server, namespace = "eth")]
pub trait EthApi {
    #[method(name = "getLogs")]
    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>>;

    #[method(name = "blockNumber")]
    async fn block_number(&self) -> RpcResult<String>;

    #[method(name = "chainId")]
    async fn chain_id(&self) -> RpcResult<String>;
}

pub struct EthApiImpl {
    db: Db,
}

#[async_trait::async_trait]
impl EthApiServer for EthApiImpl {
    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        let address = filter.address.as_ref()
            .and_then(|a| a.first())
            .copied();

        let in_scope = if let Some(addr) = address {
            self.db.is_contract_indexed(&addr.to_string()).await.unwrap_or(false)
        } else {
            false
        };

        if !in_scope {
            return Err(not_indexed_error());
        }

        let rows = self.db.query_events_for_filter(&filter).await
            .map_err(|e| internal_error(&e))?;

        Ok(rows.into_iter().map(row_to_log).collect())
    }

    async fn block_number(&self) -> RpcResult<String> {
        let n = self.db.latest_block_number().await.map_err(|e| internal_error(&e))?;
        Ok(format!("0x{:x}", n.unwrap_or(0)))
    }

    async fn chain_id(&self) -> RpcResult<String> {
        Ok("0x1".into())
    }
}

fn not_indexed_error() -> jsonrpsee::types::ErrorObject<'static> {
    jsonrpsee::types::ErrorObject::owned(
        -32000,
        "Contract not indexed. Run `scopenode status` to see what's indexed.",
        None::<()>,
    )
}
```

---

## CLI

```rust
// crates/scopenode/src/cli.rs

#[derive(Parser)]
#[command(name = "scopenode", version, about = "Scoped Ethereum node")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Override data directory (also: SCOPENODE_DATA_DIR env var)
    #[arg(long, global = true, env = "SCOPENODE_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start sync (resumes if interrupted)
    Sync {
        config: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Show indexed contracts and sync state
    Status,
    /// Query indexed events
    Query {
        #[arg(long)] contract: Option<String>,
        #[arg(long)] event: Option<String>,
        #[arg(long, default_value = "20")] limit: usize,
        #[arg(long, default_value = "table")] output: String,
    },
}
```

In `main.rs`, apply the data_dir override before opening the DB:

```rust
if let Some(dir) = cli.data_dir {
    config.node.data_dir = Some(dir);
}
let data_dir = config.node.data_dir
    .clone()
    .unwrap_or_else(|| dirs::home_dir().unwrap().join(".scopenode"));
std::fs::create_dir_all(&data_dir)?;
let db = Db::open(data_dir.join("scopenode.db")).await?;
```

### Progress display

```
scopenode — Uniswap V3 ETH/USDC (0x8ad599c3...)

Stage 1/3  Header sync      ████████████████████  100 / 100     done
Stage 2/3  Bloom scan       ████████████████████  100 / 100     done (8 hits)
Stage 3/3  Receipt fetch    ████████░░░░░░░░░░░░    4 / 8       50%

devp2p peers: 4 active  |  Speed: 2.1 receipts/sec  |  ETA: 2s
Events found: 3 Swap  |  1 Mint
```

---

## Dry run output

```
Dry run complete for Uniswap V3 ETH/USDC (0x8ad599c3...)
  Block range:     17000000 → 17000100 (100 blocks)
  Bloom matches:   8 blocks (8.0% hit rate)
  Estimated time:  < 1 minute

Start sync? [Y/n]
```

---

## Tests to write

```
Unit tests:
  - Config parses valid TOML
  - Config rejects unknown fields
  - Config catches invalid block ranges
  - CLI --data-dir overrides config data_dir
  - SCOPENODE_DATA_DIR env var overrides --data-dir
  - BloomTarget.matches() — true when address + topic present
  - BloomTarget.matches() — false when address absent
  - BloomTarget.matches() — false when no topics match
  - verify_receipts() — passes on correct receipts (known block)
  - verify_receipts() — fails on tampered receipts (wrong root)
  - verify_receipts() — passes on empty block (empty MPT root)
  - EventAbi.topic0() — correct keccak256 for known events
  - EventAbi.signature() — correct canonical form
  - SourcifyClient: parses ABI correctly from known metadata.json structure
  - SourcifyClient: returns NotOnSourcify error for unknown address (mocked 404)

Staging smoke tests (run against ~/.scopenode-staging):
  - scopenode sync config.test.toml completes without error
  - Events stored match expected count for known block range
  - Sync is resumable: interrupt halfway, re-run, picks up from cursor
  - scopenode status shows correct event counts after sync
  - pending_retry blocks are logged when peers fail

Integration tests (require network, --ignored):
  - devp2p discovers and connects to 3+ mainnet peers
  - GetBlockHeaders returns correct header for block 17000000
  - GetReceipts returns receipts that pass Merkle verification
  - Sourcify returns valid ABI for Uniswap V3 pool address
  - eth_getLogs from localhost:8545 returns events for synced range
```

---

## Definition of done

- `cargo build` passes with zero warnings (`#![deny(warnings)]`)
- `cargo test` passes (all non-network tests)
- `cargo clippy -- -D warnings` passes
- `--data-dir` and `SCOPENODE_DATA_DIR` correctly override config `data_dir`
- `config.test.toml` exists and works against `~/.scopenode-staging`
- `scopenode sync config.test.toml --dry-run` shows bloom estimate and prompts
- `scopenode sync config.test.toml` syncs 100 blocks end-to-end via devp2p
- Headers fetched from devp2p peers, stored in SQLite
- Receipts fetched from devp2p peers using batched `GetReceipts`
- Every receipt passes Merkle verification against `receipts_root` in header
- Events stored with `source = 'devp2p'`
- ABI fetched from Sourcify — no API key required
- If contract not on Sourcify: clear error telling user to set `abi_override`
- `eth_getLogs` via `cast` or viem returns correct events from staging node
- Sync is resumable: interrupt and re-run, picks up where it left off
- Error messages are human-readable (no hex dumps, no Rust panic output)
- `scopenode status` shows indexed contracts and event counts

---

## What you learn in this phase

**Ethereum data model:** Block header structure (every field and why it exists),
how `receipts_root` and `logs_bloom` work, transaction receipt format, log
structure (address, topics, data), how events are encoded in raw bytes.

**devp2p networking:** discv4 peer discovery (Kademlia DHT, ENR records),
RLPx transport (ECIES handshake, session keys, frame encryption), the ETH wire
protocol (`GetBlockHeaders`, `GetReceipts`), batching requests, handling peer
failures.

**Bloom filters:** The 2048-bit filter, k=3 hash functions, why false negatives
are impossible, how false positive rate is calculated, why this gives us 85-90%
block skipping.

**ABI encoding:** How Solidity events become bytes — topic0 as event selector,
indexed vs non-indexed params, the head/tail ABI encoding layout.

**Merkle Patricia Tries:** How `receipts_root` is built, leaf/extension/branch
node types, why inlining nodes < 32 bytes matters, how `alloy-trie` builds it,
why this is the fundamental trustless verification mechanism.

**Rust patterns:** Workspace structure, `thiserror`/`anyhow` split, `tracing`
spans, `sqlx` compile-time query checking, batched async requests,
`INSERT OR IGNORE` for idempotency.
