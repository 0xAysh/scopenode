# scopenode

A focused Ethereum event indexer. Point it at a directory of local ERA1 archive files, tell it which contract events to watch, and it builds a queryable SQLite database you can serve via standard JSON-RPC.

No API keys. No Infura. No live P2P networking. ERA1 files only.

---

## How it works

```
ERA1 files (local disk)
    │
    ├── bloom filter (headers)   ──▶  skip ~87% of blocks instantly
    ├── fetch receipts           ──▶  Merkle verify against receipts_root
    └── ABI-decode matching logs ──▶  INSERT OR IGNORE into SQLite
                                              │
                                    JSON-RPC :8545   eth_getLogs
                                    REST API :8546   GET /events
```

**Pipeline per contract:**

| Step | What happens |
|------|--------------|
| 1. Scan ERA1 source | Discover `.era1` files in `era_dir` that overlap the block range |
| 2. Bloom filter | Check each block header's bloom — skip blocks that can't contain your events |
| 3. Receipt decode | Decompress and decode ERA1 receipts for bloom-hit blocks |
| 4. Merkle verify | Reconstruct receipt trie, assert root == `receipts_root` in the header |
| 5. Decode + store | ABI-decode matching logs, `INSERT OR IGNORE` into SQLite |

Every sync is **resumable** — interrupt with Ctrl+C and re-run `scopenode sync`.

---

## Install

**Prerequisites:** Rust 1.80+ and a local ERA1 archive directory.

```bash
git clone https://github.com/0xAysh/scopenode
cd scopenode
cargo build --release
# binary: ./target/release/scopenode
```

---

## Quick start

**1. Write a config:**

```toml
# config.toml
[node]
port      = 8545
rest_port = 8546
data_dir  = "~/.scopenode"
era_dir   = "~/era1"            # directory containing *.era1 files

[[contracts]]
name       = "Uniswap V3 ETH/USDC"
address    = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events     = ["Swap"]
from_block = 25000000
to_block   = 25010000
abi_override = "./abis/UniswapV3Pool.json"  # required — local ABI file
```

**2. Index events:**

```bash
scopenode sync --config config.toml
```

**3. Serve:**

```bash
scopenode serve --config config.toml
```

**4. Query:**

```bash
# via standard Ethereum JSON-RPC
cast logs --rpc-url http://localhost:8545 \
  --address 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8 \
  --event "Swap(address,address,int256,int256,uint160,uint128,int24)"

# via REST
curl "http://localhost:8546/events?contract=0x8ad5...&fromBlock=25000000&toBlock=25010000"
```

---

## Config reference

```toml
[node]
port      = 8545            # JSON-RPC port (default: 8545)
rest_port = 8546            # REST API port (default: 8546)
data_dir  = "~/.scopenode"  # SQLite database location (tilde expanded)
era_dir   = "~/era1"        # Directory containing *.era1 files (required)

[[contracts]]
name         = "My Contract"   # Optional label for progress output
address      = "0x..."         # Contract address (required)
events       = ["Transfer"]    # Event names to index (required, at least one)
from_block   = 17000000        # First block, inclusive (required)
to_block     = 18000000        # Last block, inclusive (required)
abi_override = "./abi.json"    # Local ABI JSON file (required — no remote fetch)

# Add as many [[contracts]] sections as needed.
# Block numbers accept shorthand: "16M" → 16_000_000, "12.3K" → 12_300.
```

**Validation at startup:**
- `abi_override` must be set — remote ABI fetching is not supported
- `to_block` must be set — live sync is not supported
- `to_block >= from_block` required
- Unknown fields in config are rejected

---

## CLI

```
scopenode <COMMAND>

Commands:
  sync   Read ERA1 files and index contract events into SQLite
  serve  Start the JSON-RPC (:8545) and REST (:8546) servers

Options:
  -v, --verbose   Increase log verbosity (-v info, -vv debug, -vvv trace)
```

### `sync`

```bash
scopenode sync [--config <path>] [--dry-run]

Options:
  --config <path>   Path to config file (default: ./config.toml)
  --dry-run         Print ERA1 source path and contract list, then exit
```

### `serve`

```bash
scopenode serve [--config <path>]

Options:
  --config <path>   Path to config file (default: ./config.toml)
```

Both servers run until Ctrl+C.

---

## JSON-RPC (`:8545`)

Standard Ethereum JSON-RPC. Drop into any Ethereum tooling:

```javascript
// viem
const logs = await client.getLogs({ address: "0x8ad5..." });

// ethers.js
const provider = new ethers.JsonRpcProvider("http://localhost:8545");

// cast
cast rpc eth_getLogs '{"address":"0x8ad5...","fromBlock":"0x17D4208","toBlock":"0x17D6990"}'
```

**Supported methods:**

| Method | Notes |
|--------|-------|
| `eth_getLogs` | Supports `address`, `topics[0]`, `fromBlock`, `toBlock`. Returns error (not truncation) when result exceeds 10,000 rows. |
| `eth_blockNumber` | Highest block number in the events table. |
| `eth_chainId` | Always `0x1` (Ethereum mainnet). |
| `net_peerCount` | Always `0x0` (ERA1-only; no live peers). |

---

## REST API (`:8546`)

