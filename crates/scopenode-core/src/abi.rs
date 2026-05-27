//! ABI loading, caching, and event log decoding.
//!
//! ABIs are loaded from the mandatory `abi_override` file in config and cached
//! in SQLite after the first load to avoid repeated file I/O on every sync run.
//!
//! # Decoding pipeline
//! 1. [`EventAbi::topic0`] — compute keccak256 of the event signature
//! 2. [`EventDecoder::extract_and_decode`] — scan receipts, match logs by topic0
//! 3. [`EventDecoder::decode_log`] — split indexed (topics) vs non-indexed (data)
//! 4. [`decode_indexed_param`] / [`decode_abi_data`] — decode each parameter
//! 5. [`dyn_sol_value_to_json`] — convert to JSON for SQLite storage

use crate::config::ContractConfig;
use crate::error::AbiError;
use alloy_consensus::ReceiptEnvelope;
use alloy_dyn_abi::{DynSolType, DynSolValue};
use alloy_primitives::{keccak256, Address, Bytes, Log as PrimitiveLog, B256};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

/// A single input parameter definition from a Solidity event ABI.
#[derive(Debug, Clone, Deserialize)]
pub struct EventInput {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub indexed: bool,
    #[serde(default)]
    pub components: Vec<EventInput>,
}

/// Parsed ABI for a single Solidity event.
#[derive(Debug, Clone)]
pub struct EventAbi {
    pub name: String,
    pub inputs: Vec<EventInput>,
}

impl EventAbi {
    /// Compute the canonical event signature string used for topic0 computation.
    pub fn signature(&self) -> String {
        let params: Vec<String> = self
            .inputs
            .iter()
            .map(|i| canonical_type(&i.ty, &i.components))
            .collect();
        format!("{}({})", self.name, params.join(","))
    }

    /// Compute `topic0 = keccak256(canonical_signature)`.
    pub fn topic0(&self) -> B256 {
        keccak256(self.signature().as_bytes())
    }
}

fn canonical_type(ty: &str, components: &[EventInput]) -> String {
    if ty == "tuple" || ty.starts_with("tuple[") {
        let suffix = ty.strip_prefix("tuple").unwrap_or("");
        let inner: Vec<String> = components
            .iter()
            .map(|c| canonical_type(&c.ty, &c.components))
            .collect();
        format!("({}){}", inner.join(","), suffix)
    } else {
        ty.to_string()
    }
}

/// Read an ABI from a local JSON file (the `abi_override` config option).
pub fn load_abi_override(
    path: &std::path::Path,
    address: Address,
) -> Result<Vec<EventAbi>, AbiError> {
    let content = std::fs::read_to_string(path)?;
    let abi_array: Vec<Value> = serde_json::from_str(&content)
        .map_err(|e| AbiError::ParseFailed(address, e.to_string()))?;
    parse_events_from_abi(address, &abi_array)
}

/// Extract and parse all `event` entries from a raw ABI JSON array.
fn parse_events_from_abi(address: Address, abi_array: &[Value]) -> Result<Vec<EventAbi>, AbiError> {
    let events: Vec<EventAbi> = abi_array
        .iter()
        .filter(|entry| entry["type"].as_str() == Some("event"))
        .filter_map(|entry| {
            let name = entry["name"].as_str()?.to_string();
            let inputs = entry["inputs"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|i| serde_json::from_value::<EventInput>(i.clone()).ok())
                        .collect()
                })
                .unwrap_or_default();
            Some(EventAbi { name, inputs })
        })
        .collect();

    if events.is_empty() {
        return Err(AbiError::ParseFailed(
            address,
            "No event entries found in ABI".to_string(),
        ));
    }

    Ok(events)
}

