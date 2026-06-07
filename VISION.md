# scopenode Vision

## One-line vision

scopenode is a local-first Ethereum event backend for applications that need a
small, trustworthy slice of historical chain data.

## The problem

Many Ethereum applications begin by querying events from a few known contracts
over a bounded range. Their immediate need is much smaller than “run a full
node” or “build a general indexer,” but provider scripts and hosted indexers
still introduce credentials, rate limits, cost, and external dependencies.

scopenode's bet is that a narrow local dataset can be more useful than a broad
remote dependency during development, research, reproducible analysis, and
early product validation.

## Product promise

```text
Choose contracts, events, and ranges.
Point scopenode at local execution-history archives.
Build a verified SQLite event dataset.
Query it through familiar local APIs.
```

The promise is bounded. scopenode should answer data it has indexed completely,
reject unsupported or uncovered requests explicitly, and never imply it has a
general view of Ethereum state.

## Current product

Today scopenode provides:

- local `.era1` and `.ere` execution-history input;
- multi-contract, single-pass archive scanning;
- header bloom filtering;
- receipt-trie verification against `receipts_root`;
- ABI resolution through cache, local JSON, or Sourcify;
- named-event or wildcard event indexing;
- SQLite event and coverage storage;
- coverage-aware bounded event queries;
- `sync`, `serve`, and `status` commands;
- JSON-RPC `eth_getLogs`, `eth_blockNumber`, `eth_chainId`, and
  `net_peerCount`;
- REST events, status, contracts, and ABI endpoints.

It does not currently provide live sync, state reads, transaction/receipt/block
APIs, WebSockets, tracing, provider fallback, or complete Ethereum filter
compatibility.

## Product principles

### Scoped beats universal

Do not become a partial full node by accident. Make configured historical event
scopes correct, understandable, and fast.

### Loud failure beats silent incompleteness

An empty covered range and an uncovered range are different facts. The product
must preserve that distinction from SQLite through every transport.

### Verification should match the claim

Receipt-root verification proves that decoded logs belong to the receipt set
committed by the supplied block header. Local archive provenance and canonical
chain verification are separate concerns and must not be overstated.

### Local-first does not mean network-pure

Archive data and query serving are local. ABI resolution may use Sourcify unless
the ABI is already cached or supplied locally. This dependency should remain
visible and optional through configuration.

### Compatibility is earned method by method

Provider-compatible shapes are useful only when their supported filter and
coverage semantics are explicit. Unsupported combinations should be rejected,
not approximated.

### The codebase should teach its architecture

Domain outcomes and interfaces should make correctness policy visible. A new
contributor should be able to follow one sync and one query without reading the
entire repository.

## Target users

- application developers needing reproducible historical contract events;
- protocol researchers working with bounded mainnet ranges;
- analytics and security tools focused on known contracts;
- teams that already possess execution-history archives;
- contributors learning Ethereum data structures through a real pipeline.

scopenode is a poor fit for users who need current head data, arbitrary contract
state, traces, broad transaction search, or hosted production availability.

## Near-term direction

The highest-value work is to deepen the existing product before adding a second
data source:

1. improve archive and first-run diagnostics;
2. expose coverage more clearly in status and APIs;
3. complete Ethereum log-filter compatibility incrementally;
4. add structured observability and local benchmarks;
5. improve fixtures and end-to-end verification;
6. reduce naming and module-size debt around the ERA1/ERE source stack.

The observability design and implementation plan under `docs/superpowers/` are
approved future work but are not implemented in the current codebase.

## Possible later expansion

These are directions, not current commitments:

- transaction, receipt, and block read models derived from archive facts;
- WebSocket notifications over locally indexed data;
- explicit provider fallback for unsupported requests;
- live execution data with a separately designed canonicality and reorg model;
- export and migration tooling.

Live P2P sync should not be treated as the automatic next step. It changes the
trust model, storage lifecycle, failure modes, and product identity. It deserves
a fresh design decision rather than restoration by nostalgia.

## Success

scopenode succeeds when a user can create a bounded, locally queryable Ethereum
event dataset and understand exactly:

- what source files were used;
- which ranges completed;
- what was cryptographically verified;
- which ABI definitions decoded the logs;
- which queries can be answered completely.

Correct scope awareness is a more meaningful success metric than pretending to
cover the whole chain.
