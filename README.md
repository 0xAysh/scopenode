# scopenode

A custom Ethereum node that syncs exactly the contract events you care about — directly from mainnet peers, verified cryptographically, served locally at `localhost:8545`.

No Infura. No Alchemy. No API keys. No rate limits.

```
scopenode sync config.toml
# ✓ connected to 12 devp2p peers
# ✓ fetched 1,000 headers  (bloom: 87 candidates)
# ✓ verified 87 receipt batches against receiptsRoot
# ✓ decoded 412 Swap events → stored in SQLite
# ✓ JSON-RPC server running at localhost:8545
```

---

## How it works

```text
  Ethereum mainnet peers (devp2p)
          │
          │  GetBlockHeaders    ──▶  bloom scan (local CPU)
          │  GetReceipts        ──▶  Merkle verify (alloy-trie)
          │
          ▼
  SQLite (WAL mode)
          │
          ▼
  JSON-RPC :8545   ──▶  eth_getLogs / eth_blockNumber / eth_chainId
```

**Five pipeline stages**, run once per configured contract:

| Stage | What happens |
|---|---|
| **1. ABI fetch** | Pull event signatures from [Sourcify](https://sourcify.dev) (or a local file). Cached in SQLite. |
| **2. Header sync** | `GetBlockHeaders` via devp2p → store `logs_bloom` + `receipts_root` for each block. |
| **3. Bloom scan** | CPU-only: check each header's bloom filter. Skips ~87% of blocks instantly. |
| **4. Receipt fetch + verify** | `GetReceipts` for bloom candidates → rebuild Merkle Patricia Trie → assert root == `receipts_root`. |
| **5. Decode + store** | ABI-decode matching logs via `alloy-dyn-abi`. `INSERT OR IGNORE` into SQLite. |

Every sync is **resumable** — interrupt with Ctrl+C, re-run `scopenode sync`, pick up exactly where you left off.

---

## Install

**Prerequisites:** Rust 1.80+ and Cargo.

```bash
git clone https://github.com/you/scopenode
cd scopenode
cargo build --release
# binary at: ./target/release/scopenode
```

---

## Quick start

**1. Write a config:**

```toml
# config.toml

[node]
port = 8545

[[contracts]]
name     = "Uniswap V3 ETH/USDC"
address  = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events   = ["Swap", "Mint", "Burn"]
from_block = 17000000
to_block   = 17001000
```

**2. Sync:**

```bash
scopenode sync config.toml
```

**3. Query:**

```bash
# standard eth_getLogs via any Ethereum library
cast logs --rpc-url http://localhost:8545 \
  --address 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8 \
  --event "Swap(address,address,int256,int256,uint160,uint128,int24)"
```

Or via the built-in query command:

```bash
scopenode query --contract 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8 --event Swap
```

---

## Config reference

```toml
[node]
port      = 8545          # JSON-RPC port (default: 8545)
data_dir  = "~/.scopenode" # Where to store the SQLite database

[[contracts]]
name       = "My Contract"   # Optional label
address    = "0x..."         # Contract address (required)
events     = ["Transfer"]    # Event names to index (required)
from_block = 17000000        # First block (required)
to_block   = 18000000        # Last block (optional — omit for live-tip sync)
abi_override = "./abi.json"  # Local ABI file if contract isn't on Sourcify

# Add as many [[contracts]] sections as you need
```

**Data directory resolution** (highest priority first):

1. `--data-dir /path` CLI flag
2. `SCOPENODE_DATA_DIR=/path` environment variable
3. `data_dir = "..."` in config file
4. Default: `~/.scopenode/`

---

## CLI

```
scopenode <COMMAND>

Commands:
  sync    Sync events for contracts in a config file
  status  Show indexed contracts and event counts
  query   Query indexed events from the terminal
  help    Print help

Options:
  --data-dir <PATH>   Override data directory
  -v, --verbose       Increase log verbosity (-vv, -vvv)
```

### `sync`

```bash
scopenode sync config.toml [OPTIONS]

Options:
  --dry-run   Bloom scan only — show candidate count and time estimate, don't fetch receipts
```

### `status`

```bash
scopenode status
# Contract: Uniswap V3 ETH/USDC (0x8ad5...)
#   Events indexed: 412 (Swap: 389, Mint: 18, Burn: 5)
#   Blocks:  17,000,000 – 17,001,000  (headers: ✓  receipts: ✓)
```

### `query`

```bash
scopenode query [OPTIONS]

Options:
  --contract <ADDR>    Filter by contract address
  --event <NAME>       Filter by event name
  --limit <N>          Max results (default: 20)
  --output <FORMAT>    Output format: table or json
```

---

## JSON-RPC

scopenode serves standard Ethereum JSON-RPC at `localhost:8545`. Drop it into any Ethereum tooling:

```javascript
// viem
const client = createPublicClient({ transport: http("http://localhost:8545") });
const logs = await client.getLogs({ address: "0x8ad5..." });

// ethers.js
const provider = new ethers.JsonRpcProvider("http://localhost:8545");

// web3.py
w3 = Web3(Web3.HTTPProvider("http://localhost:8545"))
```

**Supported methods:**

| Method | Description |
|---|---|
| `eth_getLogs` | Query indexed events. Supports `address`, `topics`, `fromBlock`, `toBlock`. |
| `eth_blockNumber` | Highest indexed block. |
| `eth_chainId` | Always `0x1` (Ethereum mainnet). |

Querying a contract that hasn't been indexed returns a clear error pointing to `scopenode status`.

---

## Project structure

```
scopenode/
├── config.example.toml
├── crates/
│   ├── scopenode/           # CLI binary
│   │   └── src/
│   │       ├── main.rs
│   │       └── commands/
│   │           ├── sync.rs
│   │           ├── status.rs
│   │           └── query.rs
│   ├── scopenode-core/      # Pipeline + P2P networking
│   │   └── src/
│   │       ├── pipeline.rs  # 5-stage orchestrator
│   │       ├── network.rs   # EthNetwork trait + DevP2PNetwork (reth devp2p)
│   │       ├── headers.rs   # Bloom filter scanning
│   │       ├── receipts.rs  # Merkle Patricia Trie verification
│   │       ├── abi.rs       # Sourcify fetch + event decoding
│   │       ├── config.rs    # TOML config types
│   │       ├── types.rs     # ScopeHeader, StoredEvent, etc.
│   │       └── error.rs
│   ├── scopenode-storage/   # SQLite layer
│   │   └── src/
│   │       ├── db.rs        # Db handle (Arc, WAL mode, INSERT OR IGNORE)
│   │       └── migrations/
│   │           └── 001_init.sql
│   └── scopenode-rpc/       # JSON-RPC server
│       └── src/
│           └── server.rs    # eth_getLogs, eth_blockNumber, eth_chainId
```

**Key design decisions:**

- **`EthNetwork` trait** — the pipeline is generic over its transport. Swapping from devp2p to a different source (e.g. ERA1 archives) only changes `network.rs`.
- **No RPC provider** — all block data comes from Ethereum P2P peers via `GetBlockHeaders` and `GetReceipts` wire messages.
- **Merkle verification** — receipts are rejected if the reconstructed trie root doesn't match `receipts_root` in the header. Peers cannot forge events.
- **Bloom filter scan** — skips ~87% of blocks with zero false negatives before touching the network.
- **Idempotent storage** — `INSERT OR IGNORE` everywhere, so interrupting and re-running is always safe.

---

## Storage

Database: `~/.scopenode/scopenode.db` (SQLite, WAL mode).

**Tables:**

| Table | Contents |
|---|---|
| `headers` | Block headers: number, hash, receipts_root, logs_bloom, timestamp, gas_used |
| `bloom_candidates` | Blocks that passed bloom filter per contract |
| `events` | Decoded events: contract, event_name, block, tx_hash, log_index, raw_topics, raw_data, decoded JSON |
| `sync_cursor` | Per-contract progress: headers_done_to, receipts_done_to |
| `contracts` | Contract registry + cached ABI JSON from Sourcify |

---

## Roadmap

**Phase 1 — MVP (current)**
- [x] devp2p networking (discv4 + RLPx + ETH wire)
- [x] Header sync, bloom scan, receipt fetch, Merkle verification
- [x] Sourcify ABI fetch + `alloy-dyn-abi` decoding
- [x] SQLite storage, WAL mode, resumable sync
- [x] JSON-RPC server (`eth_getLogs`, `eth_blockNumber`, `eth_chainId`)
- [ ] `status` and `query` commands

**Phase 2 — Trustless**
- [ ] Helios beacon light client for live header sync
- [ ] ERA1 archive support for historical blocks
- [ ] Proxy contract detection (EIP-1967)
- [ ] Multi-peer header agreement

**Phase 3 — Production**
- [ ] Live sync (watch new blocks)
- [ ] Reorg detection and handling
- [ ] REST API at `:8546`
- [ ] Server-Sent Events (SSE) for live streaming
- [ ] Webhooks
- [ ] CSV/JSON/Parquet export

---

## Key dependencies

| Crate | Purpose |
|---|---|
| `alloy` | Ethereum types, RLP, provider traits |
| `alloy-dyn-abi` | Runtime ABI decoding for arbitrary event logs |
| `alloy-trie` | Merkle Patricia Trie (receipt root verification) |
| `reth-network` | devp2p: discv4 discovery + RLPx transport |
| `reth-eth-wire` | ETH wire protocol messages (GetBlockHeaders, GetReceipts) |
| `sqlx` | Async SQLite with compile-time query checking |
| `jsonrpsee` | JSON-RPC 2.0 server |
| `tokio` | Async runtime |
| `clap` | CLI argument parsing |
| `indicatif` | Progress bars |
