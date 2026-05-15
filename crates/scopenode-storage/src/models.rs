//! SQLite row types — direct representations of database rows.

/// A decoded event row as stored in SQLite.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredEvent {
    pub contract: String,
    pub event_name: String,
    pub topic0: String,
    pub block_number: i64,
    pub block_hash: String,
    pub tx_hash: String,
    pub tx_index: i64,
    pub log_index: i64,
    pub raw_topics: String,
    pub raw_data: String,
    pub decoded: String,
    pub source: String,
}

/// Contract registry row — one row per indexed contract.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContractRow {
    pub address: String,
    pub name: Option<String>,
    pub abi_json: Option<String>,
}
