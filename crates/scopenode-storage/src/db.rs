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
use crate::models::{ContractRow, StoredEvent};
use crate::types::{EventFilter, QueryResult};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};
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
    pub async fn open(path: PathBuf) -> Result<Self, DbError> {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
        use std::str::FromStr;

        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .map_err(|e| DbError::Open(e.to_string()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePool::connect_with(options)
            .await
            .map_err(|e| DbError::Open(e.to_string()))?;

        sqlx::migrate!("src/migrations")
            .run(&pool)
            .await
            .map_err(|e| DbError::Migration(e.to_string()))?;

        info!(path = %path.display(), "Database opened");

        Ok(Self { pool, path })
    }

    // ─── Events ────────────────────────────────────────────────────────────────

    /// Bulk insert decoded events inside a single transaction.
    ///
    /// Uses `INSERT OR IGNORE` on `(block_number, tx_index, log_index)` for
    /// idempotent re-sync.
    pub async fn insert_events(&self, events: &[StoredEvent]) -> Result<(), DbError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;

        // SQLite has a finite bind-parameter limit. Each event uses 12 binds,
        // so chunking keeps bulk inserts comfortably under common limits while
        // still avoiding one statement per row.
        for chunk in events.chunks(500) {
            let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new(
                "INSERT OR IGNORE INTO events \
                 (contract, event_name, topic0, block_number, block_hash, \
                  tx_hash, tx_index, log_index, raw_topics, raw_data, decoded, source, timestamp) ",
            );
            builder.push_values(chunk, |mut row, e| {
                row.push_bind(&e.contract)
                    .push_bind(&e.event_name)
                    .push_bind(&e.topic0)
                    .push_bind(e.block_number)
                    .push_bind(&e.block_hash)
                    .push_bind(&e.tx_hash)
                    .push_bind(e.tx_index)
                    .push_bind(e.log_index)
                    .push_bind(&e.raw_topics)
                    .push_bind(&e.raw_data)
                    .push_bind(&e.decoded)
                    .push_bind(&e.source)
                    .push_bind(e.timestamp);
            });
            builder
                .build()
                .execute(&mut *tx)
                .await
                .map_err(|e| DbError::Query(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// Return the highest block number stored in the events table.
    ///
    /// Returns `None` if no events have been indexed yet.
    pub async fn latest_block_number(&self) -> Result<Option<u64>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            max_num: Option<i64>,
        }
        let row = sqlx::query_as::<_, Row>("SELECT MAX(block_number) as max_num FROM events")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.max_num.map(|n| n as u64))
    }

    /// Check if a contract address has been indexed.
    pub(crate) async fn is_contract_indexed(&self, address: &str) -> Result<bool, DbError> {
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

    /// Count total events for a contract (across all event types).
    pub async fn count_events_for_contract(&self, contract: &str) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }
        let row =
            sqlx::query_as::<_, Row>(r#"SELECT COUNT(*) as count FROM events WHERE contract = ?"#)
                .bind(contract)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }

    /// Return per-event-name counts for a contract.
    pub(crate) async fn event_counts_for_contract(
        &self,
        contract: &str,
    ) -> Result<Vec<(String, i64)>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            event_name: String,
            count: i64,
        }
        let rows = sqlx::query_as::<_, Row>(
            r#"SELECT event_name, COUNT(*) as count
               FROM events
               WHERE contract = ?
               GROUP BY event_name
               ORDER BY event_name ASC"#,
        )
        .bind(contract)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(rows.into_iter().map(|r| (r.event_name, r.count)).collect())
    }

    /// Return the min/max block range for indexed events.
    pub(crate) async fn indexed_block_range(&self) -> Result<Option<(u64, u64)>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            min_num: Option<i64>,
            max_num: Option<i64>,
        }
        let row = sqlx::query_as::<_, Row>(
            "SELECT MIN(block_number) as min_num, MAX(block_number) as max_num FROM events",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(match (row.min_num, row.max_num) {
            (Some(min), Some(max)) => Some((min as u64, max as u64)),
            _ => None,
        })
    }

    // ─── Contracts ────────────────────────────────────────────────────────────

    /// Insert or update a contract record.
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
    pub async fn get_contract_abi(&self, address: &str) -> Result<Option<String>, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            abi_json: Option<String>,
        }
        let row = sqlx::query_as::<_, Row>(r#"SELECT abi_json FROM contracts WHERE address = ?"#)
            .bind(address)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.and_then(|r| r.abi_json))
    }

    /// Return all indexed contracts with their event count in one query.
    pub(crate) async fn contracts_with_event_counts(
        &self,
    ) -> Result<Vec<(ContractRow, i64)>, DbError> {
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
               LEFT JOIN events e ON e.contract = c.address
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
                    ContractRow {
                        address: r.address,
                        name: r.name,
                        abi_json: r.abi_json,
                    },
                    r.event_count,
                )
            })
            .collect())
    }

    /// Count the number of indexed contracts.
    pub(crate) async fn count_contracts(&self) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }
        let row = sqlx::query_as::<_, Row>("SELECT COUNT(*) as count FROM contracts")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }

    // ─── Queries ──────────────────────────────────────────────────────────────

    /// Query events with the given filter.
    ///
    /// When `filter.limit > 0` the storage layer queries `limit + 1` rows
    /// internally. Returns [`QueryResult::Capped`] when the result exceeds
    /// `limit`; [`QueryResult::Results`] otherwise.
    pub(crate) async fn query_events_for_filter(
        &self,
        filter: &EventFilter,
    ) -> Result<QueryResult, DbError> {
        let from = filter.from_block.unwrap_or(0) as i64;
        let to = filter.to_block.map(|b| b as i64).unwrap_or(i64::MAX);
        let has_contract = filter.contract.is_some();
        let has_event_name = filter.event_name.is_some();
        let has_topic0 = filter.topic0.is_some();

        let query_limit = if filter.limit > 0 {
            filter.limit + 1
        } else {
            i64::MAX as u64
        };
        let sql = format!(
            "{} LIMIT ? OFFSET ?",
            filter_sql_base(has_contract, has_event_name, has_topic0)
        );

        let mut q = sqlx::query_as::<_, StoredEvent>(&sql);
        if let Some(c) = &filter.contract {
            q = q.bind(c.as_str());
        }
        if let Some(e) = &filter.event_name {
            q = q.bind(e.as_str());
        }
        if let Some(t) = &filter.topic0 {
            q = q.bind(t.as_str());
        }
        q = q
            .bind(from)
            .bind(to)
            .bind(query_limit as i64)
            .bind(filter.offset as i64);

        let mut rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;

        if filter.limit > 0 && rows.len() as u64 > filter.limit {
            rows.truncate(filter.limit as usize);
            return Ok(QueryResult::Capped {
                results: rows,
                cap: filter.limit,
            });
        }

        Ok(QueryResult::Results(rows))
    }

    /// Count all events across all contracts.
    pub(crate) async fn count_all_events(&self) -> Result<i64, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }
        let row = sqlx::query_as::<_, Row>("SELECT COUNT(*) as count FROM events")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(row.count)
    }

    /// Return the on-disk size of the database in bytes.
    pub(crate) async fn db_size_bytes(&self) -> Result<u64, DbError> {
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
}

