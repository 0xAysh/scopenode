# Code Quality Pass Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix 10 correctness, performance, and hygiene issues across the scopenode codebase — silent parse failures, an N+1 query, a cursor-rollback bug, missing schema index, fragile UNIQUE constraint, missing test coverage, a missing HTTP timeout, and stale planning docs at root.

**Architecture:** Seven independent tasks in dependency order. Tasks 1–3 fix runtime correctness bugs. Task 4 is a single SQLite migration file. Task 5 is a one-liner reliability fix. Task 6 adds three focused tests. Task 7 is a filesystem move with a memory update.

**Tech Stack:** Rust, alloy (consensus + rpc types), sqlx/SQLite, tokio, reqwest, alloy-trie

---

## File Map

| File | Change |
|------|--------|
| `crates/scopenode-core/src/error.rs` | Add `CoreError::Internal(String)` variant |
| `crates/scopenode-core/src/pipeline.rs` | Fix bloom_scan parse errors, fix N+1, fix cursor rollback |
| `crates/scopenode-core/src/live.rs` | Fix reorg seed loop parse error |
| `crates/scopenode-storage/src/db.rs` | Add `get_fetched_set` method |
| `crates/scopenode-storage/src/migrations/003_schema_hardening.sql` | New migration: bloom index + events UNIQUE |
| `crates/scopenode-core/src/abi.rs` | Add 30s timeout to SourcifyClient |
| `crates/scopenode-core/src/receipts.rs` | Add 3 tests |
| `phases/` → `docs/phases/` | `git mv` + update memory |

---

## Task 1 — Add `CoreError::Internal` and fix silent parse failures

**Files:**
- Modify: `crates/scopenode-core/src/error.rs`
- Modify: `crates/scopenode-core/src/pipeline.rs` (bloom_scan, lines ~321-322)
- Modify: `crates/scopenode-core/src/live.rs` (reorg seed loop, line ~91)

### Background
`bloom_scan` calls `.parse().unwrap_or_default()` on the block hash and receipts_root strings loaded from SQLite. A malformed stored value silently becomes `B256::ZERO`. A zero block hash sent to the peer returns nothing; a zero receipts_root causes every Merkle check to fail. Both leave blocks stuck in `pending_retry` with no log output explaining why.

The live-sync reorg seed loop has the same pattern (line 91 of `live.rs`), but seeding is best-effort so we log + skip rather than aborting.

- [ ] **Step 1: Add `CoreError::Internal` variant**

In `crates/scopenode-core/src/error.rs`, add a new variant to `CoreError` after the `Io` variant:

```rust
#[error("Internal error: {0}")]
Internal(String),
```

The full enum becomes:

```rust
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("Network error: {0}")]
    Network(#[from] NetworkError),

    #[error("ABI error: {0}")]
    Abi(#[from] AbiError),

    #[error("Verification error: {0}")]
    Verify(#[from] VerifyError),

    #[error("Config error: {0}")]
    Config(#[from] ConfigError),

    #[error("Storage error: {0}")]
    Storage(#[from] scopenode_storage::DbError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}
```

- [ ] **Step 2: Fix bloom_scan parse errors (pipeline.rs)**

In `pipeline.rs`, inside `bloom_scan`, find the two `unwrap_or_default()` calls in the loop body (around line 321):

```rust
// BEFORE
let hash: B256 = h.hash.parse().unwrap_or_default();
let receipts_root: B256 = h.receipts_root.parse().unwrap_or_default();
```

Replace with:

```rust
// AFTER
let hash: B256 = h.hash.parse().map_err(|_| {
    CoreError::Internal(format!(
        "block {}: malformed hash in DB: {:?}",
        h.number, h.hash
    ))
})?;
let receipts_root: B256 = h.receipts_root.parse().map_err(|_| {
    CoreError::Internal(format!(
        "block {}: malformed receipts_root in DB: {:?}",
        h.number, h.receipts_root
    ))
})?;
```

- [ ] **Step 3: Fix reorg seed loop parse error (live.rs)**

In `live.rs`, find the reorg seed block (around line 89–93):

```rust
// BEFORE
self.reorg_detector.seed(headers.iter().map(|h| {
    let hash: B256 = h.hash.parse().unwrap_or_default();
    (h.number as u64, hash)
}));
```