/// Seam between ABI caching logic and its storage backend.
///
/// Implement this to provide a cache for ABI JSON strings. The two-method
/// interface keeps `AbiCache` free of any storage-crate dependency.
#[async_trait]
pub trait AbiStore: Send + Sync {
    async fn load(&self, address: &str) -> Result<Option<String>, AbiError>;
    async fn save(&self, address: &str, name: Option<&str>, abi_json: &str) -> Result<(), AbiError>;
}

/// Caches contract ABIs to avoid re-loading the override file every sync.
pub struct AbiCache {
    store: Arc<dyn AbiStore>,
}

impl AbiCache {
    pub fn new(store: Arc<dyn AbiStore>) -> Self {
        Self { store }
    }

    /// Get the event ABIs for a contract, loading from the cache or `abi_override`.
    ///
    /// Returns [`AbiError::AbiRequired`] if `abi_override` is not set.
    pub async fn get_or_fetch(&self, contract: &ContractConfig) -> Result<Vec<EventAbi>, AbiError> {
        let addr_str = contract.address.to_checksum(None);

        // Fast path: ABI already cached from a previous run.
        if let Ok(Some(cached)) = self.store.load(&addr_str).await {
            let all_events = parse_cached_events(contract.address, &cached)?;
            return filter_events(all_events, &contract.events, contract.address);
        }

        // Slow path: load from abi_override file. Missing abi_override is a config
        // error that should have been caught by Config::validate(), but we guard here
        // for safety in case someone constructs ContractConfig programmatically.
        let override_path = contract
            .abi_override
            .as_ref()
            .ok_or(AbiError::AbiRequired(contract.address))?;

        let all_events = load_abi_override(override_path, contract.address)?;

        let abi_json = serde_json::to_string(
            &all_events
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "inputs": e.inputs.iter().map(serialize_event_input).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();

        let _ = self.store.save(&addr_str, contract.name.as_deref(), &abi_json).await;

        filter_events(all_events, &contract.events, contract.address)
    }
}

fn parse_cached_events(address: Address, cached_json: &str) -> Result<Vec<EventAbi>, AbiError> {
    let entries: Vec<Value> = serde_json::from_str(cached_json)
        .map_err(|e| AbiError::ParseFailed(address, e.to_string()))?;

    let events = entries
        .iter()
        .filter_map(|entry| {
            let name = entry["name"].as_str()?.to_string();
            let inputs = entry["inputs"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|i| serde_json::from_value::<EventInput>(i.clone()).ok())
                        .collect()
                })
                .unwrap_or_default();
            Some(EventAbi { name, inputs })
        })
        .collect();

    Ok(events)
}

fn serialize_event_input(i: &EventInput) -> Value {
    serde_json::json!({
        "name": i.name,
        "type": i.ty,
        "indexed": i.indexed,
        "components": i.components.iter().map(serialize_event_input).collect::<Vec<_>>(),
    })
}

fn filter_events(
    all_events: Vec<EventAbi>,
    wanted: &[String],
    address: Address,
) -> Result<Vec<EventAbi>, AbiError> {
    let event_map: HashMap<String, EventAbi> = all_events
        .into_iter()
        .map(|e| (e.name.clone(), e))
        .collect();

    let mut result = Vec::new();
    for name in wanted {
        match event_map.get(name) {
            Some(e) => result.push(e.clone()),
            None => return Err(AbiError::EventNotFound(name.clone(), address)),
        }
    }
    Ok(result)
}

/// A fully-decoded event log carrying native Ethereum types.
///
/// Produced by [`EventDecoder::extract_and_decode`]. Callers that need to
/// persist events convert this to `StoredEvent` via a storage-layer adapter.
#[derive(Debug, Clone)]
pub struct DecodedEvent {
    pub contract: Address,
    pub event_name: String,
    pub topic0: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub tx_index: u64,
    pub log_index: u64,
    pub raw_topics: Vec<B256>,
    pub raw_data: Bytes,
    pub decoded: serde_json::Value,
    pub source: String,
    pub timestamp: u64,
}

