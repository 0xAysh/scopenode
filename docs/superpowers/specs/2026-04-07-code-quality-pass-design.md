# Code Quality Pass — Design Spec

**Date:** 2026-04-07
**Scope:** 10 targeted fixes across performance, correctness, schema, testing, and cleanup.

---

## Overview

Seven logical steps, all independent. Each maps to one or more of the original 10 items.

---

## Step 1 — Silent failures → hard errors (items #2, #7, live.rs:91)

**Problem:** `h.hash.parse().unwrap_or_default()` and `h.receipts_root.parse().unwrap_or_default()` in `bloom_scan` silently produce `B256::ZERO` on malformed stored strings. A zero hash passed to `GetReceipts` causes the peer to return nothing; a zero `receipts_root` causes every Merkle check to fail. Both result in blocks stuck in `pending_retry` with no diagnostic.

**Fix:**
- `pipeline.rs` `bloom_scan`: replace both `unwrap_or_default()` calls with `?`-propagated errors via `.map_err(|e| CoreError::Internal(format!(...)))`.
- `live.rs` reorg seed loop (line 91): log a `warn!` and skip the bad header — seeding is best-effort and should not abort startup.

**Files:** `crates/scopenode-core/src/pipeline.rs`, `crates/scopenode-core/src/live.rs`

---

## Step 2 — N+1 query fix (item #1)

**Problem:** `fetch_and_store` calls `self.db.is_fetched(block_num, &addr_str)` inside a loop — one `SELECT` per bloom candidate. At 10,000 candidates that's 10,000 round trips before a single receipt is fetched.

**Fix:**
- Add `pub async fn get_fetched_set(&self, contract: &str) -> Result<HashSet<u64>, DbError>` to `Db`. One query: `SELECT block_number FROM bloom_candidates WHERE contract = ? AND fetched = 1`.
- In `fetch_and_store`: call `get_fetched_set` once before the loop, then filter `candidates` in memory with `.iter().filter(|(n, _, _)| !fetched_set.contains(n))`.
- `is_fetched` is kept — used in tests and `run_retry`.

**Files:** `crates/scopenode-storage/src/db.rs`, `crates/scopenode-core/src/pipeline.rs`

---

## Step 3 — Cursor rollback bug (item #3)

**Problem:** In `fetch_and_store`, when `remaining` is empty (all blocks already fetched on resume), `to` falls back to `contract.from_block`. The subsequent `upsert_sync_cursor` writes that stale value into `receipts_done_to`, potentially rolling the cursor backward.

**Fix:** Wrap the cursor upsert in `if let Some(&(last_block, _, _)) = remaining.last()`. When `remaining` is empty, skip the upsert entirely — the existing cursor is preserved.

**Files:** `crates/scopenode-core/src/pipeline.rs`

---

## Step 4 — Schema migration (items #6, #8)

**New file:** `crates/scopenode-storage/src/migrations/003_schema_hardening.sql`

### 4a — Bloom candidates index (item #8)
`get_unfetched_candidates` queries `WHERE contract = ? AND fetched = 0`. The existing PK is `(block_number, contract)`, so SQLite scans the whole table and filters. Fix:
```sql
CREATE INDEX IF NOT EXISTS idx_bloom_contract ON bloom_candidates(contract, fetched);
```

### 4b — Events UNIQUE constraint (item #6)
The current `UNIQUE (tx_hash, log_index)` depends on a synthetic `tx_hash = keccak256(block_hash || tx_index)`. Two blocks with a `B256::ZERO` hash (common in tests) can collide, causing silent `INSERT OR IGNORE` drops. Replace with a semantically correct constraint.

SQLite requires table recreation to change a UNIQUE constraint:
```sql
CREATE TABLE events_new (
    -- same columns as events --
    UNIQUE (block_number, tx_index, log_index)
);
INSERT INTO events_new SELECT * FROM events;
DROP TABLE events;
ALTER TABLE events_new RENAME TO events;
CREATE INDEX IF NOT EXISTS idx_events_lookup ON events(contract, event_name, block_number) WHERE reorged = 0;
```

**Files:** `crates/scopenode-storage/src/migrations/003_schema_hardening.sql`

---

## Step 5 — SourcifyClient timeout (item #10)

**Problem:** `reqwest::Client::new()` has no timeout. If Sourcify is slow or down, the ABI fetch stage hangs forever.

**Fix:**
```rust
http: reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(30))
    .build()
    .expect("reqwest client"),
```

**Files:** `crates/scopenode-core/src/abi.rs`

---

## Step 6 — receipts.rs tests (item #9)

**Problem:** `receipts.rs` is the most security-critical file (Merkle trie verification) and has zero tests.

**Three tests:**
1. `empty_receipts_match_empty_root` — `verify_receipts(&[], EMPTY_ROOT_HASH, 0)` → `Ok(())`
2. `empty_receipts_wrong_root` — `verify_receipts(&[], B256::from([1u8;32]), 0)` → `Err(RootMismatch)`
3. `tampered_receipt_fails` — build a valid single-receipt trie, compute its root, then pass a receipt with a mutated `cumulative_gas_used` and assert `Err(RootMismatch)`

**Files:** `crates/scopenode-core/src/receipts.rs`

---

## Step 7 — phases/ → docs/phases/ (item #4)

**Problem:** Four planning markdown files sit at the repo root (`phases/PHASE_01_mvp.md`, etc.), giving the repo an unfinished feel.

**Fix:** `git mv phases/ docs/phases/` — preserves git history. Update any cross-references in `README.md`. Update `MEMORY.md` pointer from `phases/` to `docs/phases/`.

**Files:** `phases/` → `docs/phases/`, `README.md`, `/Users/0xayush/.claude/projects/.../memory/MEMORY.md`

---

## Sequencing

Steps are independent and can be executed in any order. Recommended order: 1 → 2 → 3 → 4 → 5 → 6 → 7. Commit after each step.

## Non-changes

- `is_fetched` on `Db` is kept (used in tests).
- No changes to `reorg.rs` or `live.rs` logic beyond the seed loop fix.
- No changes to public API or CLI surface.
