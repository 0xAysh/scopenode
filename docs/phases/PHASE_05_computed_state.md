# Phase 5 — Computed State (Rhai Handlers)

## Goal

Let users define per-event handler scripts that run after each event is
decoded and stored. Handlers can write derived state to user-defined SQLite
tables — running totals, current positions, price snapshots, leaderboards.
This covers 80% of what subgraph mappings do, with zero WASM complexity.

**Prerequisite:** Phase 4 — REST API and SQL views must be in place (handlers
expose their output through the same `/views/<name>` endpoint).

---

## The gap this fills

scopenode currently stores raw decoded events. If you want "current pool price"
or "total volume in the last 24h as a live number" you have to query and
aggregate yourself on every read. Handlers let you maintain that state
incrementally — compute it once per event, store it, serve it instantly.

```
Without handlers:             With handlers:
  GET /events → 50k rows        GET /views/pool_state → 1 row
  client aggregates             scopenode maintains it
```

---

## What we build

### Rhai scripting engine

[Rhai](https://rhai.rs) is a pure-Rust, sandboxed scripting language. It
embeds in ~5 lines, has no FFI, and compiles with the rest of the workspace.
The syntax is familiar (JavaScript-like) and fast enough for per-event work.

```toml
[[contracts]]
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events  = ["Swap", "Mint", "Burn"]
handler = "./handlers/uniswap_v3.rhai"
```

### Handler interface

Each handler script runs once per decoded event. The engine injects two
objects: `event` (read-only) and `db` (write-only, scoped to handler tables).

```rhai
// handlers/uniswap_v3.rhai

if event.name == "Swap" {
    let price = parse_sqrt_price(event.decoded.sqrtPriceX96);

    db.upsert("pool_state", #{
        contract:    event.contract,
        sqrt_price:  event.decoded.sqrtPriceX96,
        price_usd:   price,
        last_block:  event.block_number,
        updated_at:  event.timestamp,
    });

    db.increment("pool_stats", #{ contract: event.contract }, "swap_count", 1);
    db.add("pool_stats", #{ contract: event.contract }, "volume_token0",
           event.decoded.amount0);
}

if event.name == "Mint" || event.name == "Burn" {
    db.upsert("liquidity_events", #{
        contract:    event.contract,
        block:       event.block_number,
        tick_lower:  event.decoded.tickLower,
        tick_upper:  event.decoded.tickUpper,
        amount:      event.decoded.amount,
        kind:        event.name,
    });
}
```

### `event` object (read-only)

| Field | Type | Description |
|---|---|---|
| `event.name` | string | Event name (`"Swap"`, `"Transfer"`, …) |
| `event.contract` | string | Checksummed contract address |
| `event.block_number` | int | Block number |
| `event.block_hash` | string | Block hash (hex) |
| `event.tx_hash` | string | Transaction hash (hex) |
| `event.tx_index` | int | Transaction index in block |
| `event.log_index` | int | Log index in block |
| `event.timestamp` | int | Unix timestamp |
| `event.decoded` | map | ABI-decoded fields (names from ABI) |
| `event.raw_topics` | array | Raw topic bytes as hex strings |
| `event.raw_data` | string | Raw data bytes as hex string |

`event.decoded` values: integers and U256 as strings (avoids JS precision
loss), addresses as checksummed hex strings, booleans as booleans.

### `db` object (write-only, handler-scoped)

The `db` object exposes four operations. All table names are namespaced under
the handler file name to prevent collisions between handlers.

```
db.upsert(table, key_map)
  — INSERT OR REPLACE a row. key_map is a Rhai map: #{col: val, ...}
  — Creates the table on first call (schema inferred from key_map types)

db.increment(table, where_map, column, delta)
  — Atomically add delta to column in the matching row (INSERT 0 if missing)

db.add(table, where_map, column, value)
  — Alias for increment with signed delta (handles negative amounts)

db.delete(table, where_map)
  — Delete rows matching where_map (used for reorg rollback — see below)
```

Tables created by handlers are prefixed `h_<handler_name>_` in SQLite.
They are exposed automatically at `GET /views/h_<handler_name>_<table>`.

### Schema inference

On the first `db.upsert` call for a table, scopenode infers the SQLite schema
from the map's value types:

| Rhai type | SQLite type |
|---|---|
| int | INTEGER |
| float | REAL |
| string | TEXT |
| bool | INTEGER (0/1) |

All columns are `NOT NULL`. If a subsequent call includes a new key not in the
original schema, scopenode runs `ALTER TABLE ADD COLUMN` automatically.

### Reorg handling

When the reorg detector marks events `reorged = 1`, it also re-runs the
handler for each orphaned event in reverse order with a special
`event.reorg = true` flag. Handlers should use this to undo state:

```rhai
if event.reorg {
    // Undo this event's effect
    if event.name == "Swap" {
        db.add("pool_stats", #{ contract: event.contract },
               "volume_token0", -event.decoded.amount0);
        db.increment("pool_stats", #{ contract: event.contract },
                     "swap_count", -1);
    }
    return;
}
// Normal (non-reorg) processing below...
```

Handlers that don't implement reorg rollback are safe — their derived state
may be temporarily stale after a reorg (corrected when the winning chain's
events re-run the handler). The `events` table itself is always correct.

### Execution model

- Handlers run **synchronously** after each event is stored, in the same
  SQLite write transaction. Handler failure rolls back the event store — the
  block is marked `pending_retry`.
- One Rhai `Engine` per contract is created at startup and reused. Scripts
  are compiled once (`AST`) and evaluated per event.
- Maximum execution time: 100ms per handler call. Exceeded → error logged,
  block marked `pending_retry`.
- No network access from handlers. No filesystem access. No `eval()`.
  The Rhai engine is configured with `Engine::new_raw()` + explicit module
  allowlist (only `db` and `event` are registered).

### `scopenode handler check <script.rhai>`

Validates a handler script without running a sync:
1. Parses and compiles the Rhai AST — catches syntax errors
2. Runs against a synthetic event fixture — catches runtime errors
3. Reports which tables would be created and their inferred schemas

```bash
scopenode handler check ./handlers/uniswap_v3.rhai
✓  Compiled successfully
✓  Tables created:
     pool_state       (contract TEXT, sqrt_price TEXT, price_usd REAL, last_block INTEGER, updated_at INTEGER)
     pool_stats       (contract TEXT, swap_count INTEGER, volume_token0 TEXT)
     liquidity_events (contract TEXT, block INTEGER, tick_lower INTEGER, tick_upper INTEGER, amount TEXT, kind TEXT)
✓  Dry run complete (no DB changes)
```

---

## What this enables

After Phase 5, scopenode can answer queries that previously required
application-side aggregation:

```bash
# Current pool price (1 row, instant)
curl http://localhost:8546/views/h_uniswap_v3_pool_state

# All-time swap count and volume (1 row, instant)
curl http://localhost:8546/views/h_uniswap_v3_pool_stats

# All liquidity events (filterable)
curl "http://localhost:8546/views/h_uniswap_v3_liquidity_events?tick_lower=-200"
```

Combined with Phase 4 SQL views, users can build complex derived queries on
top of handler-maintained state without any server-side code.

---

## Definition of done

- [ ] `handler = "./path.rhai"` in config loads and compiles the script at startup
- [ ] Handler runs once per decoded event, after `INSERT OR IGNORE INTO events`
- [ ] `event.decoded` fields accessible by ABI parameter name
- [ ] `db.upsert` creates the table on first call; schema inferred from value types
- [ ] `db.increment` and `db.add` work atomically within the SQLite transaction
- [ ] Handler tables exposed at `GET /views/h_<handler>_<table>`
- [ ] Handler failure rolls back the event store; block marked `pending_retry`
- [ ] Handler execution capped at 100ms; exceeded → `pending_retry`
- [ ] No network/filesystem access from handler (verified by Rhai engine config)
- [ ] Reorg: orphaned events re-run handler with `event.reorg = true`
- [ ] `scopenode handler check <script>` validates and dry-runs without touching DB
- [ ] Schema evolution: new key in `db.upsert` call triggers `ALTER TABLE ADD COLUMN`
- [ ] Unit tests: upsert creates table, increment idempotency, reorg rollback, timeout enforcement
- [ ] Integration test: Uniswap V3 handler produces correct `pool_stats` for fixture blocks

## New dependencies

```toml
rhai = { version = "1", features = ["sync"] }
# sync feature required for Send + Sync across tokio tasks
```