/// Precomputed decode metadata for one Solidity event.
struct CompiledEvent {
    name: String,
    indexed_inputs: Vec<EventInput>,
    non_indexed_inputs: Vec<EventInput>,
    non_indexed_tuple_type: Option<DynSolType>,
}

impl CompiledEvent {
    fn new(event: &EventAbi) -> Result<Self, AbiError> {
        let indexed_inputs = event
            .inputs
            .iter()
            .filter(|input| input.indexed)
            .cloned()
            .collect();
        let non_indexed_inputs: Vec<EventInput> = event
            .inputs
            .iter()
            .filter(|input| !input.indexed)
            .cloned()
            .collect();

        let non_indexed_tuple_type = if non_indexed_inputs.is_empty() {
            None
        } else {
            let sol_types: Result<Vec<DynSolType>, _> = non_indexed_inputs
                .iter()
                .map(|input| input.ty.parse::<DynSolType>())
                .collect();
            Some(DynSolType::Tuple(sol_types.map_err(
                |e: alloy_dyn_abi::Error| AbiError::Decode(e.to_string()),
            )?))
        };

        Ok(Self {
            name: event.name.clone(),
            indexed_inputs,
            non_indexed_inputs,
            non_indexed_tuple_type,
        })
    }
}

/// Decodes raw Ethereum log data into named [`DecodedEvent`] values.
pub struct EventDecoder {
    events: HashMap<B256, CompiledEvent>,
    contract: Address,
}

impl EventDecoder {
    pub fn new(events: &[EventAbi], contract: Address) -> Result<Self, AbiError> {
        let mut map = HashMap::new();
        for event in events {
            map.insert(event.topic0(), CompiledEvent::new(event)?);
        }
        Ok(Self {
            events: map,
            contract,
        })
    }

    pub fn extract_and_decode(
        &self,
        receipts: &[ReceiptEnvelope<PrimitiveLog>],
        tx_hashes: &[B256],
        block_num: u64,
        block_hash: B256,
        timestamp: u64,
        source: &str,
    ) -> Vec<DecodedEvent> {
        let mut results = Vec::new();
        let mut cumulative_log_index: u64 = 0;

        for (tx_idx, receipt) in receipts.iter().enumerate() {
            let tx_hash = tx_hashes.get(tx_idx).copied().unwrap_or_default();
            for log in receipt.logs() {
                let log_index = cumulative_log_index;
                cumulative_log_index += 1;
                if let Some(event) = self.decode_log(
                    log,
                    block_num,
                    block_hash,
                    tx_hash,
                    tx_idx as u64,
                    log_index,
                    timestamp,
                    source,
                ) {
                    results.push(event);
                }
            }
        }

        results
    }

    /// Decode a single raw log into a `DecodedEvent`, or `None` if the log does
    /// not match this decoder's contract address and configured event signatures.
    ///
    /// This is the primary public entry point for log decoding. It can be called
    /// in tests with a synthetic `PrimitiveLog` and known block context to verify
    /// the full decode chain without constructing a receipt list.
    #[allow(clippy::too_many_arguments)]
    pub fn decode_log(
        &self,
        log: &PrimitiveLog,
        block_number: u64,
        block_hash: B256,
        tx_hash: B256,
        tx_index: u64,
        log_index: u64,
        timestamp: u64,
        source: &str,
    ) -> Option<DecodedEvent> {
        if log.address != self.contract {
            return None;
        }
        let topics = log.topics();
        let topic0 = *topics.first()?;
        let event_abi = self.events.get(&topic0)?;

        let raw_data = Bytes::from(log.data.data.clone().to_vec());
        let decoded = self.decode_fields(event_abi, topics, &raw_data);

        Some(DecodedEvent {
            contract: self.contract,
            event_name: event_abi.name.clone(),
            topic0,
            block_number,
            block_hash,
            tx_hash,
            tx_index,
            log_index,
            raw_topics: topics.to_vec(),
            raw_data,
            decoded,
            source: source.to_owned(),
            timestamp,
        })
    }

