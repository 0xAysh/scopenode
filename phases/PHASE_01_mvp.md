# Phase 1 — Working MVP

## Goal

Get a fully working end-to-end pipeline in the simplest possible way. By the
end of this phase, scopenode can sync verified Ethereum events and serve them
at `localhost:8545` — a real, usable tool. We use standard Ethereum RPC for
headers and receipts here (Helios + Portal Network come in Phase 2).

**What "done" looks like:**

```bash
scopenode sync config.toml
# syncs Uniswap V3 Swap events, stores in SQLite

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

Keep this file in the repo root. Add to `.gitignore` if it contains API keys.

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
config.test.toml          # may contain API keys
*.db                      # never commit SQLite files
*.db.snap                 # never commit snapshots
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


| Concern                 | Choice                                 | Why                                                  |
| ----------------------- | -------------------------------------- | ---------------------------------------------------- |
| Ethereum types          | `alloy`                                | Standard across reth, lighthouse ecosystem           |
| Header + receipt source | `alloy` provider (public RPC)          | Simple for MVP, replaced in Phase 2                  |
| ABI decoding            | `alloy-dyn-abi`                        | Runtime decoding for arbitrary ABIs                  |
| Merkle verification     | `alloy-trie`                           | Same as reth — well-tested                           |
| Storage                 | `sqlx` + SQLite                        | Zero setup, excellent for local read-heavy workloads |
| JSON-RPC server         | `jsonrpsee`                            | Standard in the Rust Ethereum ecosystem              |
| Async                   | `tokio`                                | The standard                                         |
| CLI                     | `clap` v4                              | Derive API, clean                                    |
| Progress                | `indicatif`                            | The standard progress bar library                    |
| HTTP client             | `reqwest`                              | For Etherscan ABI fetching                           |
| Error handling          | `thiserror` (libs) + `anyhow` (binary) | Standard pattern                                     |
| Logging                 | `tracing` + `tracing-subscriber`       | Async-aware, structured                              |
| Config                  | `serde` + `toml`                       | Standard                                             |


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
    │       ├── headers.rs      # header fetching + bloom scan
    │       ├── receipts.rs     # receipt fetching + verification
    │       ├── abi.rs          # etherscan fetch + event decoding
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
alloy = { version = "0.9", features = ["full"] }
alloy-primitives = "0.8"
alloy-dyn-abi = "0.8"
alloy-trie = "0.7"
tokio = { version = "1", features = ["full"] }
thiserror = "2"
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap = { version = "4", features = ["derive"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
sqlx = { version = "0.8", features = ["sqlite", "runtime-tokio", "migrate"] }
indicatif = "0.17"
hex = "0.4"
```

### Config

```toml
# config.example.toml
[node]
port = 8545
rest_port = 8546  # Phase 3
data_dir = "~/.scopenode"  # optional, defaults to this
# fallback_rpc = "https://eth.llamarpc.com"  # Phase 2: used only when Portal fails

[[contracts]]
name = "Uniswap V3 ETH/USDC"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events = ["Swap", "Mint", "Burn"]
from_block = 17000000
to_block = 17000100  # small range for testing; omit for live sync
```

### Config types + validation

