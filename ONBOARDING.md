# scopenode Onboarding Guide

This document covers **every file** in the codebase — what it does, what
concepts it introduces, and how it connects to everything else. Read the
first three sections to understand the big picture, then use the file-by-file
section to own each piece individually.

---

## What Is This?

scopenode is a custom Ethereum node that syncs exactly the smart contract
events you care about — downloaded directly from Ethereum mainnet peers,
verified cryptographically, stored in SQLite, and served at
`localhost:8545` over standard Ethereum JSON-RPC.

It sits between a light node and a full node. A full node stores all of
Ethereum (1TB+, weeks to sync). A light node blindly trusts third parties.
scopenode is neither: you tell it which contracts and events you want, it
fetches exactly that from the P2P network, proves the data is real using
Merkle Patricia Trie verification, and makes it available immediately via
any Ethereum library — no Infura, no API keys, no rate limits.

---

## Developer Experience

**1. Write a config:**
```toml
[node]
port = 8545

[[contracts]]
name       = "Uniswap V3 ETH/USDC"
address    = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events     = ["Swap", "Mint", "Burn"]
from_block = 17000000
to_block   = 17001000
```

**2. Sync:**
```bash
scopenode sync config.toml
```

**3. Query via any Ethereum library or the CLI:**
```bash
cast logs --rpc-url http://localhost:8545 \
  --address 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8 \
  --event "Swap(address,address,int256,int256,uint160,uint128,int24)"

scopenode query --contract 0x8ad5... --event Swap
```

**Full CLI:**
```
scopenode sync config.toml      # sync + serve JSON-RPC
scopenode status                # indexed contracts, event counts
scopenode query [options]       # query from terminal (table/json)
scopenode export [options]      # export to CSV or JSON
scopenode validate config.toml  # check config before syncing
scopenode abi 0x8ad5...         # show available events
scopenode retry config.toml     # re-fetch failed blocks
scopenode doctor                # diagnose peers, DB, beacon
scopenode snapshot [--label]    # save database snapshot
scopenode restore [--label]     # restore from snapshot
```

---

## How Is It Organized?

### Architecture

```
   You / Your App
       |         \
       | CLI       | HTTP / WebSocket
       v           v
+-----------+  +---------------------+
| scopenode |  | JSON-RPC  :8545     |
|  binary   |  | REST API  :8546     |
+-----+-----+  +---------+-----------+
      |                   |
      | writes            | reads only
      v                   v
+----------------------------------------+
|  SQLite — ~/.scopenode/scopenode.db    |
|  (WAL mode, concurrent read + write)  |
+----------------------------------------+
      ^
      | pipeline writes
      |
+-----+------+
|  Pipeline  |   5 stages, per contract
+-----+------+
      |
      | devp2p wire protocol (TCP)
      v
Ethereum mainnet peers
(GetBlockHeaders + GetReceipts)
      |
      | HTTPS (once per contract, cached)
      v
Sourcify API  (fetch contract ABI)
```

### Four Rust crates (workspaces)

| Crate | Role |
|-------|------|
| `crates/scopenode` | CLI binary — parses args, routes to commands |
| `crates/scopenode-core` | All sync logic — pipeline, network, verification |
| `crates/scopenode-storage` | SQLite — every DB read/write in one place |
| `crates/scopenode-rpc` | JSON-RPC at `:8545` and REST API at `:8546` |

### SQLite tables

| Table | Contents |
|-------|---------|
| `headers` | Block headers: number, hash, `logs_bloom`, `receipts_root` |
| `bloom_candidates` | Blocks that matched bloom filter; `fetched`/`pending_retry` flags |
| `events` | Decoded events: address, name, block, tx hash, decoded JSON, `reorged` |
| `sync_cursor` | Per-contract progress: how far each pipeline stage has gotten |
| `contracts` | Registry + cached ABI JSON from Sourcify |

---

## Concepts You Must Know Before Reading Code

### Ethereum concepts

| Concept | What it is |
|---------|-----------|
| **Block** | A bundle of transactions added every ~12 seconds. Has a number, hash, and `parent_hash` chaining it to the previous block. |
| **Block header** | ~500-byte metadata for a block. Contains `logs_bloom` and `receipts_root`. scopenode downloads headers for ALL blocks, full data only for candidates. |
| **Transaction receipt** | The result of one transaction: success/fail, gas used, and every log (event) it emitted. |
| **Log / Event** | When a contract calls `emit Swap(...)`, a log appears in the receipt. Has an address, `topics`, and `data`. |
| **`logs_bloom`** | A 2048-bit Bloom filter in every header. Fingerprints every address and topic that emitted a log in the block. Used to skip ~87% of blocks without network calls. |
| **`receipts_root`** | A 32-byte Merkle root in every header that commits to all receipts. Recomputing the Merkle trie from received receipts and matching this root proves authenticity. |
| **Merkle Patricia Trie** | A hash tree where recomputing the root proves a set of values hasn't been tampered with. Ethereum commits receipts, transactions, and state into tries. |
| **devp2p** | Ethereum's peer-to-peer network. Peers discover each other via `discv4` (UDP Kademlia DHT) and exchange data over RLPx (TCP, ECIES-encrypted). `reth-network` handles this. |
| **ETH wire protocol** | Application-layer protocol within devp2p. scopenode uses two messages: `GetBlockHeaders` and `GetReceipts`. |
| **ABI** | Application Binary Interface — the schema for a smart contract. Defines each event's name, params, types. Fetched from Sourcify (a public ABI registry). |
| **topic0** | `keccak256("EventName(type1,type2,...)")` — the first topic in every log. Identifies which event type a log is. scopenode checks bloom filters for `(address, topic0)` pairs. |
| **Snap sync** | Most mainnet peers only store recent state, not historical receipts. scopenode must find an archive peer that has old receipt data. |
| **Reorg** | When the canonical chain switches to a longer fork, old blocks become orphaned. Events from those blocks are marked `reorged = 1` (never deleted). |
| **Beacon chain** | Ethereum's proof-of-stake consensus layer. After the Merge, it finalizes blocks every ~12.8 minutes. Helios is a light client that can verify beacon headers without trusting anyone. |
| **EIP-55** | Checksum encoding for Ethereum addresses (mixed case). scopenode uses checksum addresses as SQLite primary keys. |
| **RLP** | Recursive Length Prefix — Ethereum's binary serialization format. Receipts are RLP-encoded before building the Merkle trie. |