Replace with (best-effort: warn + skip):

```rust
// AFTER
self.reorg_detector.seed(
    headers.iter().filter_map(|h| {
        match h.hash.parse::<B256>() {
            Ok(hash) => Some((h.number as u64, hash)),
            Err(e) => {
                warn!(
                    block = h.number,
                    err = %e,
                    "Malformed header hash in DB — skipping for reorg seed"
                );
                None
            }
        }
    }),
);
```

- [ ] **Step 4: Build and run tests**

```bash
cd /Users/0xayush/Projects/scopenode
cargo test -p scopenode-core 2>&1 | tail -20
```

Expected: all existing tests pass, no new failures. The `bloom_scan` and seed loop are not directly tested here — they'll be covered by end-to-end flow if invoked.

- [ ] **Step 5: Commit**

```bash
git add crates/scopenode-core/src/error.rs \
        crates/scopenode-core/src/pipeline.rs \
        crates/scopenode-core/src/live.rs
git commit -m "core: surface bloom_scan parse failures as CoreError::Internal"
```

---

## Task 2 — Fix N+1 query in fetch_and_store

**Files:**
- Modify: `crates/scopenode-storage/src/db.rs`
- Modify: `crates/scopenode-core/src/pipeline.rs` (fetch_and_store, lines ~373-377)

### Background
`fetch_and_store` calls `self.db.is_fetched(block_num, &addr_str)` inside a `for` loop — one `SELECT` per bloom candidate. With 10,000 candidates that's 10,000 round trips before a single receipt is fetched. Fix: load the full fetched set in one query and filter in memory.

- [ ] **Step 1: Add `get_fetched_set` to Db**

In `crates/scopenode-storage/src/db.rs`, add `use std::collections::HashSet;` to the imports at the top of the file.

Then add this method after `is_fetched` (around line 219):

```rust
/// Return the set of block numbers already fetched for a contract.
///
/// Used by `fetch_and_store` to filter candidates in one query instead of
/// N individual `is_fetched` calls. Returns block numbers where `fetched = 1`.
pub async fn get_fetched_set(&self, contract: &str) -> Result<HashSet<u64>, DbError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        block_number: i64,
    }

    let rows = sqlx::query_as::<_, Row>(
        r#"SELECT block_number FROM bloom_candidates WHERE contract = ? AND fetched = 1"#,
    )
    .bind(contract)
    .fetch_all(&self.pool)
    .await
    .map_err(|e| DbError::Query(e.to_string()))?;

    Ok(rows.into_iter().map(|r| r.block_number as u64).collect())
}
```

- [ ] **Step 2: Replace the per-block loop in fetch_and_store**

In `pipeline.rs`, inside `fetch_and_store`, replace the `for` loop that calls `is_fetched` per block (around lines 372–377):

```rust
// BEFORE
let mut remaining = Vec::new();
for &(block_num, block_hash, receipts_root) in candidates {
    if !self.db.is_fetched(block_num, &addr_str).await? {
        remaining.push((block_num, block_hash, receipts_root));
    }
}
```

Replace with:

```rust
// AFTER
let fetched_set = self.db.get_fetched_set(&addr_str).await?;
let remaining: Vec<(u64, B256, B256)> = candidates
    .iter()
    .copied()
    .filter(|(n, _, _)| !fetched_set.contains(n))
    .collect();
```

- [ ] **Step 3: Build and run tests**

```bash
cargo test -p scopenode-storage -p scopenode-core 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/scopenode-storage/src/db.rs \
        crates/scopenode-core/src/pipeline.rs
git commit -m "perf: replace N+1 is_fetched loop with get_fetched_set bulk query"
```

---

## Task 3 — Fix cursor rollback bug

**Files:**
- Modify: `crates/scopenode-core/src/pipeline.rs` (fetch_and_store, lines ~455-469)

### Background
When resuming after a full sync (`remaining` is empty), the cursor upsert fallback sets `to = contract.from_block`, potentially rolling `receipts_done_to` backward. When `remaining` is empty there's nothing to do — skip the upsert entirely.

- [ ] **Step 1: Wrap cursor upsert in a `remaining.last()` guard**

In `pipeline.rs` `fetch_and_store`, find the cursor upsert block at the end of the function (around lines 455–469):

