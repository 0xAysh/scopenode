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
       &fromBlock=N  &toBlock=N
       &limit=100    &offset=0

GET  /status
GET  /contracts
GET  /abi/0x<address>
GET  /stream/events?contract=0x...&event=Swap     ← SSE
```

CORS open by default. All query params optional and combinable.
SSE subscribes to the Phase 3 broadcast channel — zero extra overhead.

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
- [ ] Unit tests: webhook retry/backoff, SSE fan-out, export format correctness

## New dependencies

```toml
axum         = "0.7"
tower-http   = { version = "0.5", features = ["cors"] }
async-stream = "0.3"
dialoguer    = "0.11"
parquet      = "51"
```