### Rust concepts

| Concept | Where you'll see it |
|---------|-------------------|
| **`async`/`await`** | Everything in the pipeline, DB, and network is async. `tokio` drives it all. `async fn` returns a `Future`; `.await` suspends until it resolves. |
| **`Arc<T>`** | Atomic Reference Count — cheap shared ownership across threads. `Db`, `DevP2PNetwork`, the peer map are all `Arc`-wrapped so multiple tasks can hold them. |
| **`Arc<RwLock<T>>`** | Shared mutable state. The peer map in `network.rs` uses this: the background event listener writes it, request tasks read it. `RwLock` allows multiple concurrent readers OR one writer. |
| **Traits** | Rust's interfaces. `EthNetwork` is a trait — any type implementing it works in `Pipeline<N>`. This enables `MockNetwork` in tests without changing pipeline code. |
| **`#[async_trait]`** | Rust's native `async fn` in traits has restrictions. The `async_trait` crate macro works around this, adding a `Box<dyn Future>` wrapper behind the scenes. |
| **`Result<T, E>` and `?`** | Rust's error handling. `?` returns `Err` to the caller if the value is `Err`. Every fallible function returns `Result`. |
| **`thiserror` / `anyhow`** | `thiserror` creates structured error enums in library crates. `anyhow` is used in the binary for ergonomic error messages with context. |
| **`#[derive(Clone)]`** | Automatically implements the `Clone` trait (copying a value). `Db` is `Clone` because `SqlitePool` is internally `Arc`-wrapped — cloning is cheap. |
| **`#[derive(sqlx::FromRow)]`** | Tells `sqlx` how to map a SQLite row into a Rust struct automatically. Used on every model struct in `models.rs`. |
| **`VecDeque`** | A double-ended queue used in `reorg.rs` as a sliding window. Efficient push to back and pop from front. |
| **`broadcast::channel`** | A tokio channel where one sender can have many receivers. Used for live events (`eth_subscribe`) and new block headers. |
| **`INSERT OR IGNORE`** | SQLite idiom: if the row's unique key already exists, the insert silently does nothing. Makes every stage safe to re-run without duplication. |
| **Lifetime `'a`** | Rust's borrow checker annotation. `DbStream<'a, T>` in `db.rs` means the stream borrows data for lifetime `'a`. Rarely needs to be written manually in new code here. |

---

## Primary Flows

### Sync flow (`scopenode sync config.toml`)

```
main.rs: parse CLI, open Db, resolve data_dir
  |
  v
commands/sync.rs: run()
  apply --blocks override if present
  DevP2PNetwork::start(data_dir)
    load or create node.key (stable peer identity)
    boot NetworkManager (discv4 UDP + RLPx TCP)
    spawn TransactionsManager (tx gossip relay)
    wait indefinitely for ≥1 ETH-handshaked peer
  |
  v
Pipeline::run(dry_run, progress)  [pipeline.rs]
  For each contract in config:
    Stage 1: ABI
      AbiCache::get_or_fetch()
        check contracts table → if cached, done
        else: GET Sourcify API
        parse event sigs, compute topic0 hashes
        cache in SQLite contracts table
    Stage 2: Header sync
      network.get_headers(from, to)
        GetBlockHeaders → devp2p peer
        batches of 64 (ETH wire limit)
        retry up to 5×, 60s between tries
      db.insert_header() per header
      db.upsert_sync_cursor() per batch
    Stage 3: Bloom scan  (CPU only, no network)
      load all headers from SQLite
      headers.rs: BloomScanner::matches()
        check logs_bloom for (address, topic0)
      db.insert_bloom_candidate() for hits
      db.upsert_sync_cursor() when done
    Stage 4: Receipt fetch + verify + decode + store
      For each candidate (batches of 16):
        network.get_receipts_for_blocks()
          GetReceipts → devp2p peers
          loop until archive peer responds
        receipts.rs: verify_receipts()
          rebuild Merkle Patricia Trie
          assert computed root == receipts_root
        abi.rs: EventDecoder::extract_and_decode()
          filter logs by address + topic0
          decode raw bytes into named JSON
        db.insert_events() — INSERT OR IGNORE
        db.mark_fetched()
  |
  v
start_server(:8545) — JSON-RPC stays running
start_rest_server(:8546) — REST stays running
  |
  v
LiveSyncer::run() — if any contract has no to_block
  poll every 6s for new blocks
  for each new block:
    fetch header → reorg check → bloom scan
    if hit: fetch receipts → verify → decode → store
    broadcast event to eth_subscribe subscribers
```

### Query flow (`eth_getLogs` or `scopenode query`)

```
Client: eth_getLogs request
  |
  v
server.rs: get_logs(filter)
  check filter.address is in contracts table
  if not: return "not indexed" error
  |
  v
db.query_events_for_filter(
  contract, topic0, from_block, to_block, limit
)
  SQL: SELECT ... FROM events
       WHERE reorged = 0
       AND contract = ? AND topic0 = ?
       AND block_number BETWEEN ? AND ?
  |
  v
rows → convert to alloy Log types → JSON response
```

---

## Every File Explained

### `crates/scopenode/` — The CLI Binary

