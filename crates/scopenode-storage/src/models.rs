//! SQLite row types — direct representations of database rows.

use scopenode_core::abi::DecodedEvent;

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

impl From<DecodedEvent> for StoredEvent {
    fn from(e: DecodedEvent) -> Self {
        Self {
            contract: e.contract.to_checksum(None),
            event_name: e.event_name,
            topic0: e.topic0.to_string(),
            block_number: e.block_number as i64,
            block_hash: e.block_hash.to_string(),
            tx_hash: e.tx_hash.to_string(),
            tx_index: e.tx_index as i64,
            log_index: e.log_index as i64,
            raw_topics: serde_json::to_string(
                &e.raw_topics
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_default(),
            raw_data: alloy_primitives::hex::encode(&e.raw_data),
            decoded: e.decoded.to_string(),
            source: e.source,
            timestamp: e.timestamp as i64,
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, bytes, keccak256, Bytes, B256};
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

    #[test]
    fn from_decoded_event_serialises_address_as_checksum() {
        let e = make_decoded_event();
        let contract = e.contract;
        let stored: StoredEvent = e.into();
        assert_eq!(stored.contract, contract.to_checksum(None));
    }

    #[test]
    fn from_decoded_event_serialises_raw_data_as_hex() {
        let e = make_decoded_event();
        let stored: StoredEvent = e.into();
        assert_eq!(stored.raw_data, "deadbeef");
    }

    #[test]
    fn from_decoded_event_casts_indices_to_i64() {
        let e = make_decoded_event();
        let stored: StoredEvent = e.into();
        assert_eq!(stored.block_number, 18_000_000_i64);
        assert_eq!(stored.tx_index, 5_i64);
        assert_eq!(stored.log_index, 12_i64);
    }

    #[test]
    fn from_decoded_event_serialises_hashes_as_strings() {
        let e = make_decoded_event();
        let tx_hash = e.tx_hash;
        let block_hash = e.block_hash;
        let stored: StoredEvent = e.into();
        assert_eq!(stored.tx_hash, tx_hash.to_string());
        assert_eq!(stored.block_hash, block_hash.to_string());
    }

    #[test]
    fn from_decoded_event_preserves_scalar_fields() {
        let e = make_decoded_event();
        let stored: StoredEvent = e.into();
        assert_eq!(stored.event_name, "Transfer");
        assert_eq!(stored.source, "era1");
        assert_eq!(stored.timestamp, 1_700_000_000_i64);
    }

    #[test]
    fn from_decoded_event_topic0_from_keccak() {
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let e = DecodedEvent {
            contract: address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            event_name: "Transfer".to_string(),
            topic0,
            block_number: 1,
            block_hash: B256::ZERO,
            tx_hash: B256::ZERO,
            tx_index: 0,
            log_index: 0,
            raw_topics: vec![topic0],
            raw_data: Bytes::default(),
            decoded: json!({}),
            source: "era1".to_string(),
            timestamp: 0,
        };
        let stored: StoredEvent = e.into();
        assert_eq!(stored.topic0, topic0.to_string());
    }
}
