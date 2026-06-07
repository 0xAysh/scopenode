# Architecture Deepening Implementation Plan

**Status:** completed and retained as a historical implementation plan.

The current code contains the coverage store/outcomes, filter plan, shared query
front door, decode-quality model, E2Store module, archive codec, and explicit
pipeline sink seams proposed here. `source_manifest.rs` was not created as a
separate file; source manifest ownership remains in `source.rs`. Unchecked boxes
below describe the original execution sequence, not outstanding work.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deepen scopenode's query, filter, decode-quality, and ERA1 source modules so local scope correctness is explicit and easier to test.

**Architecture:** Add small public interfaces that hide larger implementation policy: `CoverageStore`/coverage outcomes for local scope completeness, `FilterPlan` for provider filter normalization, `DecodeQuality` for lossy decode/projection outcomes, and focused ERA1 source modules for manifest, e2store, and codec responsibilities. Keep existing CLI/RPC/storage behavior stable unless a test proves current behavior is silent or incomplete.

**Tech Stack:** Rust workspace, `sqlx` SQLite migrations, `jsonrpsee`, `axum`, `alloy`, `tokio`, `cargo test`.

---

## File Structure

- Create `crates/scopenode-storage/src/coverage.rs`: coverage write/read interface and `Db` methods.
- Modify `crates/scopenode-storage/src/lib.rs`: export coverage types.
- Create `crates/scopenode-storage/src/migrations/008_coverage.sql`: persist per-contract covered ranges and skipped blocks.
- Modify `crates/scopenode-core/src/era_pipeline.rs`: record successful and skipped block coverage through a new seam.
- Modify `crates/scopenode-storage/src/sink.rs`: adapt `Db` to the pipeline coverage interface.
- Modify `crates/scopenode-storage/src/query.rs`: include coverage outcomes in event queries.
- Create `crates/scopenode-rpc/src/filter_plan.rs`: normalize JSON-RPC and REST filter inputs into storage queries or explicit unsupported outcomes.
- Modify `crates/scopenode-rpc/src/lib.rs`, `server.rs`, `rest.rs`: use `FilterPlan`.
- Create `crates/scopenode-core/src/decode_quality.rs`: model valid, lossy, invalid decode/projection quality.
- Modify `crates/scopenode-core/src/era1_reader.rs`, `abi.rs`, and `crates/scopenode-rpc/src/projection.rs`: return or preserve decode quality rather than hiding fallback policy.
- Create `crates/scopenode-core/src/source_manifest.rs`, `e2store.rs`, `era1_codec.rs`: split discovery, entry iteration, and decoding modules out of `source.rs`.
- Modify `crates/scopenode-core/src/source.rs` and `lib.rs`: re-export existing public functions through the new modules so callers stay stable.

---

### Task 1: Coverage-Aware Query Module

**Files:**
- Create: `crates/scopenode-storage/src/coverage.rs`
- Create: `crates/scopenode-storage/src/migrations/008_coverage.sql`
- Modify: `crates/scopenode-storage/src/lib.rs`
- Modify: `crates/scopenode-storage/src/query.rs`
- Modify: `crates/scopenode-core/src/era_pipeline.rs`
- Modify: `crates/scopenode-storage/src/sink.rs`

- [ ] **Step 1: Write the failing test**

Add a storage behavior test showing an indexed contract with events outside the covered range is not silently answered:

```rust
#[tokio::test]
async fn query_reports_missing_coverage_for_indexed_contract_range() {
    let (db, _guard) = open_test_db().await;
    db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
    db.record_covered_range(CONTRACT, 100, 110).await.unwrap();

    let outcome = db
        .query_events(&EventQuery {
            contract: Some(CONTRACT.to_string()),
            from_block: Some(100),
            to_block: Some(120),
            limit: 100,
            ..EventQuery::default()
        })
        .await
        .unwrap();

    assert!(matches!(outcome, EventQueryOutcome::MissingCoverage { .. }));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p scopenode-storage query_reports_missing_coverage_for_indexed_contract_range`
Expected: FAIL because `record_covered_range` and `MissingCoverage` do not exist.

- [ ] **Step 3: Write minimal implementation**

Add a `covered_ranges` table, `Db::record_covered_range`, `Db::is_range_covered`, and `EventQueryOutcome::MissingCoverage`. Query checks coverage only when `contract`, `from_block`, and `to_block` are all present.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p scopenode-storage query_reports_missing_coverage_for_indexed_contract_range`
Expected: PASS.

- [ ] **Step 5: Add pipeline coverage test**

Add a core/storage integration test showing `run_era1_scopes` records coverage for processed blocks through a coverage seam.

- [ ] **Step 6: Run focused tests**

Run: `cargo test -p scopenode-storage coverage query::`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/scopenode-storage/src/coverage.rs crates/scopenode-storage/src/lib.rs crates/scopenode-storage/src/query.rs crates/scopenode-storage/src/migrations/008_coverage.sql crates/scopenode-core/src/era_pipeline.rs crates/scopenode-storage/src/sink.rs
git commit -m "feat(storage): make query coverage explicit"
```

### Task 2: Ethereum Filter Normalization Module

