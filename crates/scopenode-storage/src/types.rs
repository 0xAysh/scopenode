//! Higher-level types used by the storage layer.

/// Parameters for filtering event queries.
///
/// Optional fields restrict results to matching rows. `limit` caps the result
/// set; storage queries `limit + 1` rows internally and returns
/// [`DbError::TooManyResults`] when exceeded. `offset` supports pagination.
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
    /// Max rows to return. Storage queries `limit + 1`; returns `TooManyResults`
    /// if the result count exceeds this value. `0` means no cap.
    pub limit: u64,
    pub offset: u64,
}
