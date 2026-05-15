> **Deprecated (2026-05-14):** This phase document is superseded by the architectural reset. See `docs/brainstorms/2026-05-14-architectural-reset-requirements.md`.

# Phase 1 - EraE-Backed Working MVP

## Goal

Build the smallest useful version of the new scopenode thesis:

```text
Local EraE history files
  -> verify coverage and execution data
  -> scan only the requested scopes
  -> decode matching contract events
  -> store them in SQLite
  -> serve them through local Ethereum-compatible queries
```

Phase 1 is no longer devp2p-first. The reason is practical: random execution
peers are not a reliable source of arbitrary old receipts. EraE gives scopenode
a local historical execution-data source, while scopenode keeps its real job:
verification, scoped indexing, decoding, storage, and local serving.

The product promise stays the same:

- No full/archive node to operate.
- No hosted RPC dependency.
- No API keys.
- No rate limits.
- No full-chain database.
- Only the contracts, events, and ranges the config asks for.

## What "done" looks like

```bash
scopenode index config.toml
# Reads local EraE files.
# Confirms the requested block range is covered.
# Scans block blooms for configured contract/event scopes.
# Verifies receipts for candidate blocks against receiptsRoot.
# Decodes matching logs and stores them in SQLite.

scopenode serve
# Starts local JSON-RPC at localhost:8545.

cast logs --rpc-url http://localhost:8545 \
  --address 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8 \
  --from-block 17000000 --to-block 17000100 \
  "Swap(address,address,int256,int256,uint160,uint128,int24)"
# Returns verified locally indexed events from SQLite.
```

## Core trust model

Phase 1 treats EraE as the local source of historical execution data and then
verifies the parts scopenode uses.

```text
Config scope
  -> chain, EraE directory, contract, events, block range

EraE source manifest
  -> what files exist
  -> what block ranges they cover
  -> whether requested ranges are complete

Headers
  -> parent continuity
  -> block hash
  -> logsBloom
  -> receiptsRoot
  -> transactionsRoot

Receipts
  -> rebuild receipt trie
  -> computed root must equal header.receiptsRoot

Decoded events
  -> address + topic0 match configured scope
  -> ABI decode
  -> store raw log fields plus decoded JSON
```

EraE can be incomplete, corrupt, stale, or from the wrong chain. Phase 1 must
therefore fail loudly when local coverage is incomplete and must verify every
receipt batch it indexes. It should not silently produce partial results unless
the user explicitly enables a debug/exploration mode in a later phase.

## V1 config shape

The config becomes index-first:

```toml
[chain]
id = 1
name = "mainnet"

[source]
kind = "erae"
path = "/path/to/erae"

[storage]
data_dir = "~/.scopenode"

[serve]
rpc_port = 8545

[[scopes]]
name = "Uniswap V3 ETH/USDC"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events = ["Swap", "Mint", "Burn"]
from_block = 17000000
to_block = 17001000
abi_override = "./abi.json"
```

The exact field names can change during implementation, but the product shape
is locked: one config declares chain, local source, storage, serving behavior,
and indexing scopes.

## Pipeline

```text
Stage 1: Load config and ABI
  - Validate chain/source/storage/scope fields.
  - Fetch ABI from Sourcify or load abi_override.
  - Compute event signatures and topic0 hashes.

Stage 2: Scan EraE source coverage
  - Build a manifest of local EraE files/chunks.
  - Determine covered block ranges.
  - Fail if any requested scope range is not fully covered.

Stage 3: Read and verify headers
  - Read headers for requested ranges.
  - Verify parent_hash continuity inside the range.
  - Store header fields needed for bloom scan and proof checks.

Stage 4: Bloom scan
  - Check logsBloom for configured address + topic0 targets.
  - Store candidate blocks per scope.
  - Skip receipt reads for blocks that cannot contain matching logs.

Stage 5: Receipt verify, decode, and store
  - Read receipts for candidate blocks from EraE.
  - Rebuild the receipt trie.
  - Assert computed root == header.receiptsRoot.
  - Filter logs by contract address and topic0.
  - ABI-decode matching logs.
  - INSERT OR IGNORE into SQLite.

Stage 6: Serve local queries
  - eth_getLogs succeeds only inside fully indexed scope coverage.
  - eth_getLogs outside coverage returns an explicit out-of-scope error.
  - eth_blockNumber returns the highest fully indexed local block.
```

## Commands

Phase 1 introduces the new command language:

```bash
scopenode index config.toml
scopenode serve
scopenode status
scopenode query --contract 0x... --event Swap
scopenode validate config.toml
```

`scopenode sync` can remain as a compatibility alias during the transition, but
the docs and product language should move to `index`.

## Explicit non-goals

- No historical devp2p receipt fetching in the happy path.
- No live sync.
- No Portal Network dependency.
- No remote EraE download/cache system.
- No historical `eth_call`.
- No full account/state archive.
- No traces or internal transactions.
- No NFT metadata or portfolio API.
- No partial indexing by default.

## Definition of done

- [ ] `scopenode index config.toml` accepts a local EraE source path.
- [ ] Source manifest records available files/chunks and covered block ranges.
- [ ] Indexing fails loudly when requested scope coverage is incomplete.
- [ ] Headers are read from EraE and stored with hash, parent hash, bloom, roots, timestamp, and number.
- [ ] Header continuity is verified for the indexed range.
- [ ] Bloom scan creates candidate blocks for configured address/topic0 pairs.
- [ ] Receipts for candidates are read from EraE.
- [ ] Receipt trie verification checks computed root against `receiptsRoot`.
- [ ] Matching logs are ABI-decoded and stored in SQLite.
- [ ] `eth_getLogs` returns only fully indexed in-scope data.
- [ ] Out-of-scope or incomplete queries return clear errors.
- [ ] `eth_blockNumber` reports the highest fully indexed local block, not network head.
- [ ] Unit tests cover coverage gaps, bloom matching, receipt-root verification, and out-of-scope query behavior.