```rust
// crates/scopenode-core/src/config.rs

use alloy_primitives::Address;
use serde::Deserialize;
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]wpub struct Config {
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
    pub fallback_rpc: Option<Url>,  // Phase 2
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractConfig {
    pub name: Option<String>,
    pub address: Address,
    pub events: Vec<String>,
    pub from_block: u64,
    pub to_block: Option<u64>,
    pub webhook: Option<Url>,       // Phase 3
    pub abi_override: Option<PathBuf>,
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

### What's in a block header (and why we care)

Ethereum block headers are ~500 bytes. The two fields that drive our entire
pipeline:

- `logs_bloom` (256 bytes) — a 2048-bit bloom filter that compactly represents
every address and topic that emitted an event in this block. We use this to
skip 85-90% of blocks without making any network call.
- `receipts_root` (32 bytes) — the Merkle Patricia Trie root of all receipts in
this block. We verify fetched receipts against this root to prove they're real.

Both fields are in the `alloy::rpc::types::Header` type. We store them in SQLite.

### Bloom filters (the math)

A Bloom filter is a 2048-bit array. To add an item:

1. `keccak256(item)` → 32 bytes
2. Take bytes `[0,1]`, `[2,3]`, `[4,5]` → 3 pairs → each `mod 2048` → 3 bit positions
3. Set those 3 bits to 1

To check membership: compute the same 3 positions. If all 3 are 1 → might be present.
If any is 0 → definitely not present.

Ethereum's `logsBloom` in each header is built by running this algorithm for:

- Every contract address that emitted any log
- Every topic (indexed param) of every log

So we can ask "did contract 0x8ad... maybe emit anything in block 17000042?"
by checking if the address bits are all set. Zero false negatives. Some false
positives (~15%). 100x speedup over fetching every block's receipts.

`alloy_primitives::Bloom` implements this. Use `bloom.contains_input(BloomInput::Raw(bytes))`.

### ABI encoding and event logs

Solidity events are stored in Ethereum logs. Each log has:

- `address` — the contract that emitted it
- `topics` — up to 4 × 32-byte words
  - `topics[0]` is always `keccak256("EventName(type1,type2,...)")` — the "event selector"
  - `topics[1..3]` are indexed parameters (zero-padded to 32 bytes)
- `data` — ABI-encoded non-indexed parameters

ABI encoding (for non-indexed params in `data`):

- Fixed-size types (uint256, address, bool, bytes32) → 32-byte word, left-padded
- Dynamic types (string, bytes, arrays) → 32-byte offset pointer, then length, then data

`alloy-dyn-abi` decodes this at runtime given the ABI JSON.

### Merkle Patricia Trie (receipts_root)

The `receipts_root` is the root hash of a Merkle Patricia Trie where:

- Keys: RLP-encoded transaction indices (0, 1, 2, ...)
- Values: RLP-encoded receipts

MPT nodes are RLP-encoded and hashed with keccak256. The root hash commits to
the entire set of receipts. If we rebuild this trie from the fetched receipts
and get the same root as in the header → the receipts are authentic.

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
    source          TEXT NOT NULL,   -- 'rpc' (Phase 1), 'portal' (Phase 2)
    reorged         INTEGER NOT NULL DEFAULT 0,  -- Phase 3
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
    impl_address     TEXT   -- non-null if proxy
);
```

---

## The pipeline (orchestrated in `pipeline.rs`)

```
Config
  ↓
ABI Fetch (Etherscan)     ← once per contract, cached in SQLite
  ↓
Header Sync               ← alloy provider, batch of 200 concurrent
  ↓ (stored in SQLite)
Bloom Scan                ← pure CPU, local
  ↓ (bloom_candidates in SQLite)
Receipt Fetch             ← alloy provider (Phase 2: Portal Network)
  ↓
Merkle Verify             ← alloy-trie, against receiptsRoot
  ↓
Event Extract + Decode    ← alloy-dyn-abi
  ↓
SQLite Store              ← idempotent (INSERT OR IGNORE)
  ↓
JSON-RPC serve            ← eth_getLogs from SQLite
```

### Pipeline implementation sketch