**Files:**
- Create: `crates/scopenode-rpc/src/filter_plan.rs`
- Modify: `crates/scopenode-rpc/src/lib.rs`
- Modify: `crates/scopenode-rpc/src/server.rs`
- Modify: `crates/scopenode-rpc/src/rest.rs`

- [ ] **Step 1: Write the failing test**

Add table tests for provider filters:

```rust
#[test]
fn rpc_filter_with_multiple_addresses_is_unsupported() {
    let filter = alloy::rpc::types::Filter::new()
        .address(vec![ADDR1.parse().unwrap(), ADDR2.parse().unwrap()]);
    let plan = FilterPlan::from_rpc_filter(&filter);
    assert!(matches!(plan, FilterPlan::Unsupported { .. }));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p scopenode-rpc filter_plan`
Expected: FAIL because `filter_plan` does not exist.

- [ ] **Step 3: Write minimal implementation**

Create `FilterPlan::{Query(EventQuery), Unsupported { reason }, MissingAddress}` and conversion functions for JSON-RPC and REST.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p scopenode-rpc filter_plan`
Expected: PASS.

- [ ] **Step 5: Wire RPC and REST adapters**

Use `FilterPlan` in `server.rs` and `rest.rs`, mapping unsupported filters to explicit user-facing errors.

- [ ] **Step 6: Run RPC tests**

Run: `cargo test -p scopenode-rpc`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/scopenode-rpc/src/filter_plan.rs crates/scopenode-rpc/src/lib.rs crates/scopenode-rpc/src/server.rs crates/scopenode-rpc/src/rest.rs
git commit -m "feat(rpc): normalize filters before querying storage"
```

### Task 3: Decode Quality Module

**Files:**
- Create: `crates/scopenode-core/src/decode_quality.rs`
- Modify: `crates/scopenode-core/src/lib.rs`
- Modify: `crates/scopenode-core/src/era1_reader.rs`
- Modify: `crates/scopenode-core/src/abi.rs`
- Modify: `crates/scopenode-rpc/src/projection.rs`

- [ ] **Step 1: Write the failing test**

Add a projection test proving invalid raw data is explicitly lossy rather than silently treated as valid.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p scopenode-rpc projection_quality`
Expected: FAIL because quality outcomes do not exist.

- [ ] **Step 3: Write minimal implementation**

Add `DecodeQuality::{Valid, Lossy { reason }, Invalid { reason }}` and projection helpers that report quality while keeping existing public projection functions for compatibility.

- [ ] **Step 4: Run focused tests**

Run: `cargo test -p scopenode-core decode_quality && cargo test -p scopenode-rpc projection_quality`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/scopenode-core/src/decode_quality.rs crates/scopenode-core/src/lib.rs crates/scopenode-core/src/era1_reader.rs crates/scopenode-core/src/abi.rs crates/scopenode-rpc/src/projection.rs
git commit -m "feat(core): expose lossy decode quality"
```

### Task 4: ERA1 Reader / Codec Split

**Files:**
- Create: `crates/scopenode-core/src/source_manifest.rs`
- Create: `crates/scopenode-core/src/e2store.rs`
- Create: `crates/scopenode-core/src/era1_codec.rs`
- Modify: `crates/scopenode-core/src/source.rs`
- Modify: `crates/scopenode-core/src/lib.rs`
- Modify: `crates/scopenode-core/src/era1_reader.rs`

- [ ] **Step 1: Write characterization tests**

Run current source tests before splitting:

```bash
cargo test -p scopenode-core source:: era1_reader::
```

Expected: PASS before refactor.

- [ ] **Step 2: Move codec behavior behind `era1_codec`**

Move `decode_era1_header`, `decode_era1_receipts`, and `decode_era1_tx_hashes` to `era1_codec`, re-exporting public compatibility paths.

- [ ] **Step 3: Run characterization tests**

Run: `cargo test -p scopenode-core source:: era1_reader::`
Expected: PASS.

- [ ] **Step 4: Move e2store behavior behind `e2store`**

Move entry-reading and tuple iteration into `e2store`, preserving existing `source` re-exports for callers.

- [ ] **Step 5: Run characterization tests**

Run: `cargo test -p scopenode-core source:: era1_reader::`
Expected: PASS.

- [ ] **Step 6: Move manifest scanning behind `source_manifest`**

Move directory scan, checksum policy, and range manifest code into `source_manifest`, preserving `scan_era1_source`.

- [ ] **Step 7: Run characterization tests**

Run: `cargo test -p scopenode-core source:: era1_reader::`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/scopenode-core/src/source_manifest.rs crates/scopenode-core/src/e2store.rs crates/scopenode-core/src/era1_codec.rs crates/scopenode-core/src/source.rs crates/scopenode-core/src/lib.rs crates/scopenode-core/src/era1_reader.rs
git commit -m "refactor(core): split ERA1 source modules"
```

### Task 5: Full Verification and Push

**Files:**
- All changed Rust files.

- [ ] **Step 1: Run full workspace tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 2: Check formatting and lint if available**

Run: `cargo fmt --all -- --check`
Expected: PASS.

- [ ] **Step 3: Push main**

Run: `git push origin main`
Expected: push succeeds.