```rust
// BEFORE
let to = contract.to_block.unwrap_or(
    remaining
        .last()
        .map(|&(n, _, _)| n)
        .unwrap_or(contract.from_block),
);
let cursor = SyncCursor {
    contract: addr_str.clone(),
    from_block: contract.from_block,
    to_block: contract.to_block,
    headers_done_to: Some(to),
    bloom_done_to: Some(to),
    receipts_done_to: Some(to),
};
let _ = self.db.upsert_sync_cursor(&cursor).await;
```

Replace with:

```rust
// AFTER — skip cursor update if no work was done (all blocks already fetched on resume)
if let Some(&(last_block, _, _)) = remaining.last() {
    let to = contract.to_block.unwrap_or(last_block);
    let cursor = SyncCursor {
        contract: addr_str.clone(),
        from_block: contract.from_block,
        to_block: contract.to_block,
        headers_done_to: Some(to),
        bloom_done_to: Some(to),
        receipts_done_to: Some(to),
    };
    let _ = self.db.upsert_sync_cursor(&cursor).await;
}
```

- [ ] **Step 2: Build and run tests**

```bash
cargo test -p scopenode-core 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/scopenode-core/src/pipeline.rs
git commit -m "fix: skip sync_cursor upsert when remaining is empty to prevent rollback"
```

---

## Task 4 — Schema migration: bloom index + events UNIQUE constraint

**Files:**
- Create: `crates/scopenode-storage/src/migrations/003_schema_hardening.sql`

### Background
Two schema issues:
1. `get_unfetched_candidates` queries `WHERE contract = ? AND fetched = 0`. The PK is `(block_number, contract)`, so SQLite scans the whole table. An index on `(contract, fetched)` fixes this.
2. The events `UNIQUE (tx_hash, log_index)` depends on a synthetic `tx_hash = keccak256(block_hash || tx_index)`. Two blocks with `B256::ZERO` hash (common in tests) produce the same synthetic hash, causing silent `INSERT OR IGNORE` drops. `UNIQUE (block_number, tx_index, log_index)` is semantically correct and independent of the synthetic hash.

SQLite does not support `DROP CONSTRAINT` — the events table must be recreated.

- [ ] **Step 1: Create the migration file**

Create `crates/scopenode-storage/src/migrations/003_schema_hardening.sql`:

```sql
-- Add covering index for the contract+fetched query pattern used by
-- get_unfetched_candidates. Without this, SQLite scans the whole table.
CREATE INDEX IF NOT EXISTS idx_bloom_contract ON bloom_candidates(contract, fetched);

-- Recreate the events table with a semantically correct UNIQUE constraint.
-- The old UNIQUE (tx_hash, log_index) relied on a synthetic tx_hash derived from
-- block_hash; two blocks with B256::ZERO hash (common in tests) could collide.
-- The new UNIQUE (block_number, tx_index, log_index) is the canonical identity
-- of a log: its position in the block, independent of any synthetic hash.
-- SQLite does not support ALTER TABLE ... DROP CONSTRAINT, so we recreate.
CREATE TABLE events_new (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    contract        TEXT NOT NULL,
    event_name      TEXT NOT NULL,
    topic0          TEXT NOT NULL,
    block_number    INTEGER NOT NULL,
    block_hash      TEXT NOT NULL,
    tx_hash         TEXT NOT NULL,
    tx_index        INTEGER NOT NULL,
    log_index       INTEGER NOT NULL,
    raw_topics      TEXT NOT NULL,
    raw_data        TEXT NOT NULL,
    decoded         TEXT NOT NULL,
    source          TEXT NOT NULL,
    reorged         INTEGER NOT NULL DEFAULT 0,
    UNIQUE (block_number, tx_index, log_index)
);

INSERT INTO events_new
    SELECT id, contract, event_name, topic0, block_number, block_hash,
           tx_hash, tx_index, log_index, raw_topics, raw_data, decoded,
           source, reorged
    FROM events;

DROP TABLE events;
ALTER TABLE events_new RENAME TO events;

-- Recreate the lookup index that was on the original table.
CREATE INDEX IF NOT EXISTS idx_events_lookup
    ON events(contract, event_name, block_number) WHERE reorged = 0;
```

- [ ] **Step 2: Verify sqlx picks up the migration**

