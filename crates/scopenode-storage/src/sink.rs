//! `DbEventSink` — adapts `Db::insert_events` to the `EventSink` trait.

use crate::models::StoredEvent;
use crate::Db;
use async_trait::async_trait;
use scopenode_core::abi::DecodedEvent;
use scopenode_core::era_pipeline::{CoverageSink, EventSink};
use scopenode_core::error::CoreError;

/// Writes decoded events to SQLite by converting them to `StoredEvent` rows.
pub struct DbEventSink {
    db: Db,
}

impl DbEventSink {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl EventSink for DbEventSink {
    async fn store(&self, events: Vec<DecodedEvent>) -> Result<usize, CoreError> {
        if events.is_empty() {
            return Ok(0);
        }
        let count = events.len();
        let stored: Vec<StoredEvent> = events.into_iter().map(Into::into).collect();
        self.db
            .insert_events(&stored)
            .await
            .map_err(|e| CoreError::Storage(e.to_string()))?;
        Ok(count)
    }
}

#[async_trait]
impl CoverageSink for DbEventSink {
    async fn record_coverage(
        &self,
        contract: &str,
        from_block: u64,
        to_block: u64,
    ) -> Result<(), CoreError> {
        self.db
            .record_covered_range(contract, from_block, to_block)
            .await
            .map_err(|e| CoreError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, bytes, B256};
    use serde_json::json;

    fn make_decoded_event() -> DecodedEvent {
        DecodedEvent {
            contract: address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            event_name: "Transfer".to_string(),
            topic0: B256::from([0x11u8; 32]),
            block_number: 18_000_000,
            block_hash: B256::from([0x22u8; 32]),
            tx_hash: B256::from([0x33u8; 32]),
            tx_index: 5,
            log_index: 12,
            raw_topics: vec![B256::from([0x11u8; 32]), B256::from([0x44u8; 32])],
            raw_data: bytes!("deadbeef"),
            decoded: json!({"from": "0xabc", "to": "0xdef", "value": "1000"}),
            source: "era1".to_string(),
            timestamp: 1_700_000_000,
        }
    }

    #[tokio::test]
    async fn db_event_sink_store_inserts_events() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = tmp.into_parts();
        drop(file);
        let db = Db::open(path.to_path_buf()).await.unwrap();
        let addr_str = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").to_checksum(None);
        db.upsert_contract(&addr_str, Some("USDC"), "[]")
            .await
            .unwrap();

        let sink = DbEventSink::new(db.clone());
        let n = sink.store(vec![make_decoded_event()]).await.unwrap();
        assert_eq!(n, 1);

        let count = db.count_events_for_contract(&addr_str).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn db_event_sink_store_empty_returns_zero() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = tmp.into_parts();
        drop(file);
        let db = Db::open(path.to_path_buf()).await.unwrap();
        let sink = DbEventSink::new(db);
        let n = sink.store(vec![]).await.unwrap();
        assert_eq!(n, 0);
    }
}
