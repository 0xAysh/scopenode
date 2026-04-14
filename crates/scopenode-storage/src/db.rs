//! Database access methods — the primary interface to the SQLite storage layer.
//!
//! All operations are `async` and use the `SqlitePool` internally. The pool is
//! `Clone`-safe (internally `Arc`-wrapped by `sqlx`) so `Db` can be passed to
//! multiple tasks without locking.
//!
//! **WAL mode** is enabled at open time for safe concurrent reads while the pipeline
//! writes. Without WAL, readers would block writers and vice versa.
//!
//! **Idempotency**: all insert operations use `INSERT OR IGNORE` (or
//! `ON CONFLICT DO UPDATE`) so the pipeline can be interrupted and resumed without
//! duplicating data.

use crate::error::DbError;
use crate::models::{ContractRow, StoredEvent, StoredHeader};
use alloy_primitives::{Bloom, B256};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::path::PathBuf;
use tracing::info;

/// The main database handle.
///
/// `Clone`-safe — internally uses `sqlx::SqlitePool` which is `Arc`-wrapped.
/// Pass this by value (cloning is cheap) to async tasks and the RPC server.
#[derive(Clone)]
pub struct Db {
    pub(crate) pool: SqlitePool,
    #[allow(dead_code)]
    path: PathBuf,
}