sqlx uses the `sqlx::migrate!` macro which scans the migrations directory at compile time. Run the full build to confirm it picks up `003`:

```bash
cargo build -p scopenode-storage 2>&1 | grep -E "error|warning|003"
```

Expected: builds clean. If sqlx complains about migration ordering or checksum, check that the file name sorts after `002_bloom_hex.sql`.

- [ ] **Step 3: Run storage tests**

```bash
cargo test -p scopenode-storage 2>&1 | tail -20
```

Expected: all tests pass. The migration runs on each test DB created via `Db::open`.

- [ ] **Step 4: Commit**

```bash
git add crates/scopenode-storage/src/migrations/003_schema_hardening.sql
git commit -m "storage: add bloom index and fix events UNIQUE to (block_number, tx_index, log_index)"
```

---

## Task 5 — SourcifyClient HTTP timeout

**Files:**
- Modify: `crates/scopenode-core/src/abi.rs` (SourcifyClient::new, around line 136)

### Background
`reqwest::Client::new()` has no timeout. If Sourcify is slow or down, the ABI fetch hangs indefinitely with no feedback to the user.

- [ ] **Step 1: Add timeout to SourcifyClient::new()**

In `abi.rs`, find `SourcifyClient::new()` (around line 134):

```rust
// BEFORE
impl SourcifyClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}
```

Replace with:

```rust
// AFTER
impl SourcifyClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }
}
```

- [ ] **Step 2: Build**

```bash
cargo build -p scopenode-core 2>&1 | grep -E "^error"
```

Expected: clean build.

- [ ] **Step 3: Commit**

```bash
git add crates/scopenode-core/src/abi.rs
git commit -m "fix: add 30s timeout to SourcifyClient HTTP client"
```

---

## Task 6 — Tests for receipts.rs verify_receipts

**Files:**
- Modify: `crates/scopenode-core/src/receipts.rs` (add `#[cfg(test)] mod tests` at bottom)

### Background
`receipts.rs` is the most security-critical file in the codebase — Merkle trie verification is the primary tamper-resistance mechanism — and has zero tests. Add three:

1. Empty receipts + EMPTY_ROOT_HASH → `Ok`
2. Empty receipts + wrong root → `Err(RootMismatch)`
3. Tampered receipt (mutated gas) → `Err(RootMismatch)` against the original root

For test 3: we don't know the trie root of a synthetic receipt in advance, so we derive it by calling `verify_receipts` with a known-bad expected root (`B256::ZERO`) and extracting the `computed` field from the returned `RootMismatch`. Then we pass a mutated receipt against that root and assert it fails.

- [ ] **Step 1: Add tests module to receipts.rs**

Append to `crates/scopenode-core/src/receipts.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope};
    use alloy::rpc::types::{Log as RpcLog, TransactionReceipt};
    use alloy_primitives::{Address, B256};
    use alloy_trie::EMPTY_ROOT_HASH;

    /// Build a minimal legacy receipt with no logs.
    /// Only `cumulative_gas_used` varies between calls — enough to change the trie root.
    fn make_receipt(cumulative_gas_used: u64) -> TransactionReceipt {
        let receipt: Receipt<RpcLog> = Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used,
            logs: vec![],
        };
        let inner: ReceiptEnvelope<RpcLog> = ReceiptEnvelope::Legacy(receipt.into());
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            gas_used: cumulative_gas_used,
            effective_gas_price: 0,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        }
    }

    #[test]
    fn empty_receipts_match_empty_root() {
        assert!(verify_receipts(&[], EMPTY_ROOT_HASH, 0).is_ok());
    }

    #[test]
    fn empty_receipts_wrong_root_fails() {
        let wrong_root = B256::from([1u8; 32]);
        let result = verify_receipts(&[], wrong_root, 0);
        assert!(
            matches!(result, Err(VerifyError::RootMismatch { expected, computed, .. })
                if expected == wrong_root && computed == EMPTY_ROOT_HASH),
            "expected RootMismatch with computed=EMPTY_ROOT_HASH, got {:?}",
            result
        );
    }

    #[test]
    fn tampered_receipt_fails() {
        let receipt_a = make_receipt(21_000);

        // Derive root_a: we don't know it in advance, so we use the error.
        let root_a = match verify_receipts(&[receipt_a.clone()], B256::ZERO, 1) {
            Err(VerifyError::RootMismatch { computed, .. }) => computed,
            other => panic!("expected RootMismatch when probing root, got {:?}", other),
        };

        // Sanity: receipt_a passes against its own root.
        assert!(
            verify_receipts(&[receipt_a], root_a, 1).is_ok(),
            "receipt_a should pass against root_a"
        );

        // Tampered: different cumulative_gas_used → different trie encoding → different root.
        let receipt_b = make_receipt(42_000);
        assert!(
            verify_receipts(&[receipt_b], root_a, 1).is_err(),
            "tampered receipt should fail against root_a"
        );
    }
}
```