    fn decode_fields(&self, event: &CompiledEvent, topics: &[B256], data: &[u8]) -> Value {
        let mut obj = serde_json::Map::new();

        for (i, input) in event.indexed_inputs.iter().enumerate() {
            let topic_idx = i + 1;
            if topic_idx >= topics.len() {
                break;
            }
            let value = decode_indexed_param(&input.ty, &topics[topic_idx]);
            obj.insert(input.name.clone(), value);
        }

        if let Some(tuple_type) = &event.non_indexed_tuple_type {
            if !data.is_empty() {
                match decode_precompiled_abi_data(tuple_type, data) {
                    Ok(values) => {
                        for (input, value) in event.non_indexed_inputs.iter().zip(values.iter()) {
                            obj.insert(input.name.clone(), dyn_sol_value_to_json(value));
                        }
                    }
                    Err(e) => {
                        warn!(event = %event.name, err = %e, "Failed to decode non-indexed params");
                        obj.insert("_decode_error".to_string(), Value::String(e.to_string()));
                        obj.insert(
                            "_raw_data".to_string(),
                            Value::String(alloy_primitives::hex::encode(data)),
                        );
                    }
                }
            }
        }

        Value::Object(obj)
    }
}

fn decode_indexed_param(ty: &str, topic: &B256) -> Value {
    match ty {
        "address" => {
            let addr_bytes = &topic.as_slice()[12..];
            Value::String(format!("0x{}", alloy_primitives::hex::encode(addr_bytes)))
        }
        "bool" => Value::Bool(topic[31] != 0),
        t if t.starts_with("uint") || t.starts_with("int") => {
            let hex = alloy_primitives::hex::encode(topic.as_slice());
            let trimmed = hex.trim_start_matches('0');
            let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
            Value::String(format!("0x{}", trimmed))
        }
        t if t.starts_with("bytes") && t.len() > 5 => {
            let size: usize = t[5..].parse().unwrap_or(32);
            let size = size.min(32);
            Value::String(format!(
                "0x{}",
                alloy_primitives::hex::encode(&topic.as_slice()[..size])
            ))
        }
        _ => Value::String(format!(
            "0x{}",
            alloy_primitives::hex::encode(topic.as_slice())
        )),
    }
}

fn decode_precompiled_abi_data(
    tuple_type: &DynSolType,
    data: &[u8],
) -> Result<Vec<DynSolValue>, AbiError> {
    let decoded = tuple_type
        .abi_decode(data)
        .map_err(|e| AbiError::Decode(e.to_string()))?;

    match decoded {
        DynSolValue::Tuple(values) => Ok(values),
        other => Ok(vec![other]),
    }
}

fn dyn_sol_value_to_json(value: &DynSolValue) -> Value {
    match value {
        DynSolValue::Bool(b) => Value::Bool(*b),
        DynSolValue::Int(n, _) => Value::String(n.to_string()),
        DynSolValue::Uint(n, _) => Value::String(n.to_string()),
        DynSolValue::FixedBytes(b, _) => {
            Value::String(format!("0x{}", alloy_primitives::hex::encode(b.as_slice())))
        }
        DynSolValue::Address(a) => Value::String(a.to_checksum(None)),
        DynSolValue::Bytes(b) => Value::String(format!("0x{}", alloy_primitives::hex::encode(b))),
        DynSolValue::String(s) => Value::String(s.clone()),
        DynSolValue::Array(arr) | DynSolValue::FixedArray(arr) => {
            Value::Array(arr.iter().map(dyn_sol_value_to_json).collect())
        }
        DynSolValue::Tuple(items) => {
            Value::Array(items.iter().map(dyn_sol_value_to_json).collect())
        }
        _ => Value::String("[unknown]".to_string()),
    }
}

