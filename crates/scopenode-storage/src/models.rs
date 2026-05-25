//! SQLite row types — direct representations of database rows.

use alloy_primitives::{Address, Bytes, B256};

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
    pub timestamp: i64,
}

impl StoredEvent {
    /// Construct from decoded log data using Alloy types.
    ///
    /// Handles all serialisation (checksumming, hex encoding, JSON) so callers
    /// work with native Ethereum types and never touch string formats.
    pub fn from_decoded_log(
        contract: Address,
        event_name: &str,
        topic0: B256,
        block_number: u64,
        block_hash: B256,
        tx_hash: B256,
        tx_index: u64,
        log_index: u64,
        raw_topics: &[B256],
        raw_data: &Bytes,
        decoded: serde_json::Value,
        source: &str,
        timestamp: u64,
    ) -> Self {
        Self {
            contract: contract.to_checksum(None),
            event_name: event_name.to_owned(),
            topic0: topic0.to_string(),
            block_number: block_number as i64,
            block_hash: block_hash.to_string(),
            tx_hash: tx_hash.to_string(),
            tx_index: tx_index as i64,
            log_index: log_index as i64,
            raw_topics: serde_json::to_string(
                &raw_topics.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            )
            .unwrap_or_default(),
            raw_data: alloy_primitives::hex::encode(raw_data),
            decoded: decoded.to_string(),
            source: source.to_owned(),
            timestamp: timestamp as i64,
        }
    }
}

/// Contract registry row — one row per indexed contract.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContractRow {
    pub address: String,
    pub name: Option<String>,
    pub abi_json: Option<String>,
}
