# Phase 4 - Developer APIs and Product Polish

## Goal

Make scopenode useful as a local data product, not only a terminal indexing
tool. Apps should be able to query indexed data through familiar Ethereum APIs,
REST endpoints, streams, and export workflows while preserving strict coverage
and verification semantics.

## What we build

### JSON-RPC compatibility

scopenode serves Ethereum-compatible JSON-RPC at `localhost:8545`:

```text
eth_getLogs
eth_blockNumber
eth_chainId
```

Rules:

- `eth_getLogs` succeeds only for fully indexed in-scope ranges.
- Queries outside configured coverage return explicit out-of-scope errors.
- Queries inside a known incomplete range return explicit incomplete-range errors.
- `eth_blockNumber` returns the highest fully indexed local block, not network head.
- `eth_chainId` returns the configured chain id.

### REST API

```text
GET /events
  ?contract=0x...
  &event=Swap
  &topic0=0xddf252ad...
  &fromBlock=N
  &toBlock=N
  &limit=100
  &offset=0

GET /status
GET /sources
GET /scopes
GET /contracts
GET /abi/0x<address>
```

REST responses should expose enough coverage metadata that applications can
distinguish "no matching events" from "range was not indexed."

### Streaming APIs

When Phase 3 live/recent indexing is enabled:

```text
GET /stream/events?contract=0x...&event=Swap
eth_subscribe logs
eth_subscribe newHeads
```

Streaming is not required for static historical EraE indexing, but the API
should be ready to fan out live indexed events when that mode is active.

### Export

```bash
scopenode export --event Swap --format csv  > swaps.csv
scopenode export --event Swap --format json > swaps.json
```

Exports stream from SQLite and use the same coverage checks as JSON-RPC and
REST. A successful export should mean the requested range was fully indexed.

### Better local DX

Add polish that makes the product self-explanatory:

- Clear first-run errors for missing EraE source paths.
- `status` output that shows source coverage and indexed scope coverage.
- Human-readable progress for manifest scan, bloom scan, receipt verification,
  decode, and store.
- `--json` output for automation.
- Documentation examples for common contract-event indexing workflows.

## Definition of done

- [ ] `eth_getLogs` matches stored raw log fields for indexed scopes.
- [ ] JSON-RPC errors distinguish out-of-scope, incomplete, and malformed queries.
- [ ] `eth_blockNumber` returns the highest fully indexed local block.
- [ ] REST `/events` supports contract, event, topic0, block range, limit, and offset filters.
- [ ] REST `/status`, `/sources`, and `/scopes` expose coverage and verification status.
- [ ] Export supports CSV and JSON without loading all rows into memory.
- [ ] Streaming APIs work when live/recent indexing is enabled.
- [ ] Query, REST, export, and JSON-RPC share the same coverage checks.
- [ ] Docs include an end-to-end EraE indexing example.
- [ ] Tests cover JSON-RPC parity, REST filtering, export formats, and coverage error behavior.