> **Note on alloy types:** `Receipt.cumulative_gas_used` is `u64` in alloy 1.x. If you get a type mismatch, the field type may be `u128` — add `as u128` to the `cumulative_gas_used` argument in the `Receipt` literal, not to `TransactionReceipt.gas_used` (which is `u64`).

- [ ] **Step 2: Run the new tests**

```bash
cargo test -p scopenode-core receipts 2>&1
```

Expected output (3 tests):
```
test receipts::tests::empty_receipts_match_empty_root ... ok
test receipts::tests::empty_receipts_wrong_root_fails ... ok
test receipts::tests::tampered_receipt_fails ... ok
```

- [ ] **Step 3: Commit**

```bash
git add crates/scopenode-core/src/receipts.rs
git commit -m "test: add verify_receipts tests (empty root, wrong root, tampered receipt)"
```

---

## Task 7 — Move phases/ → docs/phases/

**Files:**
- Move: `phases/` → `docs/phases/`
- Modify: `/Users/0xayush/.claude/projects/-Users-0xayush-Projects-scopenode/memory/MEMORY.md`

### Background
Four planning documents sit at the repo root (`phases/PHASE_01_mvp.md` etc.), making the project look unfinished to anyone who clones it. Move them to `docs/phases/` with `git mv` to preserve history. The README has no references to `phases/`; only `MEMORY.md` needs updating.

- [ ] **Step 1: Create docs/phases/ and move files**

```bash
mkdir -p /Users/0xayush/Projects/scopenode/docs/phases
git mv /Users/0xayush/Projects/scopenode/phases/PHASE_01_mvp.md \
       /Users/0xayush/Projects/scopenode/docs/phases/PHASE_01_mvp.md
git mv /Users/0xayush/Projects/scopenode/phases/PHASE_02_p2p.md \
       /Users/0xayush/Projects/scopenode/docs/phases/PHASE_02_p2p.md
git mv /Users/0xayush/Projects/scopenode/phases/PHASE_03_live.md \
       /Users/0xayush/Projects/scopenode/docs/phases/PHASE_03_live.md
git mv /Users/0xayush/Projects/scopenode/phases/PHASE_04_developer.md \
       /Users/0xayush/Projects/scopenode/docs/phases/PHASE_04_developer.md
```

Verify the `phases/` directory is now empty:

```bash
ls /Users/0xayush/Projects/scopenode/phases/ 2>/dev/null && echo "WARN: phases/ not empty" || echo "OK: phases/ removed"
```

- [ ] **Step 2: Remove the empty phases/ directory**

```bash
rmdir /Users/0xayush/Projects/scopenode/phases/
```

- [ ] **Step 3: Update MEMORY.md**

In `/Users/0xayush/.claude/projects/-Users-0xayush-Projects-scopenode/memory/MEMORY.md`, find the lines (around lines 53–55):

```
- phases/PHASE_01_mvp.md
- phases/PHASE_02_trustless.md
- phases/PHASE_03_production.md
```

Replace with:

```
- docs/phases/PHASE_01_mvp.md
- docs/phases/PHASE_02_p2p.md
- docs/phases/PHASE_03_live.md
- docs/phases/PHASE_04_developer.md
```

(The MEMORY.md had stale names; update to the actual file names.)

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "chore: move phases/ planning docs to docs/phases/"
```

---

## Final verification

- [ ] **Run the full test suite**

```bash
cargo test --workspace 2>&1 | tail -30
```

Expected: all tests pass across all crates.

- [ ] **Confirm phases/ is gone from root**

```bash
ls /Users/0xayush/Projects/scopenode/ | grep phases && echo "FAIL" || echo "OK"
```

Expected: `OK`