impl Db {
    /// Open (or create) the SQLite database at `path` and run migrations.
    ///
    /// Enables WAL journal mode for concurrent read safety — this allows the
    /// JSON-RPC server to read events while the pipeline is writing new ones.
    /// Runs embedded migrations from `src/migrations/` automatically; the DB
    /// schema is always up to date after `open` returns.
    pub async fn open(path: PathBuf) -> Result<Self, DbError> {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
        use std::str::FromStr;

        let options = SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            path.display()
        ))
        .map_err(|e| DbError::Open(e.to_string()))?
        .create_if_missing(true)
        // WAL mode: writers don't block readers and readers don't block writers.
        .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePool::connect_with(options)
            .await
            .map_err(|e| DbError::Open(e.to_string()))?;

        // Run embedded migrations from src/migrations/ — these are baked into
        // the binary at compile time by the sqlx::migrate! macro.
        sqlx::migrate!("src/migrations")
            .run(&pool)
            .await
            .map_err(|e| DbError::Migration(e.to_string()))?;

        info!(path = %path.display(), "Database opened");

        Ok(Self { pool, path })
    }

    // ─── Headers ─────────────────────────────────────────────────────────────

    /// Insert a block header. Uses `INSERT OR IGNORE` so re-running is safe
    /// if the header already exists (idempotent).
    pub async fn insert_header(&self, h: &StoredHeader) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT OR IGNORE INTO headers
               (number, hash, parent_hash, timestamp, receipts_root, logs_bloom, gas_used, base_fee)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(h.number)
        .bind(&h.hash)
        .bind(&h.parent_hash)
        .bind(h.timestamp)
        .bind(&h.receipts_root)
        .bind(&h.logs_bloom)
        .bind(h.gas_used)
        .bind(h.base_fee)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// Fetch a single header by block number.
    ///
    /// Returns `None` if the header has not yet been synced. Used by the
    /// receipt verification stage to look up the expected `receipts_root`.
    pub async fn get_header(&self, block_num: u64) -> Result<Option<StoredHeader>, DbError> {
        let n = block_num as i64;
        sqlx::query_as::<_, StoredHeader>(
            r#"SELECT number, hash, parent_hash, timestamp, receipts_root, logs_bloom, gas_used, base_fee
               FROM headers WHERE number = ?"#,
        )
        .bind(n)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))
    }

    /// Fetch all headers in `[from, to]` in ascending order.
    ///
    /// Returns [`HeaderRow`] which has `logs_bloom` already parsed from the hex
    /// string into an `alloy_primitives::Bloom` — the bloom scanner can use it
    /// directly without re-parsing.
    pub async fn get_headers(&self, from: u64, to: u64) -> Result<Vec<HeaderRow>, DbError> {
        let from_i = from as i64;
        let to_i = to as i64;

        #[derive(sqlx::FromRow)]
        struct RawRow {
            number: i64,
            hash: String,
            parent_hash: String,
            timestamp: i64,
            receipts_root: String,
            logs_bloom: String,
            gas_used: i64,
            base_fee: Option<i64>,
        }

        let rows = sqlx::query_as::<_, RawRow>(
            r#"SELECT number, hash, parent_hash, timestamp, receipts_root, logs_bloom, gas_used, base_fee
               FROM headers WHERE number >= ? AND number <= ? ORDER BY number ASC"#,
        )
        .bind(from_i)
        .bind(to_i)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| HeaderRow {
                number: r.number,
                hash: r.hash,
                parent_hash: r.parent_hash,
                timestamp: r.timestamp,
                receipts_root: r.receipts_root,
                logs_bloom: bloom_from_hex(&r.logs_bloom),
                gas_used: r.gas_used,
                base_fee: r.base_fee,
            })
            .collect())
    }

    /// Return the highest block number stored in the headers table.
    ///
    /// Used by `eth_blockNumber` in the JSON-RPC server to report how far the
    /// node has synced. Returns `None` if no headers have been stored yet.
    pub async fn latest_block_number(&self) -> Result<Option<u64>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            max_num: Option<i64>,
        }
        let row = sqlx::query_as::<_, Row>("SELECT MAX(number) as max_num FROM headers")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.max_num.map(|n| n as u64))
    }

    // ─── Bloom candidates ─────────────────────────────────────────────────────

    /// Record that a block passed the bloom filter check for a contract.
    ///
    /// `fetched` and `pending_retry` default to `0` (false). Uses `INSERT OR IGNORE`
    /// so re-running the bloom scan doesn't reset the `fetched` flag on blocks that
    /// have already been successfully processed.
    pub async fn insert_bloom_candidate(
        &self,
        block_num: u64,
        block_hash: B256,
        contract: &str,
    ) -> Result<(), DbError> {
        let n = block_num as i64;
        let hash = block_hash.to_string();
        sqlx::query(
            r#"INSERT OR IGNORE INTO bloom_candidates (block_number, block_hash, contract)
               VALUES (?, ?, ?)"#,
        )
        .bind(n)
        .bind(&hash)
        .bind(contract)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// Check if receipts for this block + contract have already been fetched and verified.
    ///
    /// Used to skip already-processed blocks when resuming after an interruption.
    /// Returns `false` if the block is not in `bloom_candidates` at all.
    pub async fn is_fetched(&self, block_num: u64, contract: &str) -> Result<bool, DbError> {
        let n = block_num as i64;

        #[derive(sqlx::FromRow)]
        struct Row {
            fetched: i64,
        }

        let row = sqlx::query_as::<_, Row>(
            r#"SELECT fetched FROM bloom_candidates WHERE block_number = ? AND contract = ?"#,
        )
        .bind(n)
        .bind(contract)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.map(|r| r.fetched != 0).unwrap_or(false))
    }

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

    /// Mark a bloom candidate as successfully fetched.
    ///
    /// Sets `fetched = 1, pending_retry = 0`. Called after Merkle verification
    /// succeeds and events are inserted.
    pub async fn mark_fetched(&self, block_num: u64, contract: &str) -> Result<(), DbError> {
        let n = block_num as i64;
        sqlx::query(
            r#"UPDATE bloom_candidates SET fetched = 1 WHERE block_number = ? AND contract = ?"#,
        )
        .bind(n)
        .bind(contract)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// Check if a bloom candidate is marked for retry (`pending_retry = 1`).
    ///
    /// Returns `false` if the row doesn't exist. Used in tests to assert that a
    /// failed-receipt block was properly flagged for retry.
    pub async fn is_pending_retry(&self, block_num: u64, contract: &str) -> Result<bool, DbError> {
        let n = block_num as i64;

        #[derive(sqlx::FromRow)]
        struct Row {
            pending_retry: i64,
        }

        let row = sqlx::query_as::<_, Row>(
            r#"SELECT pending_retry FROM bloom_candidates WHERE block_number = ? AND contract = ?"#,
        )
        .bind(n)
        .bind(contract)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.map(|r| r.pending_retry != 0).unwrap_or(false))
    }

    /// Mark a bloom candidate as failed — will be retried by `scopenode retry` (Phase 3a).
    ///
    /// Sets `pending_retry = 1`. Called when receipt fetch fails or Merkle verification
    /// fails (indicating the peer sent tampered data). The block is not marked as
    /// fetched so it remains in the unfetched queue.
    pub async fn mark_retry(&self, block_num: u64, contract: &str) -> Result<(), DbError> {
        let n = block_num as i64;
        sqlx::query(
            r#"UPDATE bloom_candidates SET pending_retry = 1 WHERE block_number = ? AND contract = ?"#,
        )
        .bind(n)
        .bind(contract)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// Return bloom candidates that haven't been fetched yet for a contract.
    ///
    /// Ordered by block number (ascending) for sequential processing.
    /// Used by the retry mechanism (Phase 3a) and for resuming interrupted receipt fetches.
    pub async fn get_unfetched_candidates(
        &self,
        contract: &str,
    ) -> Result<Vec<(u64, B256)>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            block_number: i64,
            block_hash: String,
        }

        let rows = sqlx::query_as::<_, Row>(
            r#"SELECT block_number, block_hash FROM bloom_candidates
               WHERE contract = ? AND fetched = 0 ORDER BY block_number ASC"#,
        )
        .bind(contract)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let hash: B256 = r.block_hash.parse().ok()?;
                Some((r.block_number as u64, hash))
            })
            .collect())
    }

    // ─── Events ────────────────────────────────────────────────────────────────

    /// Bulk insert decoded events into SQLite inside a single transaction.
    ///
    /// Wrapping N inserts in one transaction is dramatically faster than N
    /// individual transactions (SQLite flushes to disk per-commit by default).
    /// Uses `INSERT OR IGNORE` on `(block_number, tx_index, log_index)` so
    /// re-running the pipeline for an already-processed block never creates duplicates.
    /// The transaction is all-or-nothing: if any insert fails, none are committed.
    pub async fn insert_events(&self, events: &[StoredEvent]) -> Result<(), DbError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;

        for e in events {
            sqlx::query(
                r#"INSERT OR IGNORE INTO events
                   (contract, event_name, topic0, block_number, block_hash,
                    tx_hash, tx_index, log_index, raw_topics, raw_data, decoded, source)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(&e.contract)
            .bind(&e.event_name)
            .bind(&e.topic0)
            .bind(e.block_number)
            .bind(&e.block_hash)
            .bind(&e.tx_hash)
            .bind(e.tx_index)
            .bind(e.log_index)
            .bind(&e.raw_topics)
            .bind(&e.raw_data)
            .bind(&e.decoded)
            .bind(&e.source)
            .execute(&mut *tx)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// Query events from SQLite for the `scopenode query` command.
    ///
    /// Optional filters for `contract`, `event` name, and block range.
    /// `from_block`/`to_block` default to 0/u64::MAX when `None`.
    /// The SQL is fully parameterized — no injection possible.
    pub async fn query_events(
        &self,
        contract: Option<&str>,
        event: Option<&str>,
        from_block: Option<u64>,
        to_block: Option<u64>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, DbError> {
        let limit_i = limit as i64;
        let from_i = from_block.unwrap_or(0) as i64;
        let to_i = to_block.map(|b| b as i64).unwrap_or(i64::MAX);

        let (sql, bindings) = build_query_events_sql(contract, event, limit_i);
        let mut q = sqlx::query_as::<_, StoredEvent>(&sql);
        if let Some(c) = bindings.contract {
            q = q.bind(c.to_string());
        }
        if let Some(e) = bindings.event {
            q = q.bind(e.to_string());
        }
        q = q.bind(from_i).bind(to_i).bind(limit_i);
        q.fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))
    }

    /// Check if a contract address has been indexed.
    ///
    /// Returns `true` if the contract exists in the `contracts` table (meaning it
    /// has been successfully synced at least once). Used by the JSON-RPC server to
    /// decide whether to answer or reject an `eth_getLogs` request.
    pub async fn is_contract_indexed(&self, address: &str) -> Result<bool, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }
        let row = sqlx::query_as::<_, Row>(
            r#"SELECT COUNT(*) as count FROM contracts WHERE address = ?"#,
        )
        .bind(address)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count > 0)
    }

    /// Count events for a specific contract + event name combination.
    ///
    /// Only counts non-reorged events (`reorged = 0`). Used internally.
    pub async fn count_events(
        &self,
        contract: &str,
        event_name: &str,
    ) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }
        let row = sqlx::query_as::<_, Row>(
            r#"SELECT COUNT(*) as count FROM events WHERE contract = ? AND event_name = ? AND reorged = 0"#,
        )
        .bind(contract)
        .bind(event_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }

    /// Count total non-reorged events for a contract (across all event types).
    ///
    /// Used in `scopenode status` output. `reorged = 0` excludes any events that
    /// were part of a chain reorganization (Phase 3a).
    pub async fn count_events_for_contract(&self, contract: &str) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }
        let row = sqlx::query_as::<_, Row>(
            r#"SELECT COUNT(*) as count FROM events WHERE contract = ? AND reorged = 0"#,
        )
        .bind(contract)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }

    // ─── Sync cursor ──────────────────────────────────────────────────────────

    /// Read the sync cursor for a contract.
    ///
    /// Returns `None` if no cursor exists yet (first run). The cursor is used by
    /// the pipeline to determine where each stage should resume from.
    pub async fn get_sync_cursor(
        &self,
        contract: &str,
    ) -> Result<Option<crate::types::SyncCursor>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            contract: String,
            from_block: i64,
            to_block: Option<i64>,
            headers_done_to: Option<i64>,
            bloom_done_to: Option<i64>,
            receipts_done_to: Option<i64>,
        }

        let row = sqlx::query_as::<_, Row>(
            r#"SELECT contract, from_block, to_block, headers_done_to, bloom_done_to, receipts_done_to
               FROM sync_cursor WHERE contract = ?"#,
        )
        .bind(contract)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(row.map(|r| crate::types::SyncCursor {
            contract: r.contract,
            from_block: r.from_block as u64,
            to_block: r.to_block.map(|n| n as u64),
            headers_done_to: r.headers_done_to.map(|n| n as u64),
            bloom_done_to: r.bloom_done_to.map(|n| n as u64),
            receipts_done_to: r.receipts_done_to.map(|n| n as u64),
        }))
    }

    /// Insert or update the sync cursor for a contract.
    ///
    /// On conflict, uses `CASE WHEN ... IS NOT NULL` to only advance each stage's
    /// progress — we never go backwards. Passing `None` for a stage field means
    /// "don't update that stage's cursor".
    pub async fn upsert_sync_cursor(
        &self,
        cursor: &crate::types::SyncCursor,
    ) -> Result<(), DbError> {
        let from = cursor.from_block as i64;
        let to = cursor.to_block.map(|n| n as i64);
        let headers = cursor.headers_done_to.map(|n| n as i64);
        let bloom = cursor.bloom_done_to.map(|n| n as i64);
        let receipts = cursor.receipts_done_to.map(|n| n as i64);
        sqlx::query(
            r#"INSERT INTO sync_cursor (contract, from_block, to_block, headers_done_to, bloom_done_to, receipts_done_to)
               VALUES (?, ?, ?, ?, ?, ?)
               ON CONFLICT(contract) DO UPDATE SET
                 from_block = excluded.from_block,
                 to_block = excluded.to_block,
                 headers_done_to = CASE WHEN excluded.headers_done_to IS NOT NULL THEN excluded.headers_done_to ELSE headers_done_to END,
                 bloom_done_to = CASE WHEN excluded.bloom_done_to IS NOT NULL THEN excluded.bloom_done_to ELSE bloom_done_to END,
                 receipts_done_to = CASE WHEN excluded.receipts_done_to IS NOT NULL THEN excluded.receipts_done_to ELSE receipts_done_to END"#,
        )
        .bind(&cursor.contract)
        .bind(from)
        .bind(to)
        .bind(headers)
        .bind(bloom)
        .bind(receipts)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    // ─── Contracts ────────────────────────────────────────────────────────────

    /// Insert or update a contract record.
    ///
    /// On conflict, updates the ABI but preserves any existing `name` if the new
    /// `name` is `NULL` (using `COALESCE`). This means a config change that removes
    /// the contract name won't erase a previously set name.
    pub async fn upsert_contract(
        &self,
        address: &str,
        name: Option<&str>,
        abi_json: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT INTO contracts (address, name, abi_json)
               VALUES (?, ?, ?)
               ON CONFLICT(address) DO UPDATE SET
                 name = COALESCE(excluded.name, contracts.name),
                 abi_json = excluded.abi_json"#,
        )
        .bind(address)
        .bind(name)
        .bind(abi_json)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// Retrieve the cached ABI JSON for a contract.
    ///
    /// Returns `None` if the contract is not in the `contracts` table yet (first run).
    /// The ABI is stored as a JSON array of event definitions after the first Sourcify fetch.
    pub async fn get_contract_abi(&self, address: &str) -> Result<Option<String>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            abi_json: Option<String>,
        }
        let row = sqlx::query_as::<_, Row>(
            r#"SELECT abi_json FROM contracts WHERE address = ?"#,
        )
        .bind(address)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.and_then(|r| r.abi_json))
    }

    // ─── Status ────────────────────────────────────────────────────────────────

    /// Return all indexed contracts ordered by address.
    ///
    /// Used by `scopenode status` to display a table of indexed contracts.
    pub async fn get_all_contracts(&self) -> Result<Vec<ContractRow>, DbError> {
        sqlx::query_as::<_, ContractRow>(
            r#"SELECT address, name, abi_json FROM contracts ORDER BY address ASC"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))
    }

    /// Return all indexed contracts with their non-reorged event count in one query.
    ///
    /// Equivalent to `get_all_contracts()` + `count_events_for_contract()` per row,
    /// but resolved with a single LEFT JOIN rather than N+1 queries.
    pub async fn contracts_with_event_counts(&self) -> Result<Vec<(ContractRow, i64)>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            address: String,
            name: Option<String>,
            abi_json: Option<String>,
            event_count: i64,
        }
        let rows = sqlx::query_as::<_, Row>(
            r#"SELECT c.address, c.name, c.abi_json,
                      COUNT(e.rowid) as event_count
               FROM contracts c
               LEFT JOIN events e ON e.contract = c.address AND e.reorged = 0
               GROUP BY c.address
               ORDER BY c.address ASC"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    ContractRow { address: r.address, name: r.name, abi_json: r.abi_json },
                    r.event_count,
                )
            })
            .collect())
    }

    /// Count the number of indexed contracts.
    ///
    /// Lighter than `get_all_contracts()` when only the count is needed
    /// (avoids fetching `abi_json` blobs).
    pub async fn count_contracts(&self) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row { count: i64 }
        let row = sqlx::query_as::<_, Row>("SELECT COUNT(*) as count FROM contracts")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }

    /// Count bloom candidates marked for retry (`pending_retry = 1`).
    ///
    /// Shown in `scopenode status` output. A non-zero count indicates blocks where
    /// receipt fetch or Merkle verification failed — run `scopenode retry` to
    /// re-attempt them (Phase 3a).
    pub async fn count_pending_retry(&self) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }
        let row = sqlx::query_as::<_, Row>(
            r#"SELECT COUNT(*) as count FROM bloom_candidates WHERE pending_retry = 1"#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }

    /// Count all bloom candidates for a contract (fetched + pending + unfetched).
    ///
    /// Used by `--dry-run` to report the total known candidate count when
    /// resuming a sync where the bloom scan was already completed.
    pub async fn count_bloom_candidates(&self, contract: &str) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }
        let row = sqlx::query_as::<_, Row>(
            r#"SELECT COUNT(*) as count FROM bloom_candidates WHERE contract = ?"#,
        )
        .bind(contract)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }

    /// Return the on-disk size of the database in bytes.
    ///
    /// Uses SQLite `PRAGMA page_count` and `PRAGMA page_size` to compute the
    /// total file size without reading the file from disk. Shown in `scopenode status`.
    pub async fn db_size_bytes(&self) -> Result<u64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            size: i64,
        }
        let row = sqlx::query_as::<_, Row>(
            r#"SELECT page_count * page_size as size FROM pragma_page_count(), pragma_page_size()"#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.size as u64)
    }

    /// Mark all events from the given block hashes as reorged (`reorged = 1`).
    ///
    /// Called by the live-sync pipeline when [`ReorgDetector`] returns a
    /// [`ReorgEvent`]. Events are soft-deleted — never hard-deleted — so they
    /// can be inspected post-hoc and are excluded from all normal queries via
    /// the `WHERE reorged = 0` filter.
    ///
    /// Returns the total number of event rows that were updated.
    ///
    /// [`ReorgDetector`]: scopenode_core::reorg::ReorgDetector
    /// [`ReorgEvent`]: scopenode_core::reorg::ReorgEvent
    pub async fn mark_reorged_by_hash(&self, block_hashes: &[B256]) -> Result<u64, DbError> {
        if block_hashes.is_empty() {
            return Ok(0);
        }
        let placeholders = block_hashes.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "UPDATE events SET reorged = 1 WHERE block_hash IN ({placeholders}) AND reorged = 0"
        );
        let mut q = sqlx::query(&sql);
        for hash in block_hashes {
            q = q.bind(hash.to_string());
        }
        let result = q
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(result.rows_affected())
    }

    /// Return all `(block_number, block_hash, receipts_root, contract)` tuples that
    /// have `pending_retry = 1` in `bloom_candidates`, joined with `headers` to get
    /// the `receipts_root` needed for Merkle verification.
    pub async fn get_pending_retry_candidates(
        &self,
    ) -> Result<Vec<(u64, alloy_primitives::B256, alloy_primitives::B256, String)>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            block_number: i64,
            block_hash: String,
            receipts_root: String,
            contract: String,
        }
        let rows = sqlx::query_as::<_, Row>(
            r#"SELECT bc.block_number, bc.block_hash, h.receipts_root, bc.contract
               FROM bloom_candidates bc
               JOIN headers h ON h.number = bc.block_number
               WHERE bc.pending_retry = 1"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        rows.into_iter()
            .map(|r| {
                let block_hash: alloy_primitives::B256 = r
                    .block_hash
                    .parse()
                    .map_err(|_| DbError::Query(format!("invalid block_hash: {}", r.block_hash)))?;
                let receipts_root: alloy_primitives::B256 = r
                    .receipts_root
                    .parse()
                    .map_err(|_| DbError::Query(format!("invalid receipts_root: {}", r.receipts_root)))?;
                Ok((r.block_number as u64, block_hash, receipts_root, r.contract))
            })
            .collect()
    }

    /// Query events with optional filters.
    ///
    /// All filters are combined with AND. Only non-reorged events are returned.
    /// `limit` caps the result set; `offset` supports pagination.
    ///
    /// - `contract` — EIP-55 checksummed address
    /// - `event_name` — human-readable event name (e.g. `"Swap"`)
    /// - `topic0` — raw keccak256 event selector hash (0x-prefixed hex)
    /// - `from_block` / `to_block` — inclusive block range (defaults to full range)
    #[allow(clippy::too_many_arguments)]
    pub async fn query_events_for_filter(
        &self,
        contract: Option<&str>,
        event_name: Option<&str>,
        topic0: Option<&str>,
        from_block: Option<u64>,
        to_block: Option<u64>,
        limit: usize,
        offset: u64,
    ) -> Result<Vec<StoredEvent>, DbError> {
        let from = from_block.unwrap_or(0) as i64;
        let to = to_block.map(|b| b as i64).unwrap_or(i64::MAX);

        // Build WHERE clauses dynamically — only include filters that are Some.
        let mut conditions = vec!["reorged = 0"];
        if contract.is_some()   { conditions.push("contract = ?"); }
        if event_name.is_some() { conditions.push("event_name = ?"); }
        if topic0.is_some()     { conditions.push("topic0 = ?"); }
        conditions.extend(["block_number >= ?", "block_number <= ?"]);

        let sql = format!(
            "SELECT contract, event_name, topic0, block_number, block_hash, \
             tx_hash, tx_index, log_index, raw_topics, raw_data, decoded, source \
             FROM events WHERE {} \
             ORDER BY block_number ASC, log_index ASC LIMIT ? OFFSET ?",
            conditions.join(" AND ")
        );

        let mut q = sqlx::query_as::<_, StoredEvent>(&sql);
        if let Some(c) = contract   { q = q.bind(c); }
        if let Some(e) = event_name { q = q.bind(e); }
        if let Some(t) = topic0     { q = q.bind(t); }
        q = q.bind(from).bind(to).bind(limit as i64).bind(offset as i64);

        q.fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))
    }

    /// Count all non-reorged events across all contracts.
    ///
    /// Used by the REST `/status` endpoint.
    pub async fn count_all_events(&self) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row { count: i64 }
        let row = sqlx::query_as::<_, Row>(
            "SELECT COUNT(*) as count FROM events WHERE reorged = 0",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }
}

