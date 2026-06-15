# scopenode

scopenode is a local Ethereum event indexer. It reads execution-history archives
from disk, verifies receipt data against block headers, decodes configured
contract events, stores them in SQLite, and serves the indexed data over
JSON-RPC and REST.

Current scope:

- local `.era1` and `.ere` archive files;
- Ethereum mainnet (`eth_chainId` returns `0x1`);
- configured contracts, events, and finite block ranges;
- decoded events rather than general Ethereum state;
- JSON-RPC `eth_getLogs` plus a small read-only API surface.

It is not a full node, a live chain follower, or a complete RPC replacement.

## Data flow

```text
config.toml
    │
    ├─ contract scopes + block ranges
    └─ ABI resolution: SQLite cache → local file → Sourcify
                    │
                    ▼
local .era1/.ere archives
    │
    ├─ select overlapping files once for the union range
    ├─ decode block headers, receipts, and transaction hashes
    ├─ bloom-filter each configured scope
    ├─ verify the receipt trie root
    └─ decode matching logs
                    │
                    ▼
SQLite: contracts + events + covered_ranges
                    │
          ┌─────────┴─────────┐
          ▼                   ▼
 JSON-RPC :8545          REST :8546
```

Coverage is recorded only after a scope has completed cleanly. Queries with an
explicit contract and block range fail rather than returning silently incomplete
results when that range is not covered.

## Install

Prerequisites:

- a recent stable Rust toolchain;
- local Ethereum execution-history archives in `.era1` or `.ere` format.

```bash
git clone https://github.com/0xAysh/scopenode
cd scopenode
cargo build --release
```

The binary is written to `target/release/scopenode`.

## Quick start

Create `config.toml`:

```toml
[node]
port = 8545
rest_port = 8546
data_dir = "~/.scopenode"
era_dir = "~/ethereum-archives"

[[contracts]]
name = "Uniswap V3 ETH/USDC"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events = ["Swap"]
from_block = 17000000
to_block = 17010000

# Optional. Resolution order is cache → this file → Sourcify.
abi_override = "./abis/UniswapV3Pool.json"

# Optional for proxies: fetch the implementation ABI while indexing proxy logs.
# impl_address = "0x..."
```

Then:

```bash
# Validate and display the planned scopes without reading archives.
scopenode sync --config config.toml --dry-run

# Index the configured scopes.
scopenode sync --config config.toml

# Inspect local state.
scopenode status --config config.toml

# Start both read APIs.
scopenode serve --config config.toml
```

Repeated syncs are safe: event inserts are idempotent. A repeated sync currently
rescans the selected archives; it is not a per-block checkpoint resume system.

## Configuration

```toml
[node]
port = 8545
rest_port = 8546
data_dir = "~/.scopenode"
era_dir = "~/ethereum-archives"

[[contracts]]
name = "USDC"
address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
events = ["Transfer", "Approval"]
from_block = "17M"
to_block = "17.1M"
abi_override = "./abis/USDC.json"
impl_address = "0x..."
```

Rules:

- `to_block` is required and inclusive.
- `events` must contain at least one event name.
- `events = ["*"]` indexes every event in the resolved ABI.
- `"*"` cannot be mixed with named events.
- `abi_override` and `impl_address` are optional.
- block numbers accept integers or `K`/`M` shorthand such as `"12.3K"`.
- unknown TOML fields are rejected.
- `~/` is expanded for `data_dir` and `era_dir`; other relative paths use the
  process working directory.

### ABI resolution

For each contract, scopenode resolves event definitions in this order:

1. ABI previously cached in SQLite for the configured contract address;
2. the local `abi_override`, when present and valid;
3. Sourcify full-match, then partial-match metadata on mainnet.

For proxy contracts, `impl_address` changes only the remote fetch address. Logs
are still matched and stored under the configured proxy address. A successful
local or remote resolution is normalized to event definitions and cached.

Because Sourcify is used by default when the cache and local override cannot
resolve an ABI, indexing can make outbound HTTPS requests. Use a valid local
override when deterministic, network-independent ABI resolution is required.

## CLI

```text
scopenode sync   [--config ./config.toml] [--dry-run]
scopenode serve  [--config ./config.toml]
scopenode status [--config ./config.toml]
```

`-v`, `-vv`, and `-vvv` increase log verbosity.

## JSON-RPC

The server binds to `127.0.0.1:<node.port>`.

| Method | Current behavior |
|---|---|
| `eth_getLogs` | Requires exactly one address. Supports one `topics[0]`, `fromBlock`, and `toBlock`. |
| `eth_blockNumber` | Highest block number represented by stored events, or `0x0`. |
| `eth_chainId` | Always `0x1`. |
| `net_peerCount` | Always `0x0`; there is no live peer network. |

`eth_getLogs` rejects:

- missing address (`-32602` Invalid params);
- multiple addresses (`-32002`);
- topic0 OR filters;
- topic filters beyond topic0;
- uncovered explicit ranges (`-32001`);
- result sets over 10,000 rows (`-32005`).

Example:

```bash
cast rpc --rpc-url http://127.0.0.1:8545 eth_getLogs \
  '{"address":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","fromBlock":"0x1036640","toBlock":"0x104ece0"}'
```

## REST API

The server binds to `127.0.0.1:<node.rest_port>`.

```text
GET /events
    ?contract=0x...
    &event=Transfer
    &topic0=0xddf252...
    &fromBlock=17000000
    &toBlock=17100000
    &limit=100
    &offset=0

GET /status
GET /contracts
GET /abi/:address
```

`limit` defaults to 100 and is clamped to 10,000. Offset pagination is functional:
supplying `limit=100&offset=100` returns the next page. HTTP 400 is returned only
when a query would materialise more than 10,000 rows regardless of the
user-supplied limit. The event query path is shared with JSON-RPC, so coverage
policy is consistent. REST returns an empty result for an unindexed contract;
JSON-RPC returns a not-indexed error (`-32000`).

## Storage

Database: `<data_dir>/scopenode.db`.

The active schema is intentionally small:

| Table | Purpose |
|---|---|
| `contracts` | Contract registry and normalized cached event ABI JSON. |
| `events` | Raw log identity/data plus decoded event fields and block timestamp. |
| `covered_ranges` | Successfully completed contract/range/source facts. |

SQLite runs migrations at startup. Event identity is
`(block_number, tx_index, log_index)`, allowing `INSERT OR IGNORE` to make
repeated syncs idempotent.

## Repository map

```text
crates/scopenode/
  cli.rs, runtime.rs
  commands/{sync,serve,status}.rs
  sourcify.rs

crates/scopenode-core/
  config.rs
  source.rs
  e2store.rs
  era1_codec.rs
  era1_reader.rs
  era_pipeline.rs
  abi_resolution.rs
  abi.rs
  headers.rs
  receipts.rs
  decode_quality.rs

crates/scopenode-storage/
  db.rs, models.rs, types.rs
  query.rs, coverage.rs, sink.rs
  src/migrations/

crates/scopenode-rpc/
  filter_plan.rs
  query_front_door.rs
  projection.rs
  server.rs
  rest.rs
```

See `ONBOARDING.md` for the recommended learning order and `CONTEXT.md` for the
canonical domain vocabulary.

## Verification

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Current limitations

- no live sync, reorg handling, state queries, transactions, traces, or
  subscriptions;
- mainnet chain ID is hard-coded;
- partial Ethereum filter compatibility;
- archive acquisition is outside scopenode;
- status derives its block range from stored events, not from coverage-only
  ranges;
- observability and benchmark work under `docs/superpowers/` is planned, not yet
  implemented.
