# Phase 3 - Light Client Boundary and Live/Recent Data

## Goal

Add a light-client trust anchor and prepare scopenode to bridge local historical
EraE data with recent/live chain data.

The light client is not the historical data source. Its job is to tell
scopenode what Ethereum considers canonical, finalized, safe, and current.
EraE remains the historical execution-data source.

## What changes

| Area | Phase 2 | Phase 3 |
|---|---|---|
| Canonicality | Local source provenance + verification | Light-client finalized/safe head anchor |
| Upper bound | Config range only | Reject ranges beyond policy boundary |
| Recent data | Historical EraE only | Optional recent/live boundary |
| Reorgs | Not relevant for finalized historical ranges | Detected and handled for live/recent data |
| Serving | Static indexed DB | Can expose indexed head and live subscriptions |

## What the light client does

The light client provides:

- latest consensus head
- finalized head
- safe execution block boundary where available
- execution block hash/number for consensus payloads
- finality/canonicality signal

It does not provide:

- historical receipts
- historical logs
- full transaction bodies
- contract storage history
- historical `eth_call`
- decoded events

The intended model is:

```text
Light client
  -> canonicality, finality, safe/latest execution head

EraE files
  -> historical headers, bodies, receipts

scopenode
  -> verify, scan, decode, index, serve
```

## Verification policy

Phase 3 adds a configurable boundary policy:

```toml
[chain]
id = 1

[light_client]
enabled = true
checkpoint = "0x..."
consensus_rpc = [
  "https://www.lightclientdata.org",
  "https://sync-mainnet.beaconcha.in",
]

[index_policy]
max_head = "finalized" # finalized | safe | latest
allow_unverified_head = false
```

If a scope asks for blocks beyond the selected safe boundary, indexing fails
with a clear error unless the user explicitly opts into a looser policy.

## Reorg handling

Reorg handling matters only near the live/recent boundary. For finalized
historical EraE ranges, reorgs are outside the normal concern.

For recent/live indexing:

1. Track the local indexed tip.
2. Compare each new block parent against the local tip hash.
3. On mismatch, walk back to the common ancestor.
4. Mark orphaned events as `reorged = 1`.
5. Re-index the winning chain blocks.

scopenode should never hard-delete events by default.

## Optional recent/live source

Phase 3 can introduce a recent/live execution-data source, but this is separate
from the historical EraE path. Possible sources:

- devp2p for recent headers/receipts
- a local execution node
- Portal Network when mature enough
- remote RPC only as an explicit opt-in fallback, never the default promise

The product promise must stay clear: EraE is the historical source; the light
client anchors canonicality; any live execution-data source is a separate mode.

## Operator experience

Commands should make the boundary visible:

```bash
scopenode doctor
# light client: synced
# finalized execution block: 19842301
# indexed local block: 19842000
# source coverage: 17000000-19842000 complete

scopenode index config.toml
# rejects ranges above finalized when policy=max_head="finalized"
```

## Definition of done

- [ ] Light client can report finalized/safe/latest execution block identity.
- [ ] Config supports light-client endpoints, checkpoint, and head policy.
- [ ] Indexing rejects requested ranges beyond the configured policy boundary.
- [ ] `doctor` reports light-client status and indexed-vs-finalized boundary.
- [ ] Reorg handling marks orphaned events instead of deleting them.
- [ ] Live/recent indexing is explicitly separated from historical EraE indexing.
- [ ] Any remote RPC fallback is opt-in and clearly labeled as outside the default trust story.
- [ ] Tests cover boundary rejection, unverified opt-in behavior, and reorg marking.