/// Parse a lowercase hex string from SQLite back into an alloy `Bloom` type.
///
/// Returns the zero bloom (all bits unset) on invalid input — this means any
/// bloom check on a corrupt header will return false (no false matches), which
/// is the safe fallback. Logs a warning so corrupt rows are visible.
fn bloom_from_hex(hex_str: &str) -> Bloom {
    match alloy_primitives::hex::decode(hex_str) {
        Ok(bytes) if bytes.len() == 256 => {
            let mut arr = [0u8; 256];
            arr.copy_from_slice(&bytes);
            Bloom::new(arr)
        }
        Ok(bytes) => {
            tracing::warn!(
                len = bytes.len(),
                "logs_bloom hex has wrong byte length (expected 256), using zero bloom"
            );
            Bloom::default()
        }
        Err(e) => {
            tracing::warn!(error = %e, "corrupt logs_bloom hex in DB, using zero bloom");
            Bloom::default()
        }
    }
}

/// Intermediate row type for [`Db::get_headers`] with `logs_bloom` already parsed.
///
/// Avoids re-parsing the 256-byte BLOB on every bloom check in the scanner.
/// The bloom is parsed once when loading from SQLite and stored as an `alloy_primitives::Bloom`.
#[derive(Debug, Clone)]
pub struct HeaderRow {
    /// Block number.
    pub number: i64,
    /// Block hash as a `0x`-prefixed hex string.
    pub hash: String,
    /// Parent block hash.
    pub parent_hash: String,
    /// Unix timestamp.
    pub timestamp: i64,
    /// `receipts_root` as a `0x`-prefixed hex string.
    pub receipts_root: String,
    /// Bloom filter, pre-parsed from the raw BLOB for use in the scanner.
    pub logs_bloom: Bloom,
    /// Total gas used.
    pub gas_used: i64,
    /// EIP-1559 base fee (NULL for pre-London blocks).
    pub base_fee: Option<i64>,
}

