//! Event projection — converts a [`StoredEvent`] row into the shapes required
//! by the JSON-RPC and REST response layers, classifying every row's Decode
//! quality on the way.
//!
//! This module is the single source of truth for:
//! - hex hash parsing
//! - topic JSON parsing
//! - raw-data hex decoding
//! - decoded-JSON parsing
//! - field mapping onto [`Log`] / [`EventResponse`]
//! - Decode quality classification (`Valid` / `Lossy` / `Invalid`)
//!
//! Both `server.rs` and `rest.rs` consume one [`ProjectedRow`] per stored row
//! and map its explicit outcomes; adapters never repeat parsing.
//!
//! # Quality rules
//!
//! - **Invalid** — the JSON-RPC `Log` shape cannot be produced safely:
//!   malformed contract address, transaction/block hash, topics JSON, an
//!   individual topic value, or a negative identity field.
//! - **Lossy** — a response is produced but required a documented fallback:
//!   `raw_data` hex decode failure (empty data bytes) for the `Log` shape,
//!   or `decoded` JSON parse failure (`null`) for the REST shape.
//! - **Valid** — every field parsed without fallback.

use alloy::primitives::{Address, Bytes, LogData, B256};
use alloy::rpc::types::Log;
use scopenode_storage::models::StoredEvent;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Decode quality surfaced on REST responses. Absent when the row is `Valid`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DecodeQualityResponse {
    /// `"lossy"` or `"invalid"`.
    pub quality: &'static str,
    pub reason: String,
}

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
    /// Present only when projection degraded — distinguishes a fallback
    /// `null` in `decoded` from a stored JSON `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_quality: Option<DecodeQualityResponse>,
}

/// The JSON-RPC `Log` shape with its Decode quality made explicit.
#[derive(Debug)]
pub enum LogProjection {
    /// Every `Log` field parsed without fallback.
    Valid(Box<Log>),
    /// A `Log` was produced but `raw_data` fell back to empty bytes.
    Lossy { log: Box<Log>, reason: String },
    /// The `Log` shape cannot be produced safely from this row.
    Invalid { reason: String },
}

/// One stored row projected once for both transports.
#[derive(Debug)]
pub struct ProjectedRow {
    pub log: LogProjection,
    pub event: EventResponse,
}

// ---------------------------------------------------------------------------
// Public projection function
// ---------------------------------------------------------------------------

/// Project a [`StoredEvent`] row into both transport shapes, classifying its
/// Decode quality. This is the only projection entry point.
pub fn project_row(row: &StoredEvent) -> ProjectedRow {
    let log = project_log_shape(row);

    let (decoded, rest_lossy) = match serde_json::from_str(&row.decoded) {
        Ok(value) => (value, None),
        Err(e) => (
            serde_json::Value::Null,
            Some(format!("decoded JSON parse failed: {e}")),
        ),
    };

    let decode_quality = match &log {
        LogProjection::Invalid { reason } => Some(DecodeQualityResponse {
            quality: "invalid",
            reason: reason.clone(),
        }),
        LogProjection::Lossy { reason, .. } => {
            let mut reasons = vec![reason.clone()];
            reasons.extend(rest_lossy.clone());
            Some(DecodeQualityResponse {
                quality: "lossy",
                reason: reasons.join("; "),
            })
        }
        LogProjection::Valid(_) => rest_lossy.clone().map(|reason| DecodeQualityResponse {
            quality: "lossy",
            reason,
        }),
    };

    ProjectedRow {
        log,
        event: EventResponse {
            contract: row.contract.clone(),
            event_name: row.event_name.clone(),
            topic0: row.topic0.clone(),
            block_number: row.block_number,
            block_hash: row.block_hash.clone(),
            tx_hash: row.tx_hash.clone(),
            tx_index: row.tx_index,
            log_index: row.log_index,
            decoded,
            source: row.source.clone(),
            timestamp: row.timestamp,
            decode_quality,
        },
    }
}