#[cfg(test)]
mod era1_tests {
    use super::*;
    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{address, keccak256, Bloom, Bytes, Log as PrimitiveLog, LogData, B256};

    #[test]
    fn decode_log_returns_decoded_event_for_matching_log() {
        let contract = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let topic0 = keccak256(b"Transfer(address,address,uint256)");

        let events = vec![EventAbi {
            name: "Transfer".into(),
            inputs: vec![
                EventInput { name: "from".into(), ty: "address".into(), indexed: true, components: vec![] },
                EventInput { name: "to".into(), ty: "address".into(), indexed: true, components: vec![] },
                EventInput { name: "value".into(), ty: "uint256".into(), indexed: false, components: vec![] },
            ],
        }];
        let decoder = EventDecoder::new(&events, contract).unwrap();

        let from_topic = B256::from([0x11u8; 32]);
        let to_topic = B256::from([0x22u8; 32]);
        let value_bytes = {
            let mut b = [0u8; 32];
            b[31] = 42;
            Bytes::from(b.to_vec())
        };
        let log = PrimitiveLog {
            address: contract,
            data: LogData::new_unchecked(
                vec![topic0, from_topic, to_topic],
                value_bytes,
            ),
        };

        let result = decoder.decode_log(
            &log,
            18_000_000,
            B256::from([0xAA; 32]),
            B256::from([0xBB; 32]),
            3,
            7,
            1_700_000_000,
            "era1",
        );

        let event = result.expect("matching log must decode to Some");
        assert_eq!(event.event_name, "Transfer");
        assert_eq!(event.block_number, 18_000_000);
        assert_eq!(event.tx_index, 3);
        assert_eq!(event.log_index, 7);
        assert_eq!(event.source, "era1");
        assert!(event.decoded.get("from").is_some());
        assert!(event.decoded.get("to").is_some());
        assert!(event.decoded.get("value").is_some());
    }

    #[test]
    fn decode_log_returns_none_for_wrong_contract() {
        let contract = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let other = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let topic0 = keccak256(b"Transfer(address,address,uint256)");

        let events = vec![EventAbi {
            name: "Transfer".into(),
            inputs: vec![],
        }];
        let decoder = EventDecoder::new(&events, contract).unwrap();

        let log = PrimitiveLog {
            address: other,
            data: LogData::new_unchecked(vec![topic0], Bytes::new()),
        };
        assert!(decoder.decode_log(&log, 0, B256::ZERO, B256::ZERO, 0, 0, 0, "era1").is_none());
    }

    #[test]
    fn decode_log_returns_none_for_unknown_topic0() {
        let contract = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let unknown = keccak256(b"Unknown(address)");

        let events = vec![EventAbi { name: "Transfer".into(), inputs: vec![] }];
        let decoder = EventDecoder::new(&events, contract).unwrap();

        let log = PrimitiveLog {
            address: contract,
            data: LogData::new_unchecked(vec![unknown], Bytes::new()),
        };
        let _ = topic0; // used above to construct decoder
        assert!(decoder.decode_log(&log, 0, B256::ZERO, B256::ZERO, 0, 0, 0, "era1").is_none());
    }

    #[test]
    fn event_decoder_rejects_invalid_non_indexed_type_at_construction() {
        let contract = Address::repeat_byte(0x11);
        let events = vec![EventAbi {
            name: "Bad".to_string(),
            inputs: vec![EventInput {
                name: "broken".to_string(),
                ty: "definitely_not_a_solidity_type".to_string(),
                indexed: false,
                components: vec![],
            }],
        }];

        let err = match EventDecoder::new(&events, contract) {
            Ok(_) => panic!("invalid type should fail when decoder is built"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("decode"));
    }

    #[test]
    fn extract_and_decode_finds_matching_log() {
        let contract = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let topic0 = keccak256(b"Transfer(address,address,uint256)");

        let log = PrimitiveLog {
            address: contract,
            data: LogData::new_unchecked(
                vec![topic0, B256::ZERO, B256::ZERO],
                Bytes::from(vec![0u8; 32]),
            ),
        };

        let receipt = ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![log],
            },
            logs_bloom: Bloom::default(),
        };
        let receipts = vec![ReceiptEnvelope::Legacy(receipt)];

