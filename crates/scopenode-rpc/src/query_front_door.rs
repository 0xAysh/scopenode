use crate::filter_plan::FilterPlan;
use scopenode_storage::error::DbError;
use scopenode_storage::models::StoredEvent;
use scopenode_storage::{Db, EventQueryOutcome};

pub const RESULT_CAP_MESSAGE: &str = "result set exceeds 10,000 rows — narrow your filter (smaller block range or add address/topic filter)";
pub const MISSING_COVERAGE_MESSAGE: &str = "requested range is outside local coverage — run `scopenode sync` for this contract and block range";

pub enum EventQueryResponse {
    NotIndexed,
    MissingCoverage,
    TooManyResults { cap: u64 },
    Empty,
    Results(Vec<StoredEvent>),
}

#[derive(Debug)]
pub enum EventQueryFrontDoorError {
    MissingAddress,
    Unsupported { reason: String },
    Storage(DbError),
}

pub async fn execute_event_query(
    db: &Db,
    plan: FilterPlan,
) -> Result<EventQueryResponse, EventQueryFrontDoorError> {
    let query = match plan {
        FilterPlan::Query(query) => query,
        FilterPlan::MissingAddress => return Err(EventQueryFrontDoorError::MissingAddress),
        FilterPlan::Unsupported { reason } => {
            return Err(EventQueryFrontDoorError::Unsupported { reason });
        }
    };

    db.query_events(&query)
        .await
        .map(map_event_query_outcome)
        .map_err(EventQueryFrontDoorError::Storage)
}

pub fn map_event_query_outcome(outcome: EventQueryOutcome) -> EventQueryResponse {
    match outcome {
        EventQueryOutcome::NotIndexed => EventQueryResponse::NotIndexed,
        EventQueryOutcome::MissingCoverage { .. } => EventQueryResponse::MissingCoverage,
        EventQueryOutcome::Capped { cap, .. } => EventQueryResponse::TooManyResults { cap },
        EventQueryOutcome::Empty => EventQueryResponse::Empty,
        EventQueryOutcome::Results(rows) => EventQueryResponse::Results(rows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_plan::FilterPlan;
    use scopenode_storage::{Db, EventQuery};

    const CONTRACT: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const TOPIC0: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

    fn make_event(block_number: i64, log_index: i64) -> StoredEvent {
        StoredEvent {
            contract: CONTRACT.to_string(),
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

    #[tokio::test]
    async fn query_front_door_maps_capped_event_query() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = tmp.into_parts();
        drop(file);
        let db = Db::open(path.to_path_buf()).await.unwrap();
        db.upsert_contract(CONTRACT, None, "[]").await.unwrap();
        let events: Vec<_> = (0i64..10_001)
            .map(|i| make_event(1_000 + i, i))
            .collect();
        db.insert_events(&events).await.unwrap();

        let response = execute_event_query(
            &db,
            FilterPlan::Query(EventQuery {
                contract: Some(CONTRACT.to_string()),
                limit: 10_000,
                ..EventQuery::default()
            }),
        )
        .await
        .unwrap();

        assert!(matches!(
            response,
            EventQueryResponse::TooManyResults { cap: 10_000 }
        ));
    }
}