fn project_log_shape(row: &StoredEvent) -> LogProjection {
    match try_log(row) {
        Ok((log, None)) => LogProjection::Valid(Box::new(log)),
        Ok((log, Some(reason))) => LogProjection::Lossy {
            log: Box::new(log),
            reason,
        },
        Err(reason) => LogProjection::Invalid { reason },
    }
}

/// Build the `Log` shape, or explain why it cannot be built. The optional
/// `String` reports a lossy `raw_data` fallback.
fn try_log(row: &StoredEvent) -> Result<(Log, Option<String>), String> {
    if row.block_number < 0 || row.tx_index < 0 || row.log_index < 0 {
        return Err(format!(
            "negative identity field (block {}, tx {}, log {})",
            row.block_number, row.tx_index, row.log_index
        ));
    }

    let address: Address = row
        .contract
        .parse()
        .map_err(|e| format!("contract address parse failed: {e}"))?;
    let tx_hash: B256 = row
        .tx_hash
        .parse()
        .map_err(|e| format!("tx_hash parse failed: {e}"))?;
    let block_hash: B256 = row
        .block_hash
        .parse()
        .map_err(|e| format!("block_hash parse failed: {e}"))?;

    let topic_strings: Vec<String> = serde_json::from_str(&row.raw_topics)
        .map_err(|e| format!("raw_topics JSON parse failed: {e}"))?;
    let topics: Vec<B256> = topic_strings
        .iter()
        .map(|t| {
            t.parse()
                .map_err(|e| format!("topic value parse failed: {e}"))
        })
        .collect::<Result<_, String>>()?;

    let (data_bytes, lossy) = match alloy_primitives::hex::decode(&row.raw_data) {
        Ok(bytes) => (bytes, None),
        Err(e) => (Vec::new(), Some(format!("raw_data hex decode failed: {e}"))),
    };

    let log_data = LogData::new(topics, Bytes::from(data_bytes))
        .ok_or_else(|| "log shape invalid: more than four topics".to_string())?;

    Ok((
        Log {
            inner: alloy::primitives::Log {
                address,
                data: log_data,
            },
            block_hash: Some(block_hash),
            block_number: Some(row.block_number as u64),
            block_timestamp: None,
            transaction_hash: Some(tx_hash),
            transaction_index: Some(row.tx_index as u64),
            log_index: Some(row.log_index as u64),
            removed: false,
        },
        lossy,
    ))
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

    fn invalid_reason(projected: &ProjectedRow) -> &str {
        match &projected.log {
            LogProjection::Invalid { reason } => reason,
            other => panic!("expected Invalid log projection, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Valid rows
    // ------------------------------------------------------------------

    #[test]
    fn valid_row_projects_valid_log_and_unflagged_event() {
        let row = make_valid_stored_event();
        let projected = project_row(&row);

        let log = match &projected.log {
            LogProjection::Valid(log) => log,
            other => panic!("expected Valid, got {other:?}"),
        };
        assert_eq!(log.block_number, Some(18_000_000_u64));
        assert_eq!(log.log_index, Some(7_u64));
        assert_eq!(log.transaction_index, Some(3_u64));
        assert!(!log.removed);

        assert!(projected.event.decode_quality.is_none());
        let expected: serde_json::Value = serde_json::from_str(&row.decoded).unwrap();
        assert_eq!(projected.event.decoded, expected);
    }

    #[test]
    fn empty_raw_topics_array_is_valid() {
        let row = StoredEvent {
            raw_topics: "[]".into(),
            ..make_valid_stored_event()
        };
        let projected = project_row(&row);
        assert!(matches!(projected.log, LogProjection::Valid(ref log)
            if log.inner.data.topics().is_empty()));
    }

    #[test]
    fn stored_json_null_decoded_stays_unflagged() {
        // A stored literal `null` is valid JSON — it must NOT carry a quality
        // marker, so callers can tell it apart from a parse-failure fallback.
        let row = StoredEvent {
            decoded: "null".into(),
            ..make_valid_stored_event()
        };
        let projected = project_row(&row);
        assert_eq!(projected.event.decoded, serde_json::Value::Null);
        assert!(projected.event.decode_quality.is_none());
    }

    #[test]
    fn u256_string_values_in_decoded_are_preserved() {
        let row = StoredEvent {
            decoded: "{\"amount\":\"1000000000000000000\"}".into(),
            ..make_valid_stored_event()
        };
        let projected = project_row(&row);
        let amount = projected
            .event
            .decoded
            .as_object()
            .and_then(|obj| obj.get("amount"))
            .and_then(|v| v.as_str());
        assert_eq!(
            amount,
            Some("1000000000000000000"),
            "U256 value must remain a string, not be coerced to a JSON number"
        );
    }

    // ------------------------------------------------------------------
    // Invalid rows — the Log shape cannot be produced safely
    // ------------------------------------------------------------------

    #[test]
    fn invalid_contract_address_classifies_invalid() {
        let row = StoredEvent {
            contract: "not_an_address".into(),
            ..make_valid_stored_event()
        };
        let projected = project_row(&row);
        assert!(invalid_reason(&projected).contains("contract address"));
        let quality = projected.event.decode_quality.expect("flagged on REST");
        assert_eq!(quality.quality, "invalid");
    }

    #[test]
    fn invalid_tx_hash_classifies_invalid() {
        let row = StoredEvent {
            tx_hash: "not_a_hash".into(),
            ..make_valid_stored_event()
        };
        assert!(invalid_reason(&project_row(&row)).contains("tx_hash"));
    }

    #[test]
    fn invalid_block_hash_classifies_invalid() {
        let row = StoredEvent {
            block_hash: "not_a_hash".into(),
            ..make_valid_stored_event()
        };
        assert!(invalid_reason(&project_row(&row)).contains("block_hash"));
    }

    #[test]
    fn malformed_raw_topics_json_classifies_invalid() {
        let row = StoredEvent {
            raw_topics: "THIS IS NOT JSON".into(),
            ..make_valid_stored_event()
        };
        assert!(invalid_reason(&project_row(&row)).contains("raw_topics"));
    }

    #[test]
    fn malformed_individual_topic_value_classifies_invalid() {
        // A topic that fails to parse must not be silently dropped — that
        // would change the event's semantics.
        let row = StoredEvent {
            raw_topics: "[\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\", \"bogus\"]".into(),
            ..make_valid_stored_event()
        };
        assert!(invalid_reason(&project_row(&row)).contains("topic value"));
    }

    #[test]
    fn negative_identity_fields_classify_invalid() {
        let row = StoredEvent {
            block_number: -1,
            ..make_valid_stored_event()
        };
        assert!(invalid_reason(&project_row(&row)).contains("negative identity"));
    }

    // ------------------------------------------------------------------
    // Lossy rows — produced with a documented fallback
    // ------------------------------------------------------------------

    #[test]
    fn invalid_raw_data_hex_classifies_lossy_with_empty_bytes() {
        let row = StoredEvent {
            raw_data: "not_hex!!!".into(),
            ..make_valid_stored_event()
        };
        let projected = project_row(&row);
        match &projected.log {
            LogProjection::Lossy { log, reason } => {
                assert!(reason.contains("raw_data"));
                assert!(log.inner.data.data.is_empty());
            }
            other => panic!("expected Lossy, got {other:?}"),
        }
        let quality = projected.event.decode_quality.expect("flagged on REST");
        assert_eq!(quality.quality, "lossy");
    }

    #[test]
    fn malformed_decoded_json_is_lossy_null_not_silent_null() {
        let row = StoredEvent {
            decoded: "not valid json".into(),
            ..make_valid_stored_event()
        };
        let projected = project_row(&row);

        // The Log shape doesn't carry decoded JSON — it stays Valid.
        assert!(matches!(projected.log, LogProjection::Valid(_)));

        // The REST shape falls back to null but says so.
        assert_eq!(projected.event.decoded, serde_json::Value::Null);
        let quality = projected.event.decode_quality.expect("must be flagged");
        assert_eq!(quality.quality, "lossy");
        assert!(quality.reason.contains("decoded JSON"));
    }
}