```
GET /events
    ?contract=0x...      filter by contract address
    &event=Swap          filter by event name
    &topic0=0xddf252…    filter by raw topic0 hash
    &fromBlock=N         inclusive lower bound
    &toBlock=N           inclusive upper bound
    &limit=100           max rows (default 100, hard cap 10,000)
    &offset=0            pagination offset

GET /status             block number, contract count, total event count
GET /contracts          indexed contracts with per-contract event counts
GET /abi/:address       raw cached ABI JSON for a contract
```

Returns error (HTTP 400) when result exceeds 10,000 rows — consistent with `eth_getLogs`.

---

## Storage

Database: `~/.scopenode/scopenode.db` (SQLite, WAL mode).

**Tables:**

| Table | Contents |
|-------|----------|
| `events` | Decoded events: contract, event_name, topic0, block_number, block_hash, tx_hash, tx_index, log_index, raw_topics, raw_data, decoded JSON |
| `contracts` | Contract registry + cached ABI JSON |

`UNIQUE(block_number, tx_index, log_index)` ensures re-syncing is always idempotent.

---

## Project structure

```
scopenode/
├── config.example.toml
├── crates/
│   ├── scopenode/              # CLI binary
│   │   └── src/
│   │       ├── main.rs
│   │       └── commands/
│   │           ├── sync.rs     # ERA1 scan → index pipeline
│   │           └── serve.rs    # JSON-RPC + REST server startup
│   ├── scopenode-core/         # Pipeline logic
│   │   └── src/
│   │       ├── era_pipeline.rs # Per-contract ERA1 index loop
│   │       ├── source.rs       # ERA1 file discovery + block iterator
│   │       ├── headers.rs      # Bloom filter scanning
│   │       ├── receipts.rs     # Merkle Patricia Trie verification
│   │       ├── abi.rs          # ABI loading + event decoding
│   │       ├── config.rs       # TOML config types
│   │       └── error.rs
│   ├── scopenode-storage/      # SQLite layer
│   │   └── src/
│   │       ├── db.rs           # Db handle (WAL mode, INSERT OR IGNORE)
│   │       └── migrations/     # 005 migrations total
│   └── scopenode-rpc/          # Servers
│       └── src/
│           ├── server.rs       # eth_getLogs, eth_blockNumber, eth_chainId
│           └── rest.rs         # GET /events, /status, /contracts, /abi/:address
```

---

## Background

The first version of scopenode did all of this over live devp2p. It connected directly to Ethereum mainnet peers, ran Helios as a beacon light client for BLS-verified block headers, fetched receipts via `GetReceipts` wire messages, detected reorgs with a rolling hash buffer, and had a daemon mode so the sync process could run in the background. There were 14 CLI commands.

It worked, but the dependency graph was enormous — 10 reth crates, Helios, OpenSSL, reqwest — and the codebase was fighting itself. The devp2p path, the beacon path, the retry queue, the TUI, the daemon, and the ERA1 fallback all had different assumptions about state and lifecycle. Adding anything touched five modules. Debugging meant understanding which of three pipelines was actually running.

While working on ERA1 archive support as a "fallback for historical data", it became clear that ERA1 is actually a better foundation for the whole tool — the data is local, deterministic, Merkle-verifiable, and fast. No peer connection negotiation, no receipt fetch timeouts, no reorg detection needed. Ripping out everything else and rebuilding around ERA1 as the primary source made the codebase about 70% smaller and the pipeline a straight line from file to SQLite.

Live P2P sync is back on the roadmap — but as a clean addition on top of a working core, not tangled into it from the start.

---

## Roadmap

scopenode is intentionally minimal right now. The core pipeline is solid — ERA1 files → verified events → SQLite → JSON-RPC. What's missing is the ability to stay current and to reach more users who don't have a full ERA1 archive.

| | Feature | Why |
|--|---------|-----|
| 🔜 | **Live P2P sync** | Connect to Ethereum devp2p peers (`GetBlockHeaders` + `GetReceipts`) so scopenode can index new blocks as they're produced, past the ERA1 archive boundary. Picks up where ERA1 leaves off — no gap, no RPC dependency. |
| 🔜 | **Hash-chain verification** | Verify the ERA1 file chain (each file's first block hash must match the parent of the next file) to catch truncated or corrupt archives before indexing starts. |
| 🔜 | **`eth_subscribe` (WebSocket)** | Push live events to subscribers as they land — useful for apps that poll `eth_getLogs` in a loop today. Requires live P2P sync to be meaningful. |
| 🔜 | **Multi-address + topic wildcard queries** | `eth_getLogs` currently requires a single address and at most one topic0. Lifting this to match the full Ethereum filter spec (address array, topic OR logic) removes the last compatibility gap with public RPC providers. |
| 💭 | **Checkpoint-verified light client** | Use a beacon light client (BLS-verified sync committee) to confirm that devp2p peer headers are on the canonical chain — eliminates the trust assumption in live P2P mode. |

---

## Key dependencies

| Crate | Purpose |
|-------|---------|
| `alloy` | Ethereum types, RLP, EIP-2718 encoding |
| `alloy-dyn-abi` | Runtime ABI decoding for arbitrary event logs |
| `alloy-trie` | Merkle Patricia Trie (receipt root verification) |
| `sqlx` | Async SQLite with WAL mode |
| `jsonrpsee` | JSON-RPC 2.0 server |
| `axum` | REST HTTP server |
| `tokio` | Async runtime |
| `clap` | CLI argument parsing |
| `indicatif` | Progress bars |
| `snap` | Snappy decompression for ERA1 entries |
| `sha2` | SHA256 checksums for ERA1 file verification |