// ─── Dynamic query builder helpers ─────────────────────────────────────────

/// Holds the optional binding values for a dynamic `query_events` SQL query.
struct QueryBindings<'a> {
    contract: Option<&'a str>,
    event: Option<&'a str>,
}

/// Build the SQL string and binding values for [`Db::query_events`].
///
/// Returns a `(sql, bindings)` pair. The bindings must be applied in the order:
/// contract (if Some) → event (if Some) → from_block → to_block → limit.
fn build_query_events_sql<'a>(
    contract: Option<&'a str>,
    event: Option<&'a str>,
    _limit: i64,
) -> (String, QueryBindings<'a>) {
    let base = r#"SELECT contract, event_name, topic0, block_number, block_hash,
                         tx_hash, tx_index, log_index, raw_topics, raw_data, decoded, source
                  FROM events
                  WHERE reorged = 0"#;
    // block_number range is always bound (caller passes 0 / i64::MAX as defaults).
    let tail = "AND block_number >= ? AND block_number <= ? ORDER BY block_number ASC, log_index ASC LIMIT ?";

    let (sql, bind_contract, bind_event) = match (contract, event) {
        (Some(_), Some(_)) => (
            format!("{base} AND contract = ? AND event_name = ? {tail}"),
            true,
            true,
        ),
        (Some(_), None) => (
            format!("{base} AND contract = ? {tail}"),
            true,
            false,
        ),
        (None, Some(_)) => (
            format!("{base} AND event_name = ? {tail}"),
            false,
            true,
        ),
        (None, None) => (
            format!("{base} {tail}"),
            false,
            false,
        ),
    };

    (
        sql,
        QueryBindings {
            contract: if bind_contract { contract } else { None },
            event: if bind_event { event } else { None },
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a temporary on-disk SQLite database for testing.
    ///
    /// Uses a temp file so migrations (which require WAL mode) work correctly.
    /// The file is cleaned up when `_guard` is dropped.
    async fn open_test_db() -> (Db, tempfile::TempPath) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = tmp.into_parts();
        // Drop the file handle so Db::open can open the same path without a lock conflict
        // on Windows; on Unix this is harmless. The TempPath returned to the caller keeps
        // the file alive and deletes it on drop.
        drop(file);
        let db = Db::open(path.to_path_buf()).await.unwrap();
        (db, path)
    }

    fn make_event(contract: &str, block_number: i64, block_hash: &str, tx_hash: &str, log_index: i64) -> StoredEvent {
        StoredEvent {
            contract: contract.to_string(),
            event_name: "Transfer".to_string(),
            topic0: "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".to_string(),
            block_number,
            block_hash: block_hash.to_string(),
            tx_hash: tx_hash.to_string(),
            tx_index: 0,
            log_index,
            raw_topics: "[\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\"]".to_string(),
            raw_data: "00".to_string(),
            decoded: "{}".to_string(),
            source: "devp2p".to_string(),
        }
    }

    #[tokio::test]
    async fn get_logs_excludes_reorged_events() {
        let (db, _guard) = open_test_db().await;

        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let canonical_hash = "0x1111111111111111111111111111111111111111111111111111111111111111";
        let orphan_hash   = "0x2222222222222222222222222222222222222222222222222222222222222222";

        // Insert one canonical event and one orphaned (reorged) event.
        let canonical = make_event(contract, 100, canonical_hash, "0xaaaa", 0);
        let orphaned  = make_event(contract, 101, orphan_hash,   "0xbbbb", 0);

        db.insert_events(&[canonical, orphaned]).await.unwrap();

        let hash: B256 = orphan_hash.parse().unwrap();
        let marked = db.mark_reorged_by_hash(&[hash]).await.unwrap();
        assert_eq!(marked, 1, "one event should be marked reorged");

        let rows = db
            .query_events_for_filter(Some(contract), None, None, None, None, 100, 0)
            .await
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].block_hash, canonical_hash);
    }
}
