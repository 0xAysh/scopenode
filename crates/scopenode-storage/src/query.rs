//! Event query read model — owns indexed event read policy.
//!
//! Use [`EventQuery`] + [`Db::query_events`] instead of assembling
//! [`crate::types::EventFilter`] and interpreting [`crate::types::QueryResult`]
//! directly. Outcome variants make not-indexed and empty states explicit so
//! callers never infer them from row counts.

use crate::db::Db;
use crate::error::DbError;
use crate::models::StoredEvent;
use crate::types::EventFilter;
use crate::MissingCoverage;

/// Domain-level request for an indexed event query.
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    pub contract: Option<String>,
    pub event_name: Option<String>,
    pub topic0: Option<String>,
    pub from_block: Option<u64>,
    pub to_block: Option<u64>,
    /// Max rows to return. `0` = no cap.
    pub limit: u64,
    pub offset: u64,
}

/// Explicit outcomes for an indexed event query.
#[derive(Debug)]
pub enum EventQueryOutcome {
    /// The contract is indexed, but scopenode has not covered the requested range.
    MissingCoverage { missing: MissingCoverage },
    /// One or more matching events within the result cap.
    Results(Vec<StoredEvent>),
    /// Result set hit the cap; first `cap` rows returned.
    Capped { results: Vec<StoredEvent>, cap: u64 },
    /// The specified contract address has not been indexed.
    NotIndexed,
    /// The contract is indexed but no events match the query.
    Empty,
}

/// All facts needed to render `scopenode status`.
#[derive(Debug)]
pub struct StatusSummary {
    pub latest_block: Option<u64>,
    pub event_count: i64,
    pub contract_count: i64,
    pub db_size_bytes: u64,
    pub block_range: Option<(u64, u64)>,
    pub contracts: Vec<ContractSummary>,
}

/// Per-contract summary returned by [`StatusSummary`].
#[derive(Debug)]
pub struct ContractSummary {
    pub address: String,
    pub name: Option<String>,
    pub total_events: i64,
    pub event_breakdown: Vec<(String, i64)>,
}

impl Db {
    /// Query indexed events and return an explicit outcome.
    ///
    /// If `q.contract` is set and the address is not in the contracts table,
    /// returns [`EventQueryOutcome::NotIndexed`] without running the event
    /// query. An indexed contract with no matching rows returns
    /// [`EventQueryOutcome::Empty`] rather than an empty
    /// [`EventQueryOutcome::Results`].
    pub async fn query_events(&self, q: &EventQuery) -> Result<EventQueryOutcome, DbError> {
        if let Some(contract) = &q.contract {
            if !self.is_contract_indexed(contract).await? {
                return Ok(EventQueryOutcome::NotIndexed);
            }
            if let (Some(from_block), Some(to_block)) = (q.from_block, q.to_block) {
                if !self.is_range_covered(contract, from_block, to_block).await? {
                    return Ok(EventQueryOutcome::MissingCoverage { missing: MissingCoverage {
                        contract: contract.clone(),
                        from_block,
                        to_block,
                    }});
                }
            }
        }

        let filter = EventFilter {
            contract: q.contract.clone(),
            event_name: q.event_name.clone(),
            topic0: q.topic0.clone(),
            from_block: q.from_block,
            to_block: q.to_block,
            limit: q.limit,
            offset: q.offset,
        };

        match self.query_events_for_filter(&filter).await? {
            crate::types::QueryResult::Capped { results, cap } => {
                Ok(EventQueryOutcome::Capped { results, cap })
            }
            crate::types::QueryResult::Results(rows) if rows.is_empty() => {
                Ok(EventQueryOutcome::Empty)
            }
            crate::types::QueryResult::Results(rows) => Ok(EventQueryOutcome::Results(rows)),
        }
    }

