---
status: completed
origin: docs/brainstorms/2026-05-21-efficiency-improvements-requirements.md
created: 2026-05-21
---

# Efficiency Improvements Implementation Plan

## Scope

Implement the efficiency requirements from `docs/brainstorms/2026-05-21-efficiency-improvements-requirements.md` without changing the user-facing product shape.

## Implementation units

### U1: CI hygiene and query behavior hardening

Goal: make clippy clean and correct result-cap behavior.

Files:
- Modify: `crates/scopenode-storage/src/db.rs`
- Modify: `crates/scopenode-rpc/src/server.rs`
- Modify: `crates/scopenode-rpc/src/rest.rs`
- Tests: `crates/scopenode-storage/src/db.rs`, `crates/scopenode-rpc/tests/parity.rs`

Test scenarios:
- Clippy no longer fails on cloned-ref-to-slice in tests.
- Fetching exactly the configured cap is allowed.
- Fetching more than the configured cap is detectable via limit-plus-one behavior.

### U2: Storage query/index efficiency

Goal: support the hot JSON-RPC query shape and remove streaming-query leaks.

Files:
- Modify: `crates/scopenode-storage/src/db.rs`
- Modify: `crates/scopenode-storage/src/migrations/005_reset_schema.sql` or add a new migration
- Tests: `crates/scopenode-storage/src/db.rs`

Test scenarios:
- SQLite has an index covering `contract`, `topic0`, `block_number`, `log_index`.
- Streaming events works without requiring leaked static SQL.
- Query helper remains ordered by block and log index.

### U3: ABI decoder precompilation

Goal: avoid repeated event input partitioning and `DynSolType` parsing per log.

Files:
- Modify: `crates/scopenode-core/src/abi.rs`
- Tests: `crates/scopenode-core/src/abi.rs`

Test scenarios:
- Existing event decode tests still pass.
- Invalid non-indexed ABI types fail when constructing `EventDecoder`, not repeatedly per log.
- Indexed and non-indexed decoding behavior remains unchanged.

### U4: Single-pass multi-contract ERA1 indexing

Goal: scan overlapping ERA1 files once per sync invocation and index all configured contracts from each bloom-hit block.

Files:
- Modify: `crates/scopenode/src/commands/sync.rs`
- Modify: `crates/scopenode-core/src/era_pipeline.rs`
- Tests: `crates/scopenode-core/tests/era1_pipeline_test.rs`

Test scenarios:
- Existing single-contract ERA1 pipeline still indexes expected events.
- Multiple configured contracts over the same ERA1 file are indexed in one pipeline entry point.
- One contract failure does not prevent independent scopes from indexing when safe.

## Verification

- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