        let tx_hash = B256::from([0x42u8; 32]);
        let tx_hashes = vec![tx_hash];

        let events = vec![EventAbi {
            name: "Transfer".into(),
            inputs: vec![
                EventInput {
                    name: "from".into(),
                    ty: "address".into(),
                    indexed: true,
                    components: vec![],
                },
                EventInput {
                    name: "to".into(),
                    ty: "address".into(),
                    indexed: true,
                    components: vec![],
                },
                EventInput {
                    name: "value".into(),
                    ty: "uint256".into(),
                    indexed: false,
                    components: vec![],
                },
            ],
        }];

        let decoder = EventDecoder::new(&events, contract).unwrap();
        let decoded =
            decoder.extract_and_decode(&receipts, &tx_hashes, 100, B256::ZERO, 1_000_000, "era1");

        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].event_name, "Transfer");
        assert_eq!(decoded[0].tx_hash, tx_hash);
        assert_eq!(decoded[0].tx_index, 0);
        assert_eq!(decoded[0].log_index, 0);
        assert_eq!(decoded[0].source, "era1");
        assert_eq!(decoded[0].block_number, 100);
        assert_eq!(decoded[0].contract, contract);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_type_primitive() {
        assert_eq!(canonical_type("address", &[]), "address");
        assert_eq!(canonical_type("uint256", &[]), "uint256");
        assert_eq!(canonical_type("bytes32", &[]), "bytes32");
    }

    #[test]
    fn canonical_type_tuple_expands_components() {
        let components = vec![
            EventInput {
                name: "a".into(),
                ty: "address".into(),
                indexed: false,
                components: vec![],
            },
            EventInput {
                name: "b".into(),
                ty: "uint256".into(),
                indexed: false,
                components: vec![],
            },
        ];
        assert_eq!(canonical_type("tuple", &components), "(address,uint256)");
    }

    #[test]
    fn canonical_type_tuple_array_keeps_suffix() {
        let components = vec![EventInput {
            name: "x".into(),
            ty: "uint128".into(),
            indexed: false,
            components: vec![],
        }];
        assert_eq!(canonical_type("tuple[]", &components), "(uint128)[]");
        assert_eq!(canonical_type("tuple[3]", &components), "(uint128)[3]");
    }

    #[test]
    fn canonical_type_nested_tuple() {
        let inner = vec![EventInput {
            name: "y".into(),
            ty: "bool".into(),
            indexed: false,
            components: vec![],
        }];
        let outer = vec![
            EventInput {
                name: "inner".into(),
                ty: "tuple".into(),
                indexed: false,
                components: inner,
            },
            EventInput {
                name: "z".into(),
                ty: "address".into(),
                indexed: false,
                components: vec![],
            },
        ];
        assert_eq!(canonical_type("tuple", &outer), "((bool),address)");
    }

    #[test]
    fn parse_events_ignores_functions_and_errors() {
        let addr = Address::ZERO;
        let abi = serde_json::json!([
            { "type": "constructor", "inputs": [] },
            { "type": "function",    "name": "transfer", "inputs": [] },
            { "type": "error",       "name": "NotOwner", "inputs": [] },
            { "type": "event",       "name": "Transfer", "inputs": [
                { "name": "from",  "type": "address", "indexed": true,  "components": [] },
                { "name": "to",    "type": "address", "indexed": true,  "components": [] },
                { "name": "value", "type": "uint256", "indexed": false, "components": [] }
            ]}
        ]);
        let events = parse_events_from_abi(addr, abi.as_array().unwrap()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "Transfer");
    }

    #[test]
    fn parse_events_empty_abi_returns_error() {
        let addr = Address::ZERO;
        let result = parse_events_from_abi(addr, &[]);
        assert!(matches!(result, Err(AbiError::ParseFailed(_, _))));
    }

    #[test]
    fn parse_events_no_event_entries_returns_error() {
        let addr = Address::ZERO;
        let abi = serde_json::json!([
            { "type": "function", "name": "foo", "inputs": [] }
        ]);
        let result = parse_events_from_abi(addr, abi.as_array().unwrap());
        assert!(matches!(result, Err(AbiError::ParseFailed(_, _))));
    }

    #[test]
    fn filter_events_unknown_name_returns_error() {
        let addr = Address::ZERO;
        let all = vec![EventAbi {
            name: "Transfer".into(),
            inputs: vec![],
        }];
        let result = filter_events(all, &["Transfer".to_string(), "Mint".to_string()], addr);
        assert!(matches!(result, Err(AbiError::EventNotFound(ref n, _)) if n == "Mint"));
    }

    #[test]
    fn filter_events_returns_only_requested() {
        let addr = Address::ZERO;
        let all = vec![
            EventAbi {
                name: "Transfer".into(),
                inputs: vec![],
            },
            EventAbi {
                name: "Approval".into(),
                inputs: vec![],
            },
            EventAbi {
                name: "Mint".into(),
                inputs: vec![],
            },
        ];
        let result = filter_events(all, &["Transfer".to_string()], addr).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "Transfer");
    }

    #[test]
    fn event_signature_and_topic0() {
        use alloy_primitives::keccak256;
        let event = EventAbi {
            name: "Transfer".to_string(),
            inputs: vec![
                EventInput {
                    name: "from".to_string(),
                    ty: "address".to_string(),
                    indexed: true,
                    components: vec![],
                },
                EventInput {
                    name: "to".to_string(),
                    ty: "address".to_string(),
                    indexed: true,
                    components: vec![],
                },
                EventInput {
                    name: "value".to_string(),
                    ty: "uint256".to_string(),
                    indexed: false,
                    components: vec![],
                },
            ],
        };
        let sig = event.signature();
        assert_eq!(sig, "Transfer(address,address,uint256)");
        let expected_topic0 = keccak256(b"Transfer(address,address,uint256)");
        assert_eq!(event.topic0(), expected_topic0);
    }

    #[test]
    fn swap_event_topic0() {
        use alloy_primitives::keccak256;
        let event = EventAbi {
            name: "Swap".to_string(),
            inputs: vec![
                EventInput {
                    name: "sender".to_string(),
                    ty: "address".to_string(),
                    indexed: true,
                    components: vec![],
                },
                EventInput {
                    name: "recipient".to_string(),
                    ty: "address".to_string(),
                    indexed: true,
                    components: vec![],
                },
                EventInput {
                    name: "amount0".to_string(),
                    ty: "int256".to_string(),
                    indexed: false,
                    components: vec![],
                },
                EventInput {
                    name: "amount1".to_string(),
                    ty: "int256".to_string(),
                    indexed: false,
                    components: vec![],
                },
                EventInput {
                    name: "sqrtPriceX96".to_string(),
                    ty: "uint160".to_string(),
                    indexed: false,
                    components: vec![],
                },
                EventInput {
                    name: "liquidity".to_string(),
                    ty: "uint128".to_string(),
                    indexed: false,
                    components: vec![],
                },
                EventInput {
                    name: "tick".to_string(),
                    ty: "int24".to_string(),
                    indexed: false,
                    components: vec![],
                },
            ],
        };
        let sig = event.signature();
        assert_eq!(
            sig,
            "Swap(address,address,int256,int256,uint160,uint128,int24)"
        );
        let expected = keccak256(b"Swap(address,address,int256,int256,uint160,uint128,int24)");
        assert_eq!(event.topic0(), expected);
    }
}