---

#### `src/main.rs`

The entry point for the `scopenode` binary.

**What it does:** Parses CLI arguments using `clap`, sets up structured logging
(`tracing`), opens the SQLite database, resolves the data directory, then calls
the appropriate command handler.

**Key patterns:**
- `#[tokio::main]` — marks this as the async entry point. `tokio` spawns a
  multi-threaded async runtime that drives all `async fn` calls in the program.
- `Cli::parse()` — `clap` reads `std::env::args()` and fills the `Cli` struct
  automatically based on the `#[arg(...)]` annotations in `cli.rs`.
- `EnvFilter` — the log level can be set via `RUST_LOG=debug` or via `-v`/`-vv`
  flags. `EnvFilter::try_from_default_env()` reads `RUST_LOG`; if absent, falls
  back to the flag level.
- `match &cli.command { ... }` — dispatches to the right `commands/*.rs` handler.
- `resolve_data_dir` / `expand_tilde` — helper functions to apply data directory
  priority rules and expand `~/` to the home directory.

**Rust concept:** `#[tokio::main]` is a proc-macro that rewrites `main()` into
code that creates a `tokio::runtime::Runtime` and calls `.block_on(async_main())`.

---

#### `src/cli.rs`

Defines all CLI commands, flags, and arguments using `clap v4`.

**What it does:** Declares the `Cli` struct (global flags: `--data-dir`,
`--verbose`, `--quiet`) and the `Command` enum (one variant per subcommand with
their own flags).

**Key patterns:**
- `#[derive(Parser)]` on `Cli` — `clap` generates all argument parsing logic
  from the struct fields and their `#[arg(...)]` annotations.
- `#[command(subcommand)]` on the `command` field — tells `clap` this field
  holds a subcommand.
- `#[derive(Subcommand)]` on `Command` — each variant maps to a subcommand name.
- `ArgAction::Count` on `--verbose` — allows the flag to be repeated (`-v`,
  `-vv`, `-vvv`) and counts the repetitions as a `u8`.
- `env = "SCOPENODE_DATA_DIR"` — `clap` reads the env var as a fallback if the
  flag isn't provided.

**Blockchain concept:** The `--blocks` flag on `sync` accepts human-readable
shorthand like `"16M:17M"` — parsed by `parse_blocks_flag` in `commands/sync.rs`.

---

#### `src/commands/mod.rs`

Just `pub mod` declarations for each command file. Rust requires explicit module
declarations; this file tells the compiler that `sync`, `status`, `query`, etc.
are submodules of `commands`.

---

#### `src/commands/sync.rs`

The most complex command handler — orchestrates the entire sync lifecycle.

**What it does:**
1. Applies `--blocks` range override to all contracts if provided.
2. Shows a spinner while devp2p connects.
3. Boots `DevP2PNetwork`.
4. Starts a `peer_count` background task that polls peer count every 5s and
   updates an `AtomicUsize` (used by `net_peerCount` in the RPC server).
5. Runs `Pipeline::run()` with progress bars.
6. Starts the JSON-RPC server (`:8545`) and REST server (`:8546`).
7. If any contract has `to_block = None`, starts `LiveSyncer::run()`.

**Key patterns:**
- `Arc<AtomicUsize>` for `peer_count` — an atomic integer that can be safely
  updated from one task and read from another without locks.
- `broadcast::channel` — one sender, many receivers. Events produced by live
  sync are broadcast to all active `eth_subscribe` connections simultaneously.
- `tokio::select!` — waits for multiple futures concurrently; returns when the
  first one completes. Used to race the JSON-RPC server handle against the live
  sync loop so either can stop the program.
- `parse_blocks_flag` — parses `"16M:17M"` or `"16M:+1000"` into `(u64, Option<u64>)`.

---

#### `src/commands/status.rs`

Displays a table of indexed contracts with event counts and sync progress.

**What it does:** Queries `db.contracts_with_event_counts()` (one SQL JOIN) and
`db.get_sync_cursor()` per contract, then prints a formatted table using
`indicatif`-style output.

---

#### `src/commands/query.rs`

Terminal query with table or JSON output.

**What it does:** Calls `db.query_events(...)` with the provided filters, then
prints results either as an ASCII table (default) or as JSON. Respects `--limit`.

---

#### `src/commands/export.rs`

Streams all matching events to stdout as CSV or JSON with no row cap.

**What it does:** Uses `db.stream_events_for_filter()` (a lazy SQL stream —
rows are yielded one at a time without buffering the entire result set into
memory). This is important for large exports that might be millions of rows.

**Rust concept:** `futures_core::Stream` is the async equivalent of `Iterator`.
Instead of `next()`, you `.await` the next item. `StreamExt` adds helper methods
like `.for_each()`.

---

#### `src/commands/validate.rs`

Checks a config file before committing to a sync.

**What it does:** Loads the config (which already validates field types via
`serde`), then checks that each contract address is reachable on Sourcify and
that the requested event names exist in the ABI. Reports issues before the user
starts a multi-hour sync.

---

#### `src/commands/abi.rs`

Fetches and pretty-prints a contract's available events from Sourcify.

**What it does:** Calls `SourcifyClient::fetch_events()` directly (no DB
involved) and prints event names with their parameter types. Useful for finding
the exact event names to put in your config.

---

#### `src/commands/retry.rs`

Re-fetches all blocks that were marked `pending_retry = 1`.

**What it does:** Boots `DevP2PNetwork`, then calls `Pipeline::run_retry()` which
skips header and bloom stages and only retries the receipt-fetch+verify+store
step for blocks that previously failed. Called after `scopenode retry config.toml`.

---

#### `src/commands/doctor.rs`

Health check: devp2p connectivity, beacon status, DB integrity, retry queue.

