use scopenode_storage::models::StoredEvent;
use scopenode_storage::EventQueryOutcome;

pub const RESULT_CAP_MESSAGE: &str = "result set exceeds 10,000 rows — narrow your filter (smaller block range or add address/topic filter)";
pub const MISSING_COVERAGE_MESSAGE: &str = "requested range is outside local coverage — run `scopenode sync` for this contract and block range";

pub enum EventQueryResponse {
    NotIndexed,
    MissingCoverage,
    TooManyResults { cap: u64 },
    Empty,
    Results(Vec<StoredEvent>),
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