```rust
// crates/scopenode-core/src/pipeline.rs

pub struct Pipeline {
    config: Config,
    provider: Arc<dyn Provider>,  // alloy provider
    db: Db,
    abi_cache: AbiCache,
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
        let from = contract.from_block;
        let to = contract.to_block
            .unwrap_or(self.provider.get_block_number().await?);

        // 1. ABI: fetch from Etherscan, cache in SQLite
        let events = self.abi_cache.get_or_fetch(contract).await?;
        let targets = build_bloom_targets(&events, &contract.events);

        // 2. Headers: sync range, skip already-stored (resumable)
        self.sync_headers(from, to, progress).await?;

        // 3. Bloom scan: find candidates, store in bloom_candidates
        let candidates = self.bloom_scan(contract.address, &targets, from, to).await?;

        if dry_run {
            print_dry_run_result(from, to, candidates.len());
            return Ok(());
        }

        // 4. Receipts: fetch + verify + decode + store
        self.fetch_and_store(contract, &candidates, &events, progress).await?;

        Ok(())
    }

    async fn sync_headers(&self, from: u64, to: u64, progress: &SyncProgress) -> Result<()> {
        let cursor = self.db.get_cursor(&contract.address.to_string()).await?
            .and_then(|c| c.headers_done_to)
            .unwrap_or(from.saturating_sub(1));

        let remaining: Vec<u64> = (cursor + 1..=to).collect();

        futures::stream::iter(remaining)
            .map(|n| self.provider.get_block_by_number(n.into(), false))
            .buffer_unordered(200)
            .try_for_each(|block| async {
                if let Some(b) = block {
                    self.db.insert_header(&b.header.into()).await?;
                    progress.header_inc();
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
    }

    async fn bloom_scan(
        &self,
        address: Address,
        targets: &[BloomTarget],
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, B256)>> {
        let headers = self.db.get_headers(from, to).await?;
        let candidates: Vec<_> = headers.iter()
            .filter(|h| targets.iter().any(|t| t.matches(&h.logs_bloom)))
            .map(|h| (h.number, h.hash))
            .collect();

        for (num, hash) in &candidates {
            self.db.insert_bloom_candidate(*num, *hash, &address.to_string()).await?;
        }

        Ok(candidates)
    }

    async fn fetch_and_store(
        &mut self,
        contract: &ContractConfig,
        candidates: &[(u64, B256)],
        events: &[EventAbi],
        progress: &SyncProgress,
    ) -> Result<()> {
        let decoder = EventDecoder::new(events, contract.address)?;
        let addr = contract.address.to_string();

        // Filter out already-fetched blocks (resumable)
        let remaining: Vec<_> = futures::stream::iter(candidates.iter().cloned())
            .filter_map(|(block_num, block_hash)| {
                let db = &self.db;
                let addr = &addr;
                async move {
                    if db.is_fetched(block_num, addr).await.unwrap_or(false) {
                        None
                    } else {
                        Some((block_num, block_hash))
                    }
                }
            })
            .collect()
            .await;

        // Fetch receipts in parallel (up to 32 concurrent requests)
        let provider = self.provider.clone();
        let results: Vec<_> = futures::stream::iter(remaining.iter().cloned())
            .map(|(block_num, block_hash)| {
                let provider = provider.clone();
                async move {
                    let receipts = provider.get_block_receipts(block_num.into()).await;
                    (block_num, block_hash, receipts)
                }
            })
            .buffer_unordered(32)
            .collect()
            .await;

        // Verify + decode + store (sequential — DB writes are fast, correctness matters)
        for (block_num, block_hash, receipts_result) in results {
            let receipts = match receipts_result? {
                Some(r) => r,
                None => {
                    tracing::warn!(block = block_num, "No receipts returned");
                    self.db.mark_retry(block_num, &addr).await?;
                    continue;
                }
            };

            let header = self.db.get_header(block_num).await?
                .ok_or_else(|| anyhow::anyhow!("Header {block_num} not found in DB"))?;

            match verify_receipts(&receipts, header.receipts_root, block_num) {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(block = block_num, err = %e, "Verification failed");
                    self.db.mark_retry(block_num, &addr).await?;
                    continue;
                }
            }

            let decoded = decoder.extract_and_decode(&receipts, block_num, block_hash);

            self.db.insert_events(&decoded, "rpc").await?;
            self.db.mark_fetched(block_num, &addr).await?;

            for e in &decoded {
                progress.event_found(&e.event_name);
            }
            progress.receipt_inc();
        }

        Ok(())
    }
}
```

### Batching consecutive candidate blocks

When bloom-matched blocks are consecutive (common for high-activity contracts),
we batch them into ranges and use ranged `eth_getLogs` to reduce RPC round trips.
Individual Merkle verification still runs per block.

