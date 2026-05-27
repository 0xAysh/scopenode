//! Higher-level types used by the storage layer.

use crate::models::StoredEvent;

/// Parameters for filtering event queries.
///
/// Optional fields restrict results to matching rows. `limit` caps the result
/// set; `query_events_for_filter` queries `limit + 1` rows internally and
/// returns [`QueryResult::Capped`] when the cap is exceeded. `offset` supports
/// pagination.
///
/// Adding a new filter dimension requires only adding a field here and one
/// clause in `filter_sql_base` — no changes to the API layers.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub contract: Option<String>,
    pub event_name: Option<String>,
    pub topic0: Option<String>,
    pub from_block: Option<u64>,
    pub to_block: Option<u64>,
    /// Max rows to return. `query_events_for_filter` queries `limit + 1`;
    /// returns [`QueryResult::Capped`] if the result count exceeds this value.
    /// `0` means no cap.
    pub limit: u64,
    pub offset: u64,
}

/// Result of a capped event query.
///
/// Pattern-match on the variant to handle the cap condition explicitly — no
/// hidden sentinel values or side-channel errors.
#[derive(Debug)]
pub enum QueryResult {
    /// The result set fits within the requested limit.
    Results(Vec<StoredEvent>),
    /// The result set exceeded the limit; `results` contains the first `cap`
    /// rows and `cap` is the limit that was applied.
    Capped { results: Vec<StoredEvent>, cap: u64 },
}

impl QueryResult {
    /// Extract the rows regardless of whether the result was capped.
    pub fn into_results(self) -> Vec<StoredEvent> {
        match self {
            Self::Results(v) | Self::Capped { results: v, .. } => v,
        }
    }

    /// True when the result was truncated by the cap.
    pub fn is_capped(&self) -> bool {
        matches!(self, Self::Capped { .. })
    }
}
