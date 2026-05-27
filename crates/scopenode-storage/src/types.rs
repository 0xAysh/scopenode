//! Internal filter types used by the storage execution layer.

use crate::models::StoredEvent;

/// Parameters for the SQL-level event query executed by [`crate::db::Db`].
///
/// External callers should use [`crate::query::EventQuery`] instead.
/// Adding a new filter dimension requires only a new field here and one
/// clause in `filter_sql_base`.
#[derive(Debug, Clone, Default)]
pub(crate) struct EventFilter {
    pub(crate) contract: Option<String>,
    pub(crate) event_name: Option<String>,
    pub(crate) topic0: Option<String>,
    pub(crate) from_block: Option<u64>,
    pub(crate) to_block: Option<u64>,
    /// Max rows to return. `query_events_for_filter` queries `limit + 1`;
    /// returns [`QueryResult::Capped`] if the result count exceeds this value.
    /// `0` means no cap.
    pub(crate) limit: u64,
    pub(crate) offset: u64,
}

/// Raw result of the SQL-level capped query. External callers receive
/// [`crate::query::EventQueryOutcome`] instead.
#[derive(Debug)]
pub(crate) enum QueryResult {
    /// The result set fits within the requested limit.
    Results(Vec<StoredEvent>),
    /// The result set exceeded the limit; `results` contains the first `cap`
    /// rows and `cap` is the limit that was applied.
    Capped { results: Vec<StoredEvent>, cap: u64 },
}

