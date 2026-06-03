//! Event projection — converts a [`StoredEvent`] row into the shapes required
//! by the JSON-RPC and REST response layers.
//!
//! This module is the single source of truth for:
//! - hex hash parsing
//! - topic JSON parsing
//! - raw-data hex decoding
//! - field mapping onto [`Log`] / [`EventResponse`]
//!
//! Both `server.rs` and `rest.rs` delegate all row-to-response conversion to
//! this module.

use alloy::primitives::{Address, Bytes, LogData, B256};
use alloy::rpc::types::Log;
use scopenode_core::decode_quality::DecodeQuality;
use scopenode_storage::models::StoredEvent;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// REST-layer shape for a single event row, shared by the REST adapter.
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

/// A projected JSON-RPC log plus the quality of the projection.
#[derive(Debug)]
pub struct ProjectedLog {
    pub log: Log,
    pub quality: DecodeQuality,
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
    project_log_with_quality(row).map(|projected| projected.log)
}

/// Convert a [`StoredEvent`] row into an `alloy` [`Log`] and report whether
/// any lossy fallback was needed.
pub fn project_log_with_quality(row: &StoredEvent) -> Option<ProjectedLog> {
    let address: Address = row.contract.parse().ok()?;
    let tx_hash: B256 = row.tx_hash.parse().ok()?;
    let block_hash: B256 = row.block_hash.parse().ok()?;

    let topics: Vec<B256> = serde_json::from_str::<Vec<String>>(&row.raw_topics)
        .ok()?
        .iter()
        .filter_map(|t| t.parse().ok())
        .collect();

    let (data_bytes, quality) = match alloy_primitives::hex::decode(&row.raw_data) {
        Ok(bytes) => (bytes, DecodeQuality::Valid),
        Err(e) => (
            Vec::new(),
            DecodeQuality::lossy(format!("raw_data hex decode failed: {e}")),
        ),
    };
    let data = Bytes::from(data_bytes);

    let log_data = LogData::new(topics, data)?;

    let inner_log = alloy::primitives::Log {
        address,
        data: log_data,
    };

    Some(ProjectedLog {
        quality,
        log: Log {
            inner: inner_log,
            block_hash: Some(block_hash),
            block_number: Some(row.block_number as u64),
            block_timestamp: None,
            transaction_hash: Some(tx_hash),
            transaction_index: Some(row.tx_index as u64),
            log_index: Some(row.log_index as u64),
            removed: false,
        },
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
            raw_topics: "[\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\"]"
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
        let expected: serde_json::Value = serde_json::from_str(&row.decoded).unwrap();
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

    // ------------------------------------------------------------------
    // Additional malformed-row tests (issue #33)
    // ------------------------------------------------------------------

    #[test]
    fn project_log_invalid_block_hash_returns_none() {
        // block_hash cannot be parsed as B256 — the `?` on `parse().ok()?`
        // propagates None, so the entire projection fails.
        let row = StoredEvent {
            block_hash: "not_a_hash".into(),
            ..make_valid_stored_event()
        };
        assert!(project_log(&row).is_none());
    }

    #[test]
    fn project_log_empty_raw_topics_array_produces_log() {
        // An empty JSON array is valid for raw_topics. LogData accepts zero
        // topics, so the projection should succeed and return Some(Log).
        let row = StoredEvent {
            raw_topics: "[]".into(),
            ..make_valid_stored_event()
        };
        let log = project_log(&row).expect("empty topics array should still produce Some(Log)");
        assert!(log.inner.data.topics().is_empty());
    }

    #[test]
    fn project_log_invalid_raw_data_hex_falls_back_to_empty_bytes() {
        // alloy_primitives::hex::decode returns Err for non-hex input.
        // The implementation calls .unwrap_or_default() which yields an empty
        // Vec, so the log is still produced (Some) but with zero data bytes.
        let row = StoredEvent {
            raw_data: "not_hex!!!".into(),
            ..make_valid_stored_event()
        };
        let log = project_log(&row).expect("invalid hex raw_data should still produce Some(Log)");
        assert_eq!(
            log.inner.data.data.len(),
            0,
            "data bytes should be empty when raw_data is not valid hex"
        );
    }

    #[test]
    fn project_log_with_quality_marks_invalid_raw_data_lossy() {
        let row = StoredEvent {
            raw_data: "not_hex!!!".into(),
            ..make_valid_stored_event()
        };

        let projected = project_log_with_quality(&row).expect("valid row shape should project");

        assert!(matches!(
            projected.quality,
            scopenode_core::decode_quality::DecodeQuality::Lossy { .. }
        ));
    }

    #[test]
    fn project_rest_event_null_decoded_json_falls_back_to_null() {
        // The JSON literal "null" is valid JSON: serde_json::from_str("null")
        // parses successfully as Value::Null. The result must be Value::Null,
        // not a parse error, confirming the pass-through behaviour is stable.
        let row = StoredEvent {
            decoded: "null".into(),
            ..make_valid_stored_event()
        };
        let resp = project_rest_event(&row);
        assert_eq!(resp.decoded, serde_json::Value::Null);
    }

    #[test]
    fn project_rest_event_nested_object_decoded_preserves_structure() {
        // U256 amounts are stored as JSON strings to avoid JavaScript precision
        // loss. The projection must preserve the string type exactly — not
        // coerce "1000000000000000000" into a JSON number.
        let row = StoredEvent {
            decoded: "{\"amount\":\"1000000000000000000\"}".into(),
            ..make_valid_stored_event()
        };
        let resp = project_rest_event(&row);
        let obj = resp
            .decoded
            .as_object()
            .expect("decoded should be a JSON object");
        let amount = obj.get("amount").expect("key \"amount\" must be present");
        assert_eq!(
            amount.as_str(),
            Some("1000000000000000000"),
            "U256 value must remain a string, not be coerced to a JSON number"
        );
    }
}