    /// Fetch all facts needed for `scopenode status` in one call.
    pub async fn status_summary(&self) -> Result<StatusSummary, DbError> {
        let (latest_block, event_count, contract_count, db_size_bytes, block_range, contracts_raw) =
            tokio::try_join!(
                self.latest_block_number(),
                self.count_all_events(),
                self.count_contracts(),
                self.db_size_bytes(),
                self.indexed_block_range(),
                self.contracts_with_event_counts(),
            )?;

        let mut contracts = Vec::with_capacity(contracts_raw.len());
        for (row, total_events) in contracts_raw {
            let event_breakdown = self.event_counts_for_contract(&row.address).await?;
            contracts.push(ContractSummary {
                address: row.address,
                name: row.name,
                total_events,
                event_breakdown,
            });
        }

        Ok(StatusSummary {
            latest_block,
            event_count,
            contract_count,
            db_size_bytes,
            block_range,
            contracts,
        })
    }
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

    const CONTRACT: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const TOPIC0: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

    fn make_event(contract: &str, block_number: i64, log_index: i64) -> StoredEvent {
        StoredEvent {
            contract: contract.to_string(),
            event_name: "Transfer".to_string(),
            topic0: TOPIC0.to_string(),
            block_number,
            block_hash: format!("0x{block_number:064x}"),
            tx_hash: format!("0x{:064x}", log_index + 100),
            tx_index: 0,
            log_index,
            raw_topics: format!("[\"{TOPIC0}\"]"),
            raw_data: "00".to_string(),
            decoded: "{}".to_string(),
            source: "era1".to_string(),
            timestamp: 0,
        }
    }