**What it does:** Checks the peer count, reads `beacon_status.json` from the
data directory, calls `PRAGMA integrity_check` on the SQLite DB, and counts
`pending_retry` blocks. Prints a summary. Does not require a running sync.

---

#### `src/commands/snapshot.rs`

Copies the SQLite database to a snapshot file in the data directory.

**What it does:** Copies `scopenode.db` to `snapshots/<label>.db` (or a
timestamp if no label is given). Uses `std::fs::copy`. Also copies the WAL
file if present.

---

#### `src/commands/restore.rs`

Restores the database from a previously saved snapshot.

**What it does:** Lists available snapshots (files in `snapshots/`) and copies
the chosen one over `scopenode.db`. Warns the user that the current DB will be
overwritten.

---

### `crates/scopenode-core/` — Pipeline and Networking

---

#### `src/lib.rs`

The crate root. Just `pub mod` declarations and a module-level doc comment
explaining what each module does. Read this first in the crate — it gives you
the overview before diving in.

---

#### `src/config.rs`

TOML configuration types — the schema for `config.toml`.

**What it does:** Defines `Config` (root), `NodeConfig` (port, data dir, consensus
RPCs, reorg buffer), and `ContractConfig` (address, events, block range, ABI
override). Parsing is automatic via `#[derive(Deserialize)]` from `serde`.

**Key patterns:**
- `#[serde(deny_unknown_fields)]` — TOML keys that don't match any field cause a
  parse error immediately. Catches typos like `form_block` instead of `from_block`.
- `#[serde(deserialize_with = "deser_block_number")]` — runs a custom deserializer
  that accepts both plain integers (`17000000`) and shorthand strings (`"17M"`).
- Custom `serde::de::Visitor` — `BlockVisitor` and `OptBlockVisitor` implement the
  Rust visitor pattern, telling serde how to convert each possible JSON/TOML type
  (integer, string) into a `u64`. This is the correct way to handle multi-type fields.
- `Config::validate()` — validates logical constraints (e.g. `from_block <= to_block`)
  that can't be expressed in the type system alone.

---

#### `src/types.rs`

Shared data types: `ScopeHeader`, `StoredEvent`, `BloomTarget`, `ContractStatus`.

**What it does:** Defines the core data structures that flow between pipeline
stages. These are "core types" using Rust's `u64`, `Address`, `B256`, `Bloom`
directly — the storage layer has separate "SQL types" in `models.rs` that use
`i64` and `String`.

**Key types:**
- `ScopeHeader` — a minimal Ethereum block header (just the fields scopenode
  needs: number, hash, parent_hash, timestamp, `receipts_root`, `logs_bloom`,
  gas_used, `base_fee_per_gas`). Full alloy headers have many more fields.
- `StoredEvent` — a fully decoded event ready for SQLite. Contains both raw
  on-chain data (`raw_topics`, `raw_data`) and decoded JSON (`decoded`). Both
  are stored so you can re-decode if the ABI changes.
- `BloomTarget` — precomputed `(address_bytes, topic_bytes)` pair for checking
  bloom filters. Built once per sync run from the ABI.

---

#### `src/error.rs`

Error type hierarchy for the core crate.

**What it does:** Defines `CoreError` (top-level), `NetworkError`, `AbiError`,
`VerifyError`, and `ConfigError`. Each has variants with human-readable messages
using `#[error("...")]` from `thiserror`.

**Key pattern — error hierarchy with `#[from]`:**
```
CoreError::Network(NetworkError)
CoreError::Abi(AbiError)
CoreError::Verify(VerifyError)
CoreError::Config(ConfigError)
CoreError::Storage(DbError)
```
`#[from] NetworkError` on the `Network` variant means: if you have a
`NetworkError`, the `?` operator automatically wraps it in `CoreError::Network`.
This is how `?` works across error type boundaries without `.map_err()`.