/// Build and cache the base filter SQL for the 8 possible (contract, event_name, topic0) flag
/// combinations. Both callers of the paginated query path share this single source of truth —
/// adding a new filter dimension means editing this function only.
///
/// Results are cached in a `OnceLock` array (one slot per 3-bit flag combination) so each unique
/// SQL string is built once and lives for `'static`.
fn filter_sql_base(has_contract: bool, has_event_name: bool, has_topic0: bool) -> &'static str {
    static CACHE: [std::sync::OnceLock<String>; 8] = [const { std::sync::OnceLock::new() }; 8];

    let idx =
        (has_contract as usize) | ((has_event_name as usize) << 1) | ((has_topic0 as usize) << 2);

    CACHE[idx]
        .get_or_init(|| {
            let mut conditions: Vec<&str> = Vec::new();
            if has_contract {
                conditions.push("contract = ?");
            }
            if has_event_name {
                conditions.push("event_name = ?");
            }
            if has_topic0 {
                conditions.push("topic0 = ?");
            }
            conditions.extend(["block_number >= ?", "block_number <= ?"]);
            format!(
                "SELECT contract, event_name, topic0, block_number, block_hash, \
                 tx_hash, tx_index, log_index, raw_topics, raw_data, decoded, source, timestamp \
                 FROM events WHERE {} ORDER BY block_number ASC, log_index ASC",
                conditions.join(" AND "),
            )
        })
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_test_db() -> (Db, tempfile::TempPath) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = tmp.into_parts();
        drop(file);
        let db = Db::open(path.to_path_buf()).await.unwrap();
        (db, path)
    }

    fn make_event(
        contract: &str,
        block_number: i64,
        block_hash: &str,
        tx_hash: &str,
        log_index: i64,
    ) -> StoredEvent {
        StoredEvent {
            contract: contract.to_string(),
            event_name: "Transfer".to_string(),
            topic0: "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                .to_string(),
            block_number,
            block_hash: block_hash.to_string(),
            tx_hash: tx_hash.to_string(),
            tx_index: 0,
            log_index,
            raw_topics: "[\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\"]"
                .to_string(),
            raw_data: "00".to_string(),
            decoded: "{}".to_string(),
            source: "era1".to_string(),
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn insert_and_count_events() {
        let (db, _guard) = open_test_db().await;
        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        db.upsert_contract(contract, Some("USDC"), "[]")
            .await
            .unwrap();
        let e = make_event(contract, 100, "0x1111", "0xaaaa", 0);
        db.insert_events(&[e]).await.unwrap();
        let count = db.count_events_for_contract(contract).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn idempotent_insert_deduplicates() {
        let (db, _guard) = open_test_db().await;
        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        db.upsert_contract(contract, None, "[]").await.unwrap();
        let e = make_event(contract, 100, "0x1111", "0xaaaa", 0);
        db.insert_events(std::slice::from_ref(&e)).await.unwrap();
        db.insert_events(&[e]).await.unwrap();
        let count = db.count_events_for_contract(contract).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn latest_block_number_returns_max_from_events() {
        let (db, _guard) = open_test_db().await;
        assert_eq!(db.latest_block_number().await.unwrap(), None);

        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        db.upsert_contract(contract, None, "[]").await.unwrap();
        let e100 = make_event(contract, 100, "0x1111", "0xaaaa", 0);
        let e200 = make_event(contract, 200, "0x2222", "0xbbbb", 0);
        let e300 = make_event(contract, 300, "0x3333", "0xcccc", 0);
        db.insert_events(&[e100, e200, e300]).await.unwrap();

        assert_eq!(db.latest_block_number().await.unwrap(), Some(300));
    }

    #[tokio::test]
    async fn query_events_for_filter_runtime_correctness() {
        let (db, _guard) = open_test_db().await;
        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        db.upsert_contract(contract, None, "[]").await.unwrap();
        db.insert_events(&[make_event(contract, 100, "0x1111", "0xaaaa", 0)])
            .await
            .unwrap();

        let result = db
            .query_events_for_filter(&EventFilter {
                contract: Some(contract.to_string()),
                limit: 100,
                ..EventFilter::default()
            })
            .await
            .unwrap();
        assert!(matches!(result, QueryResult::Results(rows) if rows.len() == 1));
    }

    #[tokio::test]
    async fn topic0_filter_returns_matching_events() {
        let (db, _guard) = open_test_db().await;
        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let transfer_topic0 = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        db.upsert_contract(contract, None, "[]").await.unwrap();
        db.insert_events(&[make_event(contract, 100, "0x1111", "0xaaaa", 0)])
            .await
            .unwrap();

        let result = db
            .query_events_for_filter(&EventFilter {
                topic0: Some(transfer_topic0.to_string()),
                limit: 100,
                ..EventFilter::default()
            })
            .await
            .unwrap();
        let QueryResult::Results(rows) = result else {
            panic!("expected Results")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].topic0, transfer_topic0);
    }

    #[tokio::test]
    async fn query_events_for_filter_returns_capped_when_cap_exceeded() {
        let (db, _guard) = open_test_db().await;
        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        db.upsert_contract(contract, None, "[]").await.unwrap();
        let events: Vec<_> = (0..5)
            .map(|i| {
                make_event(
                    contract,
                    100 + i,
                    &format!("0x{:04x}", i),
                    &format!("0x{:04x}", i),
                    0,
                )
            })
            .collect();
        db.insert_events(&events).await.unwrap();

        let result = db
            .query_events_for_filter(&EventFilter {
                contract: Some(contract.to_string()),
                limit: 3,
                ..EventFilter::default()
            })
            .await
            .unwrap();

        assert!(
            matches!(&result, QueryResult::Capped { results, cap: 3 } if results.len() == 3),
            "expected Capped with 3 rows, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn query_events_for_filter_returns_results_when_under_cap() {
        let (db, _guard) = open_test_db().await;
        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        db.upsert_contract(contract, None, "[]").await.unwrap();
        let events: Vec<_> = (0..3)
            .map(|i| {
                make_event(
                    contract,
                    100 + i,
                    &format!("0x{:04x}", i),
                    &format!("0x{:04x}", i),
                    0,
                )
            })
            .collect();
        db.insert_events(&events).await.unwrap();

        let result = db
            .query_events_for_filter(&EventFilter {
                contract: Some(contract.to_string()),
                limit: 10,
                ..EventFilter::default()
            })
            .await
            .unwrap();

        assert!(
            matches!(&result, QueryResult::Results(rows) if rows.len() == 3),
            "expected Results with 3 rows, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn query_events_for_filter_returns_results_when_empty() {
        let (db, _guard) = open_test_db().await;
        let contract = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        db.upsert_contract(contract, None, "[]").await.unwrap();

        let result = db
            .query_events_for_filter(&EventFilter {
                contract: Some(contract.to_string()),
                limit: 10,
                ..EventFilter::default()
            })
            .await
            .unwrap();

        assert!(
            matches!(&result, QueryResult::Results(rows) if rows.is_empty()),
            "expected empty Results, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn migration_005_drops_dead_tables() {
        let (db, _guard) = open_test_db().await;

        let err = sqlx::query("SELECT 1 FROM bloom_candidates LIMIT 1")
            .fetch_optional(&db.pool)
            .await;
        assert!(err.is_err(), "bloom_candidates table should not exist");

        let err = sqlx::query("SELECT 1 FROM sync_cursor LIMIT 1")
            .fetch_optional(&db.pool)
            .await;
        assert!(err.is_err(), "sync_cursor table should not exist");

        let err = sqlx::query("SELECT 1 FROM headers LIMIT 1")
            .fetch_optional(&db.pool)
            .await;
        assert!(err.is_err(), "headers table should not exist");
    }

    #[tokio::test]
    async fn migration_005_drops_reorged_column() {
        let (db, _guard) = open_test_db().await;
        let err = sqlx::query("SELECT reorged FROM events LIMIT 1")
            .fetch_optional(&db.pool)
            .await;
        assert!(
            err.is_err(),
            "reorged column should not exist after migration 005"
        );
    }

    #[tokio::test]
    async fn index_shape_covers_contract_topic0_block_log() {
        let (db, _guard) = open_test_db().await;
        #[derive(sqlx::FromRow)]
        struct Row {
            sql: Option<String>,
        }
        let row = sqlx::query_as::<_, Row>(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_events_contract_topic_block_log'",
        )
        .fetch_optional(&db.pool)
        .await
        .unwrap();

        let index_sql = row.and_then(|r| r.sql).unwrap_or_default().to_lowercase();
        assert!(
            index_sql.contains("contract")
                && index_sql.contains("topic0")
                && index_sql.contains("block_number")
                && index_sql.contains("log_index"),
            "idx_events_contract_topic_block_log must cover JSON-RPC lookup shape, got: {index_sql}"
        );
    }

    #[tokio::test]
    async fn index_shape_has_no_reorged_predicate() {
        let (db, _guard) = open_test_db().await;
        #[derive(sqlx::FromRow)]
        struct Row {
            sql: Option<String>,
        }
        let row = sqlx::query_as::<_, Row>(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_events_lookup'",
        )
        .fetch_optional(&db.pool)
        .await
        .unwrap();

        let index_sql = row.and_then(|r| r.sql).unwrap_or_default();
        assert!(
            !index_sql.contains("reorged"),
            "idx_events_lookup must not reference reorged column, got: {index_sql}"
        );
    }
}