    // ── outcome taxonomy ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn not_indexed_when_contract_not_in_db() {
        let (db, _guard) = open_test_db().await;
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(outcome, EventQueryOutcome::NotIndexed));
    }

    #[tokio::test]
    async fn empty_when_indexed_but_no_matching_events() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(outcome, EventQueryOutcome::Empty));
    }

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

    #[tokio::test]
    async fn not_indexed_and_empty_are_distinct() {
        let (db, _guard) = open_test_db().await;

        // Not indexed
        let ni = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(ni, EventQueryOutcome::NotIndexed));

        // Indexed but empty
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        let empty = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(empty, EventQueryOutcome::Empty));
    }

    #[tokio::test]
    async fn results_when_events_exist() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        db.insert_events(&[make_event(CONTRACT, 100, 0)])
            .await
            .unwrap();
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(outcome, EventQueryOutcome::Results(rows) if rows.len() == 1));
    }

    #[tokio::test]
    async fn capped_when_result_exceeds_limit() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        let events: Vec<_> = (0..5).map(|i| make_event(CONTRACT, 100 + i, i)).collect();
        db.insert_events(&events).await.unwrap();
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 3,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            EventQueryOutcome::Capped { cap: 3, .. }
        ));
    }

    // ── 10,000 / 10,001 cap boundary ─────────────────────────────────────────

    #[tokio::test]
    async fn exactly_ten_thousand_rows_succeeds() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        let events: Vec<_> = (0i64..10_000)
            .map(|i| make_event(CONTRACT, 1_000 + i, i))
            .collect();
        db.insert_events(&events).await.unwrap();
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 10_000,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(
            matches!(outcome, EventQueryOutcome::Results(rows) if rows.len() == 10_000),
            "expected Results(10000)"
        );
    }

    #[tokio::test]
    async fn ten_thousand_plus_one_triggers_capped() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        let events: Vec<_> = (0i64..10_001)
            .map(|i| make_event(CONTRACT, 1_000 + i, i))
            .collect();
        db.insert_events(&events).await.unwrap();
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 10_000,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(
            matches!(outcome, EventQueryOutcome::Capped { cap: 10_000, .. }),
            "expected Capped(10000)"
        );
    }

    // ── filter dimensions ────────────────────────────────────────────────────

    #[tokio::test]
    async fn topic0_filter_matches_correct_events() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        db.insert_events(&[make_event(CONTRACT, 100, 0)])
            .await
            .unwrap();
        let outcome = db
            .query_events(&EventQuery {
                topic0: Some(TOPIC0.to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(outcome, EventQueryOutcome::Results(rows) if rows.len() == 1));
    }

    #[tokio::test]
    async fn event_name_filter_matches_and_misses() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        db.insert_events(&[make_event(CONTRACT, 100, 0)])
            .await
            .unwrap();

        let hit = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                event_name: Some("Transfer".to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(hit, EventQueryOutcome::Results(rows) if rows.len() == 1));

        let miss = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                event_name: Some("Swap".to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(miss, EventQueryOutcome::Empty));
    }

    #[tokio::test]
    async fn block_range_filter() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        db.insert_events(&[
            make_event(CONTRACT, 100, 0),
            make_event(CONTRACT, 200, 1),
            make_event(CONTRACT, 300, 2),
        ])
        .await
        .unwrap();
        db.record_covered_range(CONTRACT, 150, 250).await.unwrap();
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                from_block: Some(150),
                to_block: Some(250),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(outcome, EventQueryOutcome::Results(rows) if rows.len() == 1));
    }

    #[tokio::test]
    async fn limit_and_offset_pagination() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        let events: Vec<_> = (0..5).map(|i| make_event(CONTRACT, 100 + i, i)).collect();
        db.insert_events(&events).await.unwrap();

        // offset=3, limit=5 → rows at index 3 and 4 → 2 results (not capped)
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 5,
                offset: 3,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(outcome, EventQueryOutcome::Results(rows) if rows.len() == 2));
    }

    #[tokio::test]
    async fn no_contract_filter_skips_indexed_check() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        db.insert_events(&[make_event(CONTRACT, 100, 0)])
            .await
            .unwrap();
        // No contract filter → no NotIndexed check possible
        let outcome = db
            .query_events(&EventQuery {
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert!(matches!(outcome, EventQueryOutcome::Results(_)));
    }

    // ── result ordering ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn results_ordered_by_block_then_log_index() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        db.insert_events(&[
            make_event(CONTRACT, 300, 2),
            make_event(CONTRACT, 100, 0),
            make_event(CONTRACT, 200, 1),
        ])
        .await
        .unwrap();
        let outcome = db
            .query_events(&EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        if let EventQueryOutcome::Results(rows) = outcome {
            assert_eq!(rows[0].block_number, 100);
            assert_eq!(rows[1].block_number, 200);
            assert_eq!(rows[2].block_number, 300);
        } else {
            panic!("expected Results");
        }
    }

    // ── status summary ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn status_summary_empty_db() {
        let (db, _guard) = open_test_db().await;
        let s = db.status_summary().await.unwrap();
        assert_eq!(s.event_count, 0);
        assert_eq!(s.contract_count, 0);
        assert!(s.latest_block.is_none());
        assert!(s.block_range.is_none());
        assert!(s.contracts.is_empty());
    }

    #[tokio::test]
    async fn status_summary_populated_db() {
        let (db, _guard) = open_test_db().await;
        db.upsert_contract(CONTRACT, Some("USDC"), "[]")
            .await
            .unwrap();
        db.insert_events(&[make_event(CONTRACT, 100, 0)])
            .await
            .unwrap();
        let s = db.status_summary().await.unwrap();
        assert_eq!(s.event_count, 1);
        assert_eq!(s.contract_count, 1);
        assert_eq!(s.latest_block, Some(100));
        assert_eq!(s.block_range, Some((100, 100)));
        assert_eq!(s.contracts.len(), 1);
        assert_eq!(s.contracts[0].address, CONTRACT);
        assert_eq!(s.contracts[0].total_events, 1);
        assert_eq!(s.contracts[0].event_breakdown, vec![("Transfer".to_string(), 1)]);
    }
}