```rust
// crates/scopenode-core/src/pipeline.rs

use std::ops::Range;

/// Groups consecutive block numbers into ranges.
/// Input:  [(100, h1), (101, h2), (102, h3), (200, h4), (201, h5)]
/// Output: [100..103, 200..202]
fn batch_consecutive(candidates: &[(u64, B256)]) -> Vec<Range<u64>> {
    if candidates.is_empty() {
        return vec![];
    }

    let mut ranges = vec![];
    let mut start = candidates[0].0;
    let mut end = start;

    for &(num, _) in &candidates[1..] {
        if num == end + 1 {
            end = num;
        } else {
            ranges.push(start..end + 1);
            start = num;
            end = num;
        }
    }
    ranges.push(start..end + 1);
    ranges
}
```

The pipeline uses batched ranges for `eth_getLogs` calls (fewer round trips)
but still fetches full block receipts per block for Merkle verification. For
Phase 1 this is a pragmatic optimization — batch fetching reduces wall-clock
time by 5-10x for contracts with clustered activity.

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
    fallback_rpc: Option<String>,
}

#[async_trait::async_trait]
impl EthApiServer for EthApiImpl {
    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        let address = filter.address.as_ref()
            .and_then(|a| a.first())
            .copied();

        // Check if in scope
        let in_scope = if let Some(addr) = address {
            self.db.is_contract_indexed(&addr.to_string()).await.unwrap_or(false)
        } else {
            false
        };

        if !in_scope {
            if let Some(ref url) = self.fallback_rpc {
                return proxy_to_rpc(url, "eth_getLogs", &filter).await;
            }
            return Err(not_indexed_error());
        }

        // Query SQLite and reconstruct Log objects
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

    /// Override data directory from config (also: SCOPENODE_DATA_DIR env var)
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

In `main.rs`, after loading config, apply the override before opening the DB:

```rust
// Apply CLI/env data_dir override (takes precedence over config file)
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

Speed: 2.1 receipts/sec  |  ETA: 2s
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
  - verify_receipts() — passes on empty block (empty MPT root)
  - verify_receipts() — fails on tampered receipts
  - EventAbi.topic0() — correct keccak256 for known events
  - EventAbi.signature() — correct canonical form

Staging smoke tests (run against ~/.scopenode-staging, fast):
  - scopenode sync config.test.toml completes without error
  - Events stored match expected count for known block range
  - Sync is resumable: interrupt halfway, re-run, picks up from cursor
  - scopenode status shows correct event counts after sync

Integration tests (require network, --ignored):
  - Sync 10 blocks of Uniswap V3, verify event count matches Etherscan
  - eth_getLogs returns same result as public RPC for indexed range
```

---

## Definition of done

- `cargo build` passes with zero warnings (`#![deny(warnings)]`)
- `cargo test` passes (all non-network tests)
- `cargo clippy -- -D warnings` passes
- `--data-dir` and `SCOPENODE_DATA_DIR` correctly override config `data_dir`
- `config.test.toml` exists and works against `~/.scopenode-staging`
- `scopenode sync config.test.toml --dry-run` shows bloom estimate and confirms before proceeding
- `scopenode sync config.test.toml` syncs 100 blocks end-to-end against staging dir
- Events stored in staging SQLite match what Etherscan shows for those blocks
- `eth_getLogs` via `cast` or viem returns correct events from staging node
- Sync is resumable: interrupt and re-run, picks up where it left off
- Error messages are human-readable (no hex dumps, no Rust panic output)
- `scopenode status` shows indexed contracts and event counts

---

## What you learn in this phase

**Ethereum data model:** Block header structure (every field and why it exists),
how `receipts_root` and `logs_bloom` work, transaction receipt format, log
structure (address, topics, data), how events are encoded in raw bytes.

**Bloom filters:** The 2048-bit filter, k=3 hash functions, why false negatives
are impossible, how false positive rate is calculated, why this gives us 85-90%
block skipping.

**ABI encoding:** How Solidity events become bytes — topic0 as event selector,
indexed vs non-indexed params, the head/tail ABI encoding layout.

**Merkle Patricia Tries:** How `receipts_root` is built, leaf/extension/branch
node types, why inlining nodes < 32 bytes matters, how `alloy-trie` builds it.

**Rust patterns:** Workspace structure, `thiserror`/`anyhow` split, `tracing`
spans, `sqlx` compile-time query checking, `futures::stream::buffer_unordered`
for concurrent fetching, `INSERT OR IGNORE` for idempotency.