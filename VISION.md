# scopenode — Vision

## One-line vision

scopenode is a **local-first Ethereum data backend for early apps**.

It lets developers build and validate Ethereum apps without paying for RPC
providers, running a full node, or committing to The Graph before they know the
app deserves production-scale infrastructure.

---

## The core idea

Most Ethereum apps do not need the whole chain on day one.

They need a narrow slice of mainnet data:

- events from a few contracts
- receipts for transactions they care about
- blocks around those transactions
- maybe live updates for the same scoped data

Today, developers usually reach for Infura, Alchemy, a full node, or The Graph
immediately. That means cost, rate limits, setup complexity, and operational
commitment before the product is even validated.

scopenode gives them a fourth path:

```text
Tell scopenode what your app cares about.
It builds a local verified dataset.
Your app queries localhost instead of a provider.
When the app grows, graduate to provider-scale infra.
```

The goal is not to replace every RPC provider forever.
The goal is to replace the provider calls an early app actually needs, while the
app is still small enough to run from a local scoped dataset.

---

## Product promise

For configured scopes, scopenode should provide provider-like APIs backed by
local verified Ethereum data.

```text
ERA1 historical files
    + scoped indexing
    + SQLite local storage
    + JSON-RPC / REST APIs
    + future P2P live tail
    + future light-client validation
    = local app data backend before provider scale
```

The user experience should feel like:

```text
1. Define contracts/events/ranges my app needs
2. Run scopenode sync
3. Point my app at http://localhost:8545
4. Build without paying for RPC infra
5. Move to Alchemy/The Graph/custom infra only when scale justifies it
```

---

## What scopenode is

scopenode is:

- a scoped Ethereum data backend
- a local verified event/receipt/block indexer
- a provider-compatible development backend for early apps
- a way to postpone infra spend until after product validation
- a migration bridge from prototype to production infra

scopenode is not:

- a full Ethereum node
- a general archive-state database
- a universal replacement for every RPC provider method
- a tracing node
- a promise that every Ethereum query can be answered locally for free

---

## Target users

scopenode is for developers building Ethereum apps before scale.

Examples:

- a DeFi analytics dashboard for one or two pools
- an NFT mint or holder dashboard
- a DAO governance history page
- a wallet/portfolio app for a known set of contracts
- a hackathon project that needs real mainnet data
- a researcher analyzing a bounded historical range
- a security tool watching specific contracts

The common pattern:

```text
The app needs real mainnet data,
but not enough data or traffic to justify production infra yet.
```

---

## The user journey

```text
Idea / hackathon
    ↓
scopenode on laptop
    ↓
validated app with real users
    ↓
hosted scopenode or bigger local machine
    ↓
Alchemy / Infura / The Graph / custom indexer
```

The value is not “never pay providers.”

The value is:

> Do not pay for provider-scale infrastructure before you know your app deserves
> provider-scale infrastructure.

---

## Compatibility boundary

scopenode should be honest about what it can answer.

For any request, there are three possible outcomes:

1. **Answered locally** — data is inside the verified local scope.
2. **Rejected loudly** — data is outside local coverage or unsupported.
3. **Optionally proxied** — in development mode, unsupported calls can be sent to
   a configured provider.

Silent partial data is never acceptable.

A missing range, missing contract, unsupported method, or incomplete source must
return an explicit error that tells the user what is missing and how to fix it.

---

## Provider method strategy

scopenode should first replace provider calls that are naturally derived from
blocks, transactions, receipts, and logs.

### Tier 1 — core local backend

These are the highest-value methods for early apps:

- `eth_getLogs`
- `eth_getTransactionReceipt`
- `eth_getTransactionByHash`
- `eth_getBlockByNumber`
- `eth_getBlockByHash`
- `eth_blockNumber`
- `eth_chainId`

REST equivalents should exist for app backends and scripts:

- `/events`
- `/transactions`
- `/receipts`
- `/blocks`
- `/contracts`
- `/status`

### Tier 2 — compatibility helpers

These improve app compatibility but must be scoped carefully:

- `eth_getCode` for configured contracts
- limited/cached read models for common contract state
- provider fallback mode for unsupported calls
- export to CSV/JSON
- migration tooling toward The Graph or provider-backed infra

### Tier 3 — later expansion

These are powerful but should not block initial validation:

- live P2P indexing
- checkpoint/light-client validated headers
- WebSocket subscriptions
- safe/finalized/latest head policies
- reorg handling
- hosted scopenode
- generalized state access
- tracing APIs

---

## Trust model

scopenode should be more trustworthy than a normal provider for the data it has
verified locally.

Historical path:

```text
ERA1 files
    ↓
headers + blooms + receipts
    ↓
receipt trie verification against receiptsRoot
    ↓
ABI decode
    ↓
SQLite
    ↓
JSON-RPC / REST
```

Future live path:

```text
Ethereum P2P peers
    ↓
headers + block bodies + receipts
    ↓
light client validates canonical/safe/finalized headers
    ↓
receipt trie verifies logs against receiptsRoot
    ↓
local storage
    ↓
provider-like APIs
```

The light client verifies canonical headers.
Receipt verification proves logs belong to those headers.
Coverage tracking proves whether scopenode has enough local data to answer a
query completely.

---

## Current MVP

The current implementation is intentionally narrow:

```text
local ERA1 files → verified events → SQLite → JSON-RPC + REST
```

Current strengths:

- no API keys
- no provider dependency in the indexing path
- local deterministic ERA1 input
- bloom filtering
- receipt decoding
- receipt-root verification
- ABI decoding
- SQLite persistence
- resumable/idempotent inserts
- `eth_getLogs` for indexed scopes
- REST `/events`, `/status`, `/contracts`, `/abi/:address`

Current limitations:

- historical ERA1 only
- no live P2P tail yet
- no light-client validation yet
- scoped data only
- limited JSON-RPC method surface
- incomplete `eth_getLogs` filter compatibility
- rough first-run/config/fixture UX
- no general Ethereum state support

This is acceptable for the MVP, as long as the product promise remains scoped and
coverage-aware.

---

## Product principles

### 1. Scoped beats universal

Do not try to become a full node first.

Start with the data early apps actually need. Make that path excellent.

### 2. Loud failure beats silent wrongness

If scopenode cannot answer a query completely, it must say so.

Bad:

```text
return partial logs and hope nobody notices
```

Good:

```text
error: range 18,000,000–18,100,000 is not fully indexed
missing coverage: 18,043,000–18,050,000
```

### 3. Provider-compatible where it matters

Apps should be able to point common Ethereum libraries at:

```text
http://localhost:8545
```

and use familiar methods for supported scopes.

### 4. Local-first, not local-only

Strict local mode is important for trust and reproducibility.
But development ergonomics may require optional provider fallback for unsupported
calls.

Both modes should be explicit.

### 5. Migration is part of the product

scopenode should help users leave when they outgrow it.

A user graduating to The Graph, Alchemy, or custom infra is not failure. It means
scopenode helped them validate the app cheaply.

---

## Roadmap

### Phase 1 — make the historical local MVP undeniable

Goal: one real app can replace provider-backed historical event reads with
scopenode.

- Fix config and fixture UX
- Require real local ABI files or provide clear ABI setup
- Add `scopenode status`
- Add `scopenode validate`
- Add `scopenode doctor`
- Improve `eth_getLogs` compatibility:
  - address arrays
  - topic wildcards
  - topic OR logic
  - block tags where meaningful
  - clear out-of-scope errors
- Add coverage-aware query checks
- Add a polished demo app using localhost JSON-RPC
- Document the exact provider calls scopenode supports today

### Phase 2 — expand provider-compatible local data

Goal: support the common non-state calls early apps need.

- `eth_getTransactionReceipt`
- `eth_getTransactionByHash`
- `eth_getBlockByNumber`
- `eth_getBlockByHash`
- transaction/receipt/block REST endpoints
- better SQLite indexes for app query patterns
- export command for CSV/JSON
- clear result caps and pagination

### Phase 3 — source hardening

Goal: make local historical data trustworthy and diagnosable.

- ERA1 checksum verification
- ERA1 hash-chain/continuity verification
- persistent source manifest
- incomplete-range detection
- integrity status in `status` and REST
- named errors for missing/corrupt/truncated sources

### Phase 4 — live/recent data

Goal: let scopenode continue past historical ERA1 coverage.

- P2P header fetching
- P2P receipt fetching
- safe/finalized/latest head policy
- light-client validation for canonical headers
- receipt verification for live receipts
- reorg handling near the live boundary
- WebSocket subscriptions for scoped live events

### Phase 5 — compatibility and migration

Goal: make scopenode fit naturally into real app development.

- optional provider fallback mode
- strict local mode by default for verified data paths
- unsupported-method diagnostics
- SDK examples for viem, ethers, wagmi, Foundry/cast
- migration docs:
  - scopenode → Alchemy/Infura
  - scopenode → The Graph
  - scopenode → custom indexer
- possible hosted scopenode offering

---

## Success criteria

scopenode is working if a developer can say:

```text
I built my Ethereum app against real mainnet data without paying for an RPC
provider, without running a full node, and without writing a subgraph.

When the app got traction, I knew exactly which infra to graduate to.
```

The near-term success metric is not total chain coverage.

The near-term success metric is:

> Can one real app get the exact mainnet data it needs from scopenode faster and
> cheaper than from a provider-backed script?

If yes, the product is valid.
