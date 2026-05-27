//! Event projection — converts a [`StoredEvent`] row into the shapes required
//! by the JSON-RPC and REST response layers.
//!
//! This module is the single source of truth for:
//! - hex hash parsing
//! - topic JSON parsing
//! - raw-data hex decoding
//! - field mapping onto [`Log`] / [`EventResponse`]
//!
//! Neither `server.rs` nor `rest.rs` are changed by this module; they still own
//! their own call sites. Issue #32 will migrate those call sites to use these
//! functions.

use alloy::primitives::{Address, Bytes, LogData, B256};
use alloy::rpc::types::Log;
use serde::Serialize;
use scopenode_storage::models::StoredEvent;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// REST-layer shape for a single event row.
///
/// Mirrors the private `EventResponse` in `rest.rs` field-for-field so that
/// both representations are always in sync. Issue #32 will unify them.
#[derive(Debug, Serialize)]
pub struct EventResponse {
    pub contract: String,
    pub event_name: String,
    pub topic0: String,
    pub block_number: i64,
    pub block_hash: String,
    pub tx_hash: String,
    pub tx_index: i64,
    pub log_index: i64,
    pub decoded: serde_json::Value,
    pub source: String,
    pub timestamp: i64,
}

// ---------------------------------------------------------------------------
// Public projection functions
// ---------------------------------------------------------------------------

/// Convert a [`StoredEvent`] row into an `alloy` [`Log`].
///
/// Returns `None` when any required field cannot be parsed (malformed address,
/// hash, or topic list). This preserves the same semantics as the private
/// `row_to_log` in `server.rs`.
pub fn project_log(row: &StoredEvent) -> Option<Log> {
    let address: Address = row.contract.parse().ok()?;
    let tx_hash: B256 = row.tx_hash.parse().ok()?;
    let block_hash: B256 = row.block_hash.parse().ok()?;

    let topics: Vec<B256> = serde_json::from_str::<Vec<String>>(&row.raw_topics)
        .ok()?
        .iter()
        .filter_map(|t| t.parse().ok())
        .collect();

    let data_bytes = alloy_primitives::hex::decode(&row.raw_data).unwrap_or_default();
    let data = Bytes::from(data_bytes);

    let log_data = LogData::new(topics, data)?;

    let inner_log = alloy::primitives::Log { address, data: log_data };

    Some(Log {
        inner: inner_log,
        block_hash: Some(block_hash),
        block_number: Some(row.block_number as u64),
        block_timestamp: None,
        transaction_hash: Some(tx_hash),
        transaction_index: Some(row.tx_index as u64),
        log_index: Some(row.log_index as u64),
        removed: false,
    })
}

/// Convert a [`StoredEvent`] row into an [`EventResponse`].
///
/// Always succeeds: when the `decoded` field is not valid JSON the
/// `EventResponse.decoded` field falls back to [`serde_json::Value::Null`].
pub fn project_rest_event(row: &StoredEvent) -> EventResponse {
    EventResponse {
        contract: row.contract.clone(),
        event_name: row.event_name.clone(),
        topic0: row.topic0.clone(),
        block_number: row.block_number,
        block_hash: row.block_hash.clone(),
        tx_hash: row.tx_hash.clone(),
        tx_index: row.tx_index,
        log_index: row.log_index,
        decoded: serde_json::from_str(&row.decoded).unwrap_or(serde_json::Value::Null),
        source: row.source.clone(),
        timestamp: row.timestamp,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_valid_stored_event() -> StoredEvent {
        StoredEvent {
            contract: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".into(),
            event_name: "Transfer".into(),
            topic0: "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".into(),
            block_number: 18_000_000,
            block_hash: format!("0x{:064x}", 100u64),
            tx_hash: format!("0x{:064x}", 1u64),
            tx_index: 3,
            log_index: 7,
            raw_topics:
                "[\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\"]"
                    .into(),
            raw_data: "deadbeef".into(),
            decoded: "{\"from\":\"0xabc\",\"to\":\"0xdef\"}".into(),
            source: "era1".into(),
            timestamp: 1_700_000_000,
        }
    }

    // ------------------------------------------------------------------
    // project_log tests
    // ------------------------------------------------------------------

    #[test]
    fn project_log_valid_row_produces_log() {
        let row = make_valid_stored_event();
        let log = project_log(&row).expect("should produce Some(Log) for a valid row");

        assert_eq!(log.block_number, Some(18_000_000_u64));
        assert_eq!(
            log.transaction_hash,
            Some(format!("0x{:064x}", 1u64).parse().unwrap())
        );
        assert_eq!(
            log.block_hash,
            Some(format!("0x{:064x}", 100u64).parse().unwrap())
        );
        assert_eq!(log.log_index, Some(7_u64));
        assert_eq!(log.transaction_index, Some(3_u64));
        assert!(!log.removed);
    }

    #[test]
    fn project_log_invalid_address_returns_none() {
        let row = StoredEvent {
            contract: "not_an_address".into(),
            ..make_valid_stored_event()
        };
        assert!(project_log(&row).is_none());
    }

    #[test]
    fn project_log_invalid_tx_hash_returns_none() {
        let row = StoredEvent {
            tx_hash: "not_a_hash".into(),
            ..make_valid_stored_event()
        };
        assert!(project_log(&row).is_none());
    }

    #[test]
    fn project_log_invalid_topics_returns_none() {
        // raw_topics is not valid JSON — the topics parse step returns None via `?`.
        let row = StoredEvent {
            raw_topics: "THIS IS NOT JSON".into(),
            ..make_valid_stored_event()
        };
        assert!(project_log(&row).is_none());
    }

    // ------------------------------------------------------------------
    // project_rest_event tests
    // ------------------------------------------------------------------

    #[test]
    fn project_rest_event_valid_row_preserves_fields() {
        let row = make_valid_stored_event();
        let resp = project_rest_event(&row);

        assert_eq!(resp.contract, row.contract);
        assert_eq!(resp.event_name, row.event_name);
        assert_eq!(resp.topic0, row.topic0);
        assert_eq!(resp.block_number, row.block_number);
        assert_eq!(resp.block_hash, row.block_hash);
        assert_eq!(resp.tx_hash, row.tx_hash);
        assert_eq!(resp.tx_index, row.tx_index);
        assert_eq!(resp.log_index, row.log_index);
        assert_eq!(resp.source, row.source);
        assert_eq!(resp.timestamp, row.timestamp);
        // decoded JSON must round-trip to the same value
        let expected: serde_json::Value =
            serde_json::from_str(&row.decoded).unwrap();
        assert_eq!(resp.decoded, expected);
    }

    #[test]
    fn project_rest_event_invalid_decoded_json_falls_back_to_null() {
        let row = StoredEvent {
            decoded: "not valid json".into(),
            ..make_valid_stored_event()
        };
        let resp = project_rest_event(&row);
        assert_eq!(resp.decoded, serde_json::Value::Null);
    }
}
