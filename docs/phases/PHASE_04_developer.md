# Phase 4 — Developer APIs

## Goal

Make scopenode useful beyond the terminal. A REST API and SSE stream let
any app consume events in real time. `scopenode export` writes indexed data
to CSV or JSON for use in analytics workflows.

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

### `eth_subscribe` over WebSocket

scopenode already has a broadcast channel for live events (Phase 3). Exposing
it as a standard Ethereum WebSocket subscription makes scopenode a drop-in for
Viem/ethers.js `watchContractEvent` — no client changes needed.

```bash
wscat -c ws://localhost:8545
→ {"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["logs",{"address":"0x8ad...","topics":["0xc42...swap topic0"]}]}
← {"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0x1","result":{...log...}}}
```

`jsonrpsee` has built-in WebSocket support — this is mostly wiring the
broadcast receiver into a subscription response stream.

Supported subscription types:
- `"logs"` with optional `address` + `topics` filter (mirrors `eth_getLogs` params)
- `"newHeads"` — emit each new block header as scopenode indexes it

`eth_unsubscribe` cancels the subscription. Disconnecting a WebSocket client
drops the receiver automatically (broadcast channel semantics).

### `--topic0` filter on `scopenode query`

```bash
scopenode query --contract 0xC02... --topic0 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef
```

Mirrors the REST `?topic0=` param. Useful for scripts that already hold the
keccak hash of an event signature and want to avoid passing the ABI.

### `scopenode export`

```bash
scopenode export --event Swap --format csv  > swaps.csv
scopenode export --event Swap --format json > swaps.json
```

All filters from `/events` available as flags. Streams output — no full
load into memory.

---

## Definition of done

- [ ] `GET /events` output matches `eth_getLogs` for same contract + block range
- [ ] `GET /stream/events` delivers live events via SSE within 1s of block processing
- [ ] `scopenode export --format csv/json` both produce valid output
- [ ] `scopenode query --topic0 0x...` returns the same rows as `?event=<name>` for the matching signature
- [ ] `GET /events?topic0=0x...` returns correct results; unknown topic0 returns empty list (not error)
- [ ] `eth_subscribe logs` delivers live events to a WebSocket client within 1s of indexing
- [ ] `eth_subscribe newHeads` emits each new block header as it's indexed
- [ ] `eth_unsubscribe` cancels a subscription; client disconnect auto-cleans the receiver
- [ ] Unit tests: SSE fan-out, export format correctness, topic0 filter, WebSocket subscription lifecycle

## New dependencies

```toml
axum         = "0.7"
tower-http   = { version = "0.5", features = ["cors"] }
async-stream = "0.3"
# eth_subscribe WebSocket — jsonrpsee already in workspace, just enable ws feature
jsonrpsee    = { version = "0.26", features = ["server", "macros", "ws"] }
```
