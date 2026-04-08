# Phase 4 — Developer APIs

## Goal

Make scopenode useful beyond the terminal. A REST API and SSE stream let
any app consume events in real time. Webhooks push live events to external
systems. `scopenode init` gets a new user running in under a minute.

**Prerequisite:** Phase 3 — live sync broadcast channel must be running.

---

## What we build

### REST API at `:8546`

```
GET  /events
       ?contract=0x...
       &event=Swap
       &topic0=0xddf252ad...   ← raw topic0 filter (event signature hash)
       &fromBlock=N  &toBlock=N
       &limit=100    &offset=0

GET  /status
GET  /contracts
GET  /abi/0x<address>
GET  /stream/events?contract=0x...&event=Swap     ← SSE
```

CORS open by default. All query params optional and combinable.
SSE subscribes to the Phase 3 broadcast channel — zero extra overhead.

`topic0` accepts a raw 32-byte hex topic — useful for callers that have the
event signature hash but not the human-readable name. Matches cryo's
`--topic0` filter semantics.

### Webhooks

Per-contract config:
```toml
[[contracts]]
address = "0x8ad..."
events  = ["Swap"]
webhook = "https://myapp.com/hooks/swaps"
```

Fire-and-forget via `tokio::spawn` — live sync never blocked.
Max 3 retries with exponential backoff (1s → 2s → 4s).
Failure logs a warning and drops — no queue, no crash.

Headers sent: `Content-Type: application/json`, `X-Scopenode-Event: <name>`

### `scopenode init`

Interactive wizard:
1. Enter contract address
2. Auto-detect proxy → resolve to implementation
3. Fetch events from Sourcify → multi-select with space bar
4. Enter `from_block` (number or `deploy`)
5. Toggle live sync
6. Write `config.toml` → offer to start sync immediately

Validates the resulting config with the same logic as `scopenode validate`
before writing.

### `--topic0` filter on `scopenode query`

```bash
scopenode query --contract 0xC02... --topic0 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef
```

Mirrors the REST `?topic0=` param. Useful for scripts that already hold the
keccak hash of an event signature and want to avoid passing the ABI.

### Python bindings

A `crates/scopenode-py` crate wraps the SQLite query layer via PyO3 and
exposes a pandas-friendly API:

```python
import scopenode

df = scopenode.query(
    "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
    event="Transfer",
    from_block=16_000_000,
    to_block=17_000_000,
)
print(df.head())
```

Returns a `pandas.DataFrame` with one row per event. Column names match the
ABI parameter names. Installable via `pip install scopenode` (maturin build).

### `scopenode export`

```bash
scopenode export --event Swap --format csv     > swaps.csv
scopenode export --event Swap --format json    > swaps.json
scopenode export --event Swap --format parquet > swaps.parquet
```

All filters from `/events` available as flags. Streams output — no full
load into memory.

---

## Definition of done

- [ ] `GET /events` output matches `eth_getLogs` for same contract + block range
- [ ] `GET /stream/events` delivers live events via SSE within 1s of block processing
- [ ] Webhook POST arrives within 1s of a live event
- [ ] Webhook failure never stalls or crashes live sync
- [ ] `scopenode init` produces a `config.toml` that passes `scopenode validate`
- [ ] `scopenode export --format csv/json/parquet` all produce valid output
- [ ] Parquet readable by DuckDB: `SELECT * FROM 'events.parquet' LIMIT 5`
- [ ] `scopenode query --topic0 0x...` returns the same rows as `?event=<name>` for the matching signature
- [ ] `GET /events?topic0=0x...` returns correct results; unknown topic0 returns empty list (not error)
- [ ] `pip install scopenode` succeeds; `scopenode.query(...)` returns a `pandas.DataFrame`
- [ ] DataFrame column names match ABI parameter names; U256 columns are `object` dtype (string)
- [ ] Unit tests: webhook retry/backoff, SSE fan-out, export format correctness, topic0 filter

## New dependencies

```toml
axum         = "0.7"
tower-http   = { version = "0.5", features = ["cors"] }
async-stream = "0.3"
dialoguer    = "0.11"
parquet      = "51"
pyo3         = { version = "0.21", features = ["extension-module"] }
```
