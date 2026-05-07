# Light Node + EraE Pivot Architecture Brainstorm

## Goal

Reframe scopenode from a devp2p-first custom node into a local verified
Ethereum history indexer.

The intended model is:

```text
Light node
  -> canonicality, finality, latest trusted headers

Local EraE files
  -> historical execution data: headers, bodies, receipts

scopenode
  -> scoped scan, verify, decode, index, store, serve
```

## Locked Decisions

- V1 uses a local EraE directory as the historical data source.
- V1 does not depend on remote EraE download, Portal Network, or random
  historical devp2p availability.
- The config is the contract with the user: scopenode fetches and indexes only
  what the config asks for.
- Initial scope type is contract events: address, event names, ABI source, and
  block range.
- V1 stores matching decoded events plus raw log fields needed for
  `eth_getLogs` compatibility.
- V1 does not store full receipts, unrelated logs, or a full block/transaction
  archive by default.
- V1 uses a pragmatic verified-local trust model: verify file integrity where
  available, parent continuity, body roots, receipt roots, and event inclusion;
  document local EraE provenance as an assumption for historical canonicality.
- Light node integration in v1 confirms network, provides latest/finalized head,
  rejects ranges beyond the configured safe/finalized policy, and prepares the
  future live boundary.
- Current devp2p, peer-management, mempool relay, daemon lifecycle, live sync,
  and retry-on-random-peers behavior are outside the v1 happy path.
- CLI language should become index-first:

```bash
scopenode index config.toml
scopenode serve
scopenode query ...
scopenode export ...
scopenode status
```

- Storage should migrate toward scope-based tables, including concepts like
  `sources`, `source_chunks`, `scopes`, `headers`, `index_cursor`, and `events`.
- V1 should scan local EraE sources into a manifest before indexing scopes.
- Indexing fails loudly by default when local EraE coverage is incomplete.
- A debug/exploration mode may allow partial indexing, but partial status must
  be explicit and impossible to miss.
- `eth_getLogs` succeeds only inside fully indexed scope/range coverage.
- `eth_getLogs` outside coverage returns an explicit out-of-scope or incomplete
  range error.
- `eth_blockNumber` returns the highest fully indexed block known to the local
  scopenode DB, not the latest network head.
- V1 uses one config file containing chain, light client, source, storage,
  index policy, and scopes.

## Open Architecture Question

### Module and crate structure

We have not decided whether the pivot should be implemented by refactoring
inside the current crates first, creating new dedicated crates, or building a
clean v2 workspace beside the existing code.

Options under consideration:

1. Refactor inside current crates first.
   - Keep the existing workspace mostly intact.
   - Add modules such as `source`, `erae`, `indexer`, and `light` inside
     `scopenode-core`.
   - Move `network` and daemon-era behavior out of the v1 path over time.
   - Lowest churn and best reuse, but boundaries may stay muddy for longer.

2. Create new dedicated crates.
   - Add crates such as `scopenode-erae`, `scopenode-light`, and
     `scopenode-indexer`.
   - Cleaner boundaries from the start.
   - More boilerplate and more architecture decisions before the core model is
     proven.

3. Start a clean v2 workspace.
   - Build the new architecture beside the old one, then port storage, ABI,
     verification, and RPC pieces across.
   - Cleanest mental model.
   - Highest rewrite risk and easiest path to losing working code.

Current leaning: unresolved. This should stay open until the rest of the
architecture design is clearer.

## Not In V1

- Full archive state.
- Historical `eth_call`.
- Historical balances and storage reads.
- Debug traces and internal transactions.
- Mempool and transaction submission.
- NFT metadata or portfolio APIs.
- General Alchemy/Infura replacement.
- Portal Network dependency.
- Remote EraE mirror/download/cache system.