**Blockchain concept in error messages:** `VerifyError::RootMismatch` includes
both the `expected` root (from the header we trust) and the `computed` root
(from the peer's receipt data). If they differ, the peer sent tampered data.

---

#### `src/pipeline.rs`

The 4-stage sync orchestrator. The heart of scopenode.

**What it does:** `Pipeline<N: EthNetwork>` iterates over every contract in the
config and runs four stages in order: ABI → headers → bloom → receipts. All
stages are resumable via `sync_cursor`. A `MockNetwork` in the test module shows
exactly how to test pipeline logic without a real network.

**Key patterns:**
- `Pipeline<N: EthNetwork>` — generic over the network transport. In production,
  `N = DevP2PNetwork`. In tests, `N = MockNetwork`. The compiler generates a
  separate compiled version for each concrete type (monomorphization).
- `MultiProgress` + `ProgressBar` from `indicatif` — progress bars shown in the
  terminal during sync. Each stage gets its own bar.
- Resumability: `cursor.headers_done_to.map(|n| n + 1).unwrap_or(from)` — if
  we've already synced to block `n`, start from `n+1`. If `None`, start from
  the configured `from_block`.
- `scope_header_to_stored` / `core_to_storage_event` — conversion functions
  from "core types" (`u64`, `Address`, `B256`) to "storage types" (`i64`,
  `String`) because SQLite doesn't support Ethereum types directly.

---

#### `src/network.rs`

devp2p network transport — the most complex file in the codebase.

**What it does:** Defines the `EthNetwork` trait (the pipeline's interface to
the network) and `DevP2PNetwork` (the real implementation using reth's devp2p
stack). Handles peer discovery, connection management, request routing,
blacklisting, and receipt conversion.

**Read the module-level doc comment first** — it walks through the entire boot
sequence step by step.

**Key components:**

*`EthNetwork` trait* — three methods:
- `get_headers(from, to)` — returns `Vec<ScopeHeader>` for a block range
- `get_receipts_for_blocks(blocks)` — returns one `ReceiptFetchResult` per block
- `best_block_number()` — returns the chain tip from connected peers

*`DevP2PNetwork::start()`* — boots the full devp2p stack:
1. Loads or creates a persistent `node.key` (secp256k1 key) so the Kademlia DHT
   remembers us across restarts
2. Builds `NetworkConfig` with mainnet bootnodes and a fake current head
   (so our `fork_id` matches current mainnet and peers accept us)
3. Spawns `NetworkManager` as a tokio task (runs forever)
4. Wires up `TransactionsManager` so we participate in tx gossip
   (without this, peers see us as non-participating and disconnect)
5. Spawns a background task that listens to `NetworkEvent`s and maintains the
   `peers` HashMap
6. Calls `wait_for_peers()` which loops indefinitely until ≥1 peer completes
   the ETH Status handshake

*`get_receipts_for_blocks()`* — the most important method:
- Loops indefinitely until it finds an archive peer with historical receipts
- Snap-synced peers return empty responses (they don't have old receipt data)
- MPT verification is done here too (before returning to the pipeline)
- Bad peers are blacklisted; snap-synced peers go into a per-call `tried` set
  (eligible again for the next batch)

*`build_alloy_receipts()`* — converts reth's wire receipt format to alloy's RPC
receipt format. The wire protocol doesn't include `tx_hash` or `log_index`, so
these are synthesized deterministically:
- `tx_hash = keccak256(block_hash || tx_index_as_bytes)` — unique per (block, tx)
- `log_index` = cumulative across all transactions in the block

*`WireReceipt` trait* — a tiny adapter trait so `build_alloy_receipts` can be
generic over reth's slightly different wire receipt types.

---

#### `src/headers.rs`

Bloom filter scanning — the stage that skips ~87% of blocks.

**What it does:** `BloomScanner` has two static methods: `build_targets()` which
precomputes `(address, topic0)` pairs, and `matches()` which checks whether a
block's 2048-bit bloom filter contains both the contract address AND at least
one topic0.

**Key concept — why two checks?**
The bloom filter is checked for address first, then for topic0. Both must be
present. Address alone could be a false positive from the contract being involved
in a *different* event we don't care about. Requiring both address AND topic0
means we only fetch receipts for blocks where our specific event *might* have
been emitted.

**Key concept — zero false negatives:**
Bloom filters guarantee: if the event was emitted in this block, both the address
AND the topic0 are guaranteed to be set in the bloom. The ~15% false positive
rate means some blocks are fetched and found empty — acceptable overhead.

---

#### `src/receipts.rs`

Merkle Patricia Trie verification — the core trust mechanism.

**What it does:** `verify_receipts()` takes a list of receipts received from a
peer, rebuilds the Merkle Patricia Trie, computes its root, and asserts it
matches `receipts_root` from the block header.

**Algorithm step by step:**
1. For each receipt: `key = rlp_encode_index(tx_index)`, `value = eip2718_encode(receipt)`
2. Sort `(key, value)` pairs lexicographically by key
3. Feed sorted pairs into `HashBuilder` (from `alloy-trie`)
4. Get the computed root
5. Compare to `header.receipts_root`

**Why this proves authenticity:** The header's `receipts_root` was set by the
block's creator. A peer cannot change it without also changing the block hash,
which requires redoing all the PoS work for that and every subsequent block.
So if our receipts reproduce the same root, they must be the real receipts.

**Key concept — RLP encoding:**
`rlp_encode_index(0)` = `[0x80]` (special case for 0)
`rlp_encode_index(5)` = `[0x05]` (single byte for 1-127)
`rlp_encode_index(130)` = `[0x81, 0x82]` (length-prefixed for ≥128)
This is a hand-coded subset of Ethereum's RLP spec for integer encoding.

---

#### `src/abi.rs`

ABI fetching, caching, and event log decoding.

**What it does:** Three main things:
1. `SourcifyClient` — HTTP client that fetches contract metadata from
   `sourcify.dev` (Ethereum Foundation's ABI registry, no API key required)
2. `AbiCache` — wraps `SourcifyClient` with SQLite caching so we don't re-fetch
   on every `scopenode sync` run
3. `EventDecoder` — takes a list of `EventAbi` and decodes matching logs

**ABI concepts:**
- `EventAbi` — one Solidity event: name + list of `EventInput` params
- `EventInput` — one parameter: name, type (`"address"`, `"uint256"`, etc.),
  `indexed` flag, and nested `components` for tuple types
- `EventAbi::signature()` — builds the canonical form: `"Swap(address,address,int256,...)"`.
  For tuples, `canonical_type()` expands `"tuple"` → `"(type1,type2,...)"`.
- `EventAbi::topic0()` — `keccak256(signature.as_bytes())`. This is the first
  topic in every log of this event type.

**Decoding pipeline:**
- Indexed params are in `topics[1..]` — one per topic, 32 bytes each
- Non-indexed params are ABI-encoded as a packed tuple in the `data` field
- `decode_indexed_param()` handles each type: addresses are right-aligned in
  the 32-byte topic, integers are big-endian, reference types (arrays, strings)
  are hashed (not recoverable — stored as the raw hash)
- `decode_abi_data()` uses `alloy-dyn-abi` to parse the full data field at once

**Why U256 is stored as decimal strings:** JavaScript's `Number` type has 53-bit
precision. `uint256` can hold values up to ~1.16 × 10^77 — way beyond what `Number`
can represent exactly. Decimal strings preserve the full value.

---

#### `src/live.rs`

Live sync — continuous tail of the chain tip after historical sync completes.

**What it does:** `LiveSyncer<N>` polls `best_block_number()` every 6 seconds.
For each new block it: verifies against Helios (if configured), stores the
header, runs the reorg detector, bloom-scans against live contracts, and if
there's a hit, fetches receipts, verifies, decodes, stores, and broadcasts
events on the `broadcast` channel (which feeds `eth_subscribe` subscribers).

**Key concepts:**
- "Live contracts" = contracts with `to_block = None` in the config
- Helios verification is optional — if `consensus_rpc` is empty, live headers
  are trusted from devp2p peers (less secure but still Merkle-verified)
- `reorg_detector.seed()` on startup loads the last 64 stored headers from
  SQLite so the detector can catch reorgs that started before we came back up
- `broadcast::Sender<StoredEvent>` — every decoded live event is sent here;
  `eth_subscribe "logs"` receivers get it immediately

---

#### `src/reorg.rs`

Chain reorganization detection using a rolling window.

**What it does:** `ReorgDetector` maintains a `VecDeque` (sliding window) of
`(block_number, block_hash)` pairs for the last `reorg_buffer` blocks. On each
new block, it checks whether `new_block.parent_hash` equals the current tip's
hash. If not, a reorg has occurred.

**Reorg algorithm:**
1. Walk backward through the window to find the entry whose hash equals
   `new_block.parent_hash` — that's the common ancestor
2. All entries after it are orphaned block hashes
3. Return `ReorgEvent { common_ancestor, orphaned_hashes }`
4. Caller passes `orphaned_hashes` to `db.mark_reorged_by_hash()` to soft-delete

**Why 64 blocks?** Post-Merge, the beacon chain finalizes blocks after ~2 epochs
(~12.8 minutes, ~64 blocks). A finalized block cannot be reorged without breaking
BLS-512 consensus, which is cryptographically infeasible.

**Rust concept:** `VecDeque` supports O(1) push to the back and pop from the
front — exactly the sliding window behavior needed here. A regular `Vec` would
require O(n) shifts for front removal.

---

#### `src/beacon.rs`

Beacon light client status and consensus pre-flight check.

**What it does:** Two things:
1. `BeaconStatus` enum — tracks the state of the Helios client (`NotConfigured`,
   `Syncing`, `Synced`, `Stalled`, `Error`, `FallbackUnverified`). Shared
   in-process via `BeaconStatusTx` (a `tokio::sync::watch` channel — one sender,
   multiple readers, always holds the latest value).
2. `run_consensus_preflight()` — before starting Helios, checks that all
   configured `consensus_rpc` endpoints agree on the latest block hash. If two
   endpoints report different hashes at the same slot, something is wrong.

**Why two status types?** `BeaconStatus` uses `Instant` (for elapsed time
computation) which isn't serializable. `BeaconStatusFile` is the JSON-serializable
version written to `beacon_status.json` so `scopenode doctor` (a separate process)
can read it without the sync process being alive.

**Beacon concepts:**
- A "slot" is one 12-second window in the beacon chain. Blocks are produced per slot.
- The execution payload's `block_hash` is the Ethereum block hash. This is what
  Helios verifies against what devp2p peers serve.

---

#### `src/helios_client.rs`

Lifecycle wrapper around the Helios beacon light client.

**What it does:** `HeliosGuard` wraps a `helios_ethereum::EthereumClient` and
provides `verify_block()` — the per-block verification logic. Returns `None` from
`start()` when `consensus_rpc` is empty (unverified mode).

**Verification decisions (`VerifyDecision`):**
- `Accept` — Helios agrees with the peer's block hash; process the block
- `Discard` — hash mismatch, retry on next poll tick
- `Halt` — five consecutive errors; live sync stops

**`HeliosHead` trait** — a thin seam around the real Helios client so tests can
provide a mock without actually bootstrapping the Helios consensus client.

---

### `crates/scopenode-storage/` — SQLite Layer

---

#### `src/lib.rs`

Crate root. Just `pub mod` declarations and re-exports so callers can write
`scopenode_storage::Db` instead of `scopenode_storage::db::Db`.

---

#### `src/types.rs`

Higher-level types: `SyncCursor` with `u64` (Rust-idiomatic).

**What it does:** `SyncCursor` tracks per-contract pipeline progress with
`Option<u64>` for each stage. `None` means that stage hasn't started yet.

The doc comment example is worth reading:
```
headers_done_to  = Some(13_000_000)   ← synced all headers
bloom_done_to    = Some(12_500_000)   ← interrupted mid-bloom
receipts_done_to = None               ← not started
```
Next run: skip headers, resume bloom from 12,500,001.

---

#### `src/models.rs`

SQLite row types — direct DB representations using `i64` and `String`.

**What it does:** Defines `StoredHeader`, `StoredEvent`, `ContractRow`, and
`SyncCursorRow`. These are what `sqlx` reads from and writes to the database.

**Why `i64` instead of `u64`?** SQLite's INTEGER type is a signed 64-bit int.
`sqlx` will reject `u64` at compile time. The casts (`block_num as i64`,
`row.number as u64`) happen in `db.rs`. Safe in practice — Ethereum block
numbers are currently ~20M, nowhere near `i64::MAX` (~9.2 × 10^18).

**`#[derive(sqlx::FromRow)]`** — tells sqlx to map a DB row to this struct by
matching column names to field names. The column name must exactly match the
field name (or be aliased in the SQL query).

---

#### `src/error.rs`

`DbError` — the single error type for all database operations.

**What it does:** Three variants: `Open` (can't open file), `Migration` (schema
migration failed), `Query` (SQL execution failed). All wrapped as a string for
simplicity since the sqlx error types are opaque.

---

#### `src/db.rs`

Every database operation in one file — the most important file in the storage crate.

**What it does:** `Db` is a `Clone`-safe handle to the SQLite connection pool.
Every method is an `async fn` that executes one SQL query and returns a `Result`.

**Key methods by category:**

*Headers:*
- `insert_header()` — `INSERT OR IGNORE INTO headers`
- `get_headers(from, to)` — returns all headers in range with bloom pre-parsed
- `latest_block_number()` — `SELECT MAX(number)` for `eth_blockNumber`

*Bloom candidates:*
- `insert_bloom_candidate()` — record a bloom hit
- `get_fetched_set()` — load all already-fetched block numbers in one query
  (avoids N individual `is_fetched` calls — important for large resume scenarios)
- `mark_fetched()` / `mark_retry()` — update flags after processing

*Events:*
- `insert_events()` — bulk insert inside a single transaction (dramatically
  faster than N individual transactions — SQLite flushes to disk per-commit)
- `query_events_for_filter()` — `SELECT ... WHERE reorged = 0 AND ...`
- `stream_events_for_filter()` — same as above but returns a lazy stream for
  large exports without buffering everything in memory

*Sync cursor:*
- `upsert_sync_cursor()` — `ON CONFLICT DO UPDATE` with `CASE WHEN` to never
  go backwards: `headers_done_to = CASE WHEN new IS NOT NULL THEN new ELSE old`
- `get_sync_cursor()` — load current progress for a contract

*Reorg:*
- `mark_reorged_by_hash(block_hashes)` — `UPDATE events SET reorged = 1 WHERE
  block_hash IN (...)`. Soft-delete — never hard-delete blockchain data.

**`bloom_from_hex()`** — parses the 512-char hex string back into a `Bloom`
type. Returns the zero bloom on corrupt data (safe: all bloom checks fail, no
false matches).

**`DbStream`** — wraps a `sqlx` stream to convert `sqlx::Error` into `DbError`.
The `Box::leak()` in `stream_events_for_filter` produces a `'static` reference
to the SQL string so the stream's lifetime doesn't borrow a local variable.

---

#### `src/migrations/001_init.sql`

Creates all five tables: `headers`, `bloom_candidates`, `events`, `sync_cursor`,
`contracts`. Run automatically by `sqlx::migrate!` when `Db::open()` is called.

---

#### `src/migrations/002_bloom_hex.sql`

Migrates the `logs_bloom` column from binary BLOB to a hex TEXT string. Done for
human readability when inspecting the DB with external tools like DB Browser for SQLite.

---

#### `src/migrations/003_schema_hardening.sql`

Adds constraints, indexes, and possibly new columns added after the initial schema.
Read this file to see what schema evolution has happened.

---

### `crates/scopenode-rpc/` — JSON-RPC and REST Servers

---

#### `src/lib.rs`

Crate root. Starts the JSON-RPC server via `jsonrpsee`. Merges the `eth_*` and
`net_*` method namespaces into one server (jsonrpsee uses a module system where
each proc-macro-generated impl produces an `RpcModule`; they're merged with
`.merge()`).

---

#### `src/server.rs`

Implements the Ethereum JSON-RPC methods.

**What it does:** `EthApiImpl` holds a `Db` handle and two `broadcast::Sender`
channels. The `#[rpc(server, namespace = "eth")]` proc macro generates the
dispatch glue from method names to handler functions.

**Implemented methods:**
- `eth_getLogs` — checks that the requested address is indexed, queries events,
  converts rows to alloy `Log` types, returns JSON
- `eth_blockNumber` — `SELECT MAX(number) FROM headers`
- `eth_chainId` — always `"0x1"` (mainnet)
- `eth_subscribe "logs"` — subscribes to the broadcast channel; filters by
  optional address and topic0; pushes each matching event as JSON over WebSocket
- `eth_subscribe "newHeads"` — subscribes to the headers broadcast; deduplicates
  by block number; pushes `{blockNumber, blockHash, timestamp}` per block
- `net_peerCount` — reads `AtomicUsize` set by the sync loop background task

**Key pattern — WebSocket subscriptions:** `PendingSubscriptionSink::accept()`
accepts the WebSocket subscription. The loop then `recv()` from the broadcast
channel and `send_timeout()` to the subscriber. If the subscriber is too slow
(`RecvError::Lagged`), events are dropped but the connection stays open.

---

#### `src/rest.rs`

REST API at `:8546` using `axum` + SSE for live streaming.

**What it does:** Defines five routes:
- `GET /events` — paginated query with optional filters (contract, event, topic0,
  block range, limit, offset)
- `GET /status` — block count, contract count, total events
- `GET /contracts` — all indexed contracts with event counts
- `GET /abi/:address` — raw ABI JSON for a contract
- `GET /stream/events` — SSE stream of live events (wraps the broadcast channel
  in a `tokio_stream::BroadcastStream`)

**Key pattern — SSE:** `Sse::new(stream).keep_alive(KeepAlive::default())` wraps
any async stream as a Server-Sent Events response. `axum` handles the HTTP
headers (`Content-Type: text/event-stream`). The stream is the live broadcast
channel converted to a stream, filtered by the optional address/event query params.

**Key pattern — shared state:** `AppState { db, broadcast }` is wrapped in `axum`'s
`State` extractor. `axum` clones it for each request (cheap — `Db` and
`broadcast::Sender` are both `Arc`-backed).

---

### Tests and Examples

---

#### `crates/scopenode-core/tests/fixture_verify.rs`

Integration test that fetches real Ethereum receipts and verifies them.

**What it does:** Downloads actual receipts for a specific block from a public
RPC endpoint, runs `verify_receipts()` against the real `receipts_root` from
the header, and asserts the result is `Ok`. This proves the Merkle verification
logic works against real mainnet data (not just constructed test data).

Skipped automatically when the network is unavailable (the test uses
`#[ignore]` or checks for an env var).

---

#### `crates/scopenode-core/examples/fetch_fixtures.rs`

A runnable example that downloads real block data and saves it to disk as test
fixtures. Run with `cargo run --example fetch_fixtures`. Used to generate the
data files that `fixture_verify.rs` tests against.

---

#### `crates/scopenode-rpc/tests/parity.rs`

Parity test that runs both the JSON-RPC server and the REST API against a real
(test) database and verifies that the same events are returned by both.

Ensures the two server implementations agree — a regression here would mean
`viem` using `eth_getLogs` gets different data than Python using `GET /events`.

---

## Developer Guide

### Setup

Prerequisites: Rust 1.80+ (install from [rustup.rs](https://rustup.rs)).

```bash
git clone <repo>
cd scopenode
cargo build --release
# binary: target/release/scopenode
```

The first build takes several minutes (reth + helios are large). Later builds
are incremental.

### Running

```bash
cp config.example.toml config.toml
# edit config.toml: set address, events, from_block

./target/release/scopenode sync config.toml
./target/release/scopenode status
./target/release/scopenode query --event Swap --limit 5
```

### Testing

```bash
cargo test                        # all crates
cargo test -p scopenode-core      # just core
cargo test -p scopenode-storage   # just storage
cargo test -p scopenode-rpc       # just RPC
```

All tests use temporary SQLite databases (`tempfile`) and `MockNetwork` —
no real network connection needed.

### Adding a new CLI command

1. Add a variant to `Command` in `crates/scopenode/src/cli.rs`
2. Create `crates/scopenode/src/commands/<name>.rs`
3. Declare it in `commands/mod.rs` with `pub mod <name>;`
4. Wire it up in the `match &cli.command` block in `main.rs`

### Adding a new DB query

Add a method to `impl Db` in `crates/scopenode-storage/src/db.rs`. Use
`sqlx::query_as::<_, YourReturnType>(...)` if you want automatic struct
mapping, or `sqlx::query(...)` for operations that don't return rows.

### Adding a new JSON-RPC method

1. Add it to the `EthApi` or `NetApi` trait in `server.rs` with `#[method(name = "...")]`
2. Implement it in `EthApiServer for EthApiImpl`

### Recommended reading order (from zero)

Start here if you've never read Rust or Ethereum code before:

1. **`VISION.md`** — plain English explanation of every pipeline stage and why
   it exists. No code. Read this before anything else.
2. **`crates/scopenode/src/main.rs`** — 230 lines. See the top-level structure.
3. **`crates/scopenode/src/cli.rs`** — 120 lines. See `clap` derive patterns.
4. **`crates/scopenode-core/src/types.rs`** — 160 lines. Understand the data
   types that flow through everything.
5. **`crates/scopenode-core/src/error.rs`** — 181 lines. Understand error
   hierarchy and how `thiserror` + `#[from]` work.
6. **`crates/scopenode-core/src/config.rs`** — 559 lines. See serde patterns,
   custom deserializers, and `deny_unknown_fields`.
7. **`crates/scopenode-core/src/headers.rs`** — 139 lines. The bloom filter
   check is ~10 lines of logic. Understand it fully.
8. **`crates/scopenode-core/src/receipts.rs`** — 217 lines. Understand Merkle
   verification fully. Read the module doc comment first.
9. **`crates/scopenode-core/src/abi.rs`** — 826 lines. The ABI fetch, cache, and
   decode logic. Read the module doc comment first.
10. **`crates/scopenode-storage/src/models.rs`** — 153 lines. SQLite row types.
    Note the `i64` vs `u64` patterns.
11. **`crates/scopenode-storage/src/db.rs`** — 1039 lines. Every DB operation.
    Take your time here — this is used by everything else.
12. **`crates/scopenode-core/src/pipeline.rs`** — 783 lines. The full 4-stage
    loop. Read every comment. Look at `MockNetwork` in the test module to
    understand how to test pipeline logic.
13. **`crates/scopenode-rpc/src/server.rs`** — 474 lines. JSON-RPC implementation.
14. **`crates/scopenode-rpc/src/rest.rs`** — REST + SSE.
15. **`crates/scopenode-core/src/reorg.rs`** — 302 lines. The reorg detection
    algorithm is elegant and self-contained. The tests are the clearest way to
    understand it.
16. **`crates/scopenode-core/src/beacon.rs`** — Beacon status types + consensus
    preflight. The tests use `wiremock` to mock HTTP servers.
17. **`crates/scopenode-core/src/live.rs`** — 678 lines. Live sync loop.
    Read after understanding pipeline and reorg.
18. **`crates/scopenode-core/src/network.rs`** — 1081 lines. **Hardest file.**
    Read the module doc comment first. Boot sequence + peer tracking +
    receipt conversion + WireReceipt trait. Take two sessions on this file.
19. **All `commands/*.rs`** — now that you understand the core, read each command
    handler to see how the pieces connect to the CLI.

### Practical tips

**`INSERT OR IGNORE` is everywhere intentionally.** It makes every stage safe
to re-run. Interrupt and resume at any point — already-stored rows are silently
skipped.

**Block numbers in SQLite are `i64`.** SQLite has no unsigned integers. Code casts
`num as i64` when writing and `row as u64` when reading. Safe — block numbers
won't overflow `i64::MAX` in practice.

**`network.rs` is the hardest file.** It wraps reth internals with many moving
parts. The key insight: `get_receipts_for_blocks` loops indefinitely because most
mainnet peers can't serve historical receipts. Reliability matters more than speed.

**The `EthNetwork` trait is the test seam.** To test any pipeline behavior
without a real network, implement `EthNetwork` for a mock. The existing
`MockNetwork` in `pipeline.rs` tests shows the pattern.

**WAL mode is mandatory.** `Db::open()` enables WAL unconditionally. It allows
the JSON-RPC server to read while the pipeline writes. Do not change this.

**`broadcast::channel` is not a queue.** If a receiver falls behind (lags), it
misses events. This is correct behavior for live `eth_subscribe` streams — a
slow client shouldn't block new events for fast clients.
