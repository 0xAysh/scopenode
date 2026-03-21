//! ABI fetching, caching, and event log decoding.
//!
//! **Sourcify** (<https://sourcify.dev>) is the Ethereum Foundation's open contract
//! verification platform — no API key required. We fetch the contract's
//! `metadata.json` from Sourcify, which contains the full ABI under
//! `output.abi`.
//!
//! ABIs are cached in SQLite after the first fetch to avoid repeated network
//! calls on every `scopenode sync` run. If the cache is warm, the pipeline
//! starts without any HTTP requests.
//!
//! If a contract is not on Sourcify, the user must set `abi_override` in their
//! config to point to a local ABI JSON file (e.g. exported from Hardhat or Foundry).
//!
//! # Decoding pipeline
//! 1. [`EventAbi::topic0`] — compute keccak256 of the event signature
//! 2. [`EventDecoder::extract_and_decode`] — scan receipts, match logs by topic0
//! 3. [`EventDecoder::decode_log`] — split indexed (topics) vs non-indexed (data)
//! 4. [`decode_indexed_param`] / [`decode_abi_data`] — decode each parameter
//! 5. [`dyn_sol_value_to_json`] — convert to JSON for SQLite storage

use crate::config::ContractConfig;
use crate::error::AbiError;
use crate::types::StoredEvent;
use alloy::rpc::types::TransactionReceipt;
use alloy_dyn_abi::{DynSolType, DynSolValue};
use alloy_primitives::{keccak256, Address, Bytes, B256};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use tracing::{debug, warn};

/// A single input parameter definition from a Solidity event ABI.
///
/// This mirrors the JSON structure returned by the Solidity compiler and
/// stored in the Sourcify `metadata.json` under `output.abi[*].inputs[*]`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventInput {
    /// Parameter name as declared in Solidity (e.g. `"sender"`, `"amount0"`).
    pub name: String,

    /// ABI type string (e.g. `"address"`, `"uint256"`, `"int256"`, `"bytes32"`, `"tuple"`).
    ///
    /// For array types this includes the suffix, e.g. `"uint256[]"` or `"address[3]"`.
    /// For tuple types, see `components` for the inner field types.
    #[serde(rename = "type")]
    pub ty: String,

    /// True if this parameter is stored in a log topic (indexed params are topic1..topic3).
    ///
    /// False means the parameter is ABI-encoded in the log's `data` field.
    /// Solidity allows at most 3 indexed parameters per event (topics[1..3]);
    /// topic[0] is always the event selector (topic0).
    pub indexed: bool,

    /// For `tuple` types only: the component types that make up the tuple.
    ///
    /// Empty for all non-tuple types. For nested tuples, components can themselves
    /// have non-empty `components`.
    #[serde(default)]
    pub components: Vec<EventInput>,
}

/// Parsed ABI for a single Solidity event.
///
/// Used to compute topic0, build bloom targets, and decode raw log data into
/// named fields. Constructed from the Sourcify API response or a local ABI file.
#[derive(Debug, Clone)]
pub struct EventAbi {
    /// Event name as declared in Solidity (e.g. `"Swap"`, `"Transfer"`).
    pub name: String,
    /// Ordered list of all input parameters (both indexed and non-indexed).
    pub inputs: Vec<EventInput>,
}

impl EventAbi {
    /// Compute the canonical event signature string used for topic0 computation.
    ///
    /// Format: `"EventName(type1,type2,...)"` with no spaces. Tuple types are
    /// expanded recursively, e.g. `"(address,uint256)"`.
    ///
    /// # Example
    /// ```text
    /// Swap(address,address,int256,int256,uint160,uint128,int24)
    /// ```
    pub fn signature(&self) -> String {
        let params: Vec<String> = self
            .inputs
            .iter()
            .map(|i| canonical_type(&i.ty, &i.components))
            .collect();
        format!("{}({})", self.name, params.join(","))
    }

    /// Compute `topic0 = keccak256(canonical_signature)`.
    ///
    /// This is the first topic in every log emitted by this event, and the value
    /// we look for in bloom filters and log filtering. Every contract that emits
    /// this event will use the same topic0 — it's globally unique to the event ABI.
    ///
    /// Example: `Transfer(address,address,uint256)` → `0xddf252ad...`
    pub fn topic0(&self) -> B256 {
        keccak256(self.signature().as_bytes())
    }
}

/// Produce the canonical ABI type string for a parameter, handling tuple recursion.
///
/// Tuples are encoded as `"(type1,type2,...)"` with an optional array suffix
/// like `"[]"` or `"[3]"`. The suffix is preserved from the original type string
/// (e.g. `"tuple[]"` → `"(address,uint256)[]"`).
fn canonical_type(ty: &str, components: &[EventInput]) -> String {
    if ty == "tuple" || ty.starts_with("tuple[") {
        // Strip the "tuple" prefix, keep any array suffix (e.g. "[]", "[3]").
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

/// HTTP client for the Sourcify contract verification API (`sourcify.dev`).
///
/// Makes unauthenticated GET requests — no API key needed. Sourcify is run by
/// the Ethereum Foundation and provides free access to verified contract ABIs.
pub struct SourcifyClient {
    http: reqwest::Client,
}

impl SourcifyClient {
    /// Create a new Sourcify client with a default `reqwest` HTTP client.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// Fetch all event definitions for a contract from Sourcify.
    ///
    /// Requests the Sourcify `/files/any/1/{address}` endpoint, which returns
    /// all verified source files for the contract on mainnet (chain ID 1).
    /// We parse `metadata.json` from the response to extract `output.abi`.
    ///
    /// Returns only the `event` entries from the ABI (ignoring `function`,
    /// `error`, `constructor`, etc.).
    ///
    /// Returns [`AbiError::NotOnSourcify`] if the contract has no verified source.
    pub async fn fetch_events(
        &self,
        address: Address,
    ) -> Result<Vec<EventAbi>, AbiError> {
        let url = format!(
            "https://sourcify.dev/server/files/any/1/{}",
            address.to_checksum(None)
        );

        debug!(%address, %url, "Fetching ABI from Sourcify");

        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| AbiError::FetchFailed(address, e.to_string()))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(AbiError::NotOnSourcify(address));
        }

        if !response.status().is_success() {
            return Err(AbiError::FetchFailed(
                address,
                format!("HTTP {}", response.status()),
            ));
        }

        let body: Value = response
            .json()
            .await
            .map_err(|e| AbiError::ParseFailed(address, e.to_string()))?;

        // Find metadata.json in the files array — Sourcify returns multiple source
        // files; we only need the metadata which contains the compiled ABI.
        let files = body["files"]
            .as_array()
            .ok_or_else(|| AbiError::ParseFailed(address, "No 'files' array".to_string()))?;

        let metadata_file = files
            .iter()
            .find(|f| {
                f["name"]
                    .as_str()
                    .map(|n| n.ends_with("metadata.json"))
                    .unwrap_or(false)
            })
            .ok_or_else(|| AbiError::ParseFailed(address, "No metadata.json in files".to_string()))?;

        let content_str = metadata_file["content"]
            .as_str()
            .ok_or_else(|| AbiError::ParseFailed(address, "metadata.json has no content".to_string()))?;

        let metadata: Value = serde_json::from_str(content_str)
            .map_err(|e| AbiError::ParseFailed(address, e.to_string()))?;

        // The ABI lives at metadata.output.abi — this is the standard Solidity
        // compiler metadata format (solc --metadata).
        let abi_array = metadata["output"]["abi"]
            .as_array()
            .ok_or_else(|| AbiError::ParseFailed(address, "No output.abi in metadata".to_string()))?;

        parse_events_from_abi(address, abi_array)
    }
}

impl Default for SourcifyClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Read an ABI from a local JSON file (the `abi_override` config option).
///
/// The file must be a JSON array of ABI entries in the standard Ethereum ABI
/// format (same as what `solc --abi`, Hardhat, or Foundry produce). Only `event`
/// entries are used; `function`, `error`, and `constructor` entries are ignored.
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
        // Only keep entries with "type": "event" — skip functions, errors, constructors.
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

/// Caches contract ABIs in SQLite to avoid fetching from Sourcify on every sync.
///
/// On first use, fetches from Sourcify (or the `abi_override` file) and stores
/// in the DB as a JSON string. Subsequent runs load from SQLite directly.
pub struct AbiCache {
    sourcify: SourcifyClient,
    db: scopenode_storage::Db,
}

impl AbiCache {
    /// Create a new cache backed by the given database handle.
    pub fn new(db: scopenode_storage::Db) -> Self {
        Self {
            sourcify: SourcifyClient::new(),
            db,
        }
    }

    /// Get the event ABIs for the events listed in a [`ContractConfig`].
    ///
    /// Checks the SQLite cache first. If not cached, fetches from Sourcify or
    /// the `abi_override` file, stores the result in SQLite, and returns only
    /// the events listed in `contract.events` (not the full ABI).
    ///
    /// If `contract.impl_address` is set, the ABI is fetched from that address
    /// on Sourcify instead of `contract.address`. The proxy emits the events
    /// so `address` is still used for bloom scanning and log matching.
    ///
    /// Returns [`AbiError::EventNotFound`] if an event name from the config is
    /// not present in the ABI.
    pub async fn get_or_fetch(
        &self,
        contract: &ContractConfig,
    ) -> Result<Vec<EventAbi>, AbiError> {
        let addr_str = contract.address.to_checksum(None);

        // Fast path: ABI already in SQLite from a previous run.
        if let Ok(Some(cached)) = self.db.get_contract_abi(&addr_str).await {
            let all_events = parse_cached_events(contract.address, &cached)?;
            return filter_events(all_events, &contract.events, contract.address);
        }

        // Resolve which address to use for ABI lookup.
        // For proxy contracts, the implementation holds the real ABI.
        let abi_address = contract.impl_address.unwrap_or(contract.address);
        if let Some(impl_addr) = contract.impl_address {
            debug!(
                proxy = %contract.address,
                implementation = %impl_addr,
                "Using impl_address for ABI lookup"
            );
        }

        // Slow path: fetch from Sourcify or abi_override file.
        let all_events = if let Some(override_path) = &contract.abi_override {
            load_abi_override(override_path, contract.address)?
        } else {
            self.sourcify.fetch_events(abi_address).await?
        };

        // Serialize the full ABI to JSON for storage. We store ALL events from
        // the ABI (not just the configured subset) so future re-runs with different
        // event configs don't need to re-fetch.
        // IMPORTANT: must include `components` for tuple types or the cache will
        // return incomplete ABIs and tuple decoding will fail on second run.
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

        // Cache in SQLite — ignore errors (best-effort cache write).
        let _ = self
            .db
            .upsert_contract(&addr_str, contract.name.as_deref(), &abi_json)
            .await;

        filter_events(all_events, &contract.events, contract.address)
    }
}

/// Parse the cached ABI JSON string back into a list of `EventAbi` values.
fn parse_cached_events(address: Address, cached_json: &str) -> Result<Vec<EventAbi>, AbiError> {
    let entries: Vec<Value> = serde_json::from_str(cached_json)
        .map_err(|e| AbiError::ParseFailed(address, e.to_string()))?;

    let events = entries
        .iter()
        .filter_map(|entry| {
            let name = entry["name"].as_str()?.to_string();
            // Use serde_json::from_value so components are restored correctly —
            // EventInput derives Deserialize which handles nested `components`.
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

/// Serialize a single [`EventInput`] to a JSON value, including nested `components`.
///
/// Must be used when caching ABIs in SQLite. Omitting `components` would
/// corrupt the cache for any contract using tuple-typed parameters.
fn serialize_event_input(i: &EventInput) -> Value {
    serde_json::json!({
        "name": i.name,
        "type": i.ty,
        "indexed": i.indexed,
        // Recursively serialize components for tuple types.
        // Empty array for non-tuple types — serde_json::from_value::<EventInput>
        // will deserialize it back as an empty Vec via #[serde(default)].
        "components": i.components.iter().map(serialize_event_input).collect::<Vec<_>>(),
    })
}

/// Filter a list of `EventAbi` values to only the names requested in the config.
///
/// Returns [`AbiError::EventNotFound`] for any name in `wanted` that is not in the ABI.
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

/// Decodes raw Ethereum log data into named [`StoredEvent`] values.
///
/// Keyed by topic0 so logs can be dispatched to the correct [`EventAbi`] in O(1).
/// One `EventDecoder` is constructed per contract per sync run.
pub struct EventDecoder {
    /// Map from topic0 → EventAbi, for O(1) dispatch from any log.
    events: HashMap<B256, EventAbi>,
    contract: Address,
}

impl EventDecoder {
    /// Create a decoder for the given events and contract address.
    ///
    /// Pre-computes topic0 for each event so the hot loop in
    /// `extract_and_decode` only does map lookups.
    pub fn new(events: &[EventAbi], contract: Address) -> Result<Self, AbiError> {
        let map = events.iter().map(|e| (e.topic0(), e.clone())).collect();
        Ok(Self {
            events: map,
            contract,
        })
    }

    /// Scan all receipts from a block and extract logs that match our target.
    ///
    /// A log matches if:
    /// - (a) It was emitted by our target `contract` address, AND
    /// - (b) Its topic0 matches one of our watched events.
    ///
    /// Returns decoded [`StoredEvent`] values ready for SQLite insertion.
    pub fn extract_and_decode(
        &self,
        receipts: &[TransactionReceipt],
        block_num: u64,
        block_hash: B256,
    ) -> Vec<StoredEvent> {
        let mut results = Vec::new();

        for receipt in receipts {
            let tx_hash = receipt.transaction_hash;
            let tx_index = receipt.transaction_index.unwrap_or(0);

            for log in receipt.inner.logs() {
                // Skip logs from other contracts immediately.
                if log.address() != self.contract {
                    continue;
                }
                let topics = log.topics();
                let topic0 = match topics.first() {
                    Some(t) => *t,
                    None => continue, // Anonymous events have no topic0; we don't handle them.
                };

                // O(1) dispatch: look up the EventAbi by topic0.
                let event_abi = match self.events.get(&topic0) {
                    Some(e) => e,
                    None => continue, // This event is not in our watched list.
                };

                let log_index = log.log_index.unwrap_or(0);
                let raw_data = log.data().data.clone();
                let decoded = self.decode_log(event_abi, topics, raw_data.as_ref());

                results.push(StoredEvent {
                    contract: self.contract,
                    event_name: event_abi.name.clone(),
                    topic0,
                    block_number: block_num,
                    block_hash,
                    tx_hash,
                    tx_index,
                    log_index,
                    raw_topics: topics.to_vec(),
                    raw_data: Bytes::from(raw_data.0.to_vec()),
                    decoded,
                    source: "devp2p".to_string(),
                });
            }
        }

        results
    }

    /// Decode a single log's topics and data into a JSON object of named fields.
    ///
    /// Indexed params come from `topics[1..]`; non-indexed params are ABI-decoded
    /// from the `data` field. The result is a `serde_json::Value::Object` with
    /// one key per named parameter.
    fn decode_log(&self, abi: &EventAbi, topics: &[B256], data: &[u8]) -> Value {
        let mut obj = serde_json::Map::new();

        // Separate indexed from non-indexed inputs.
        let indexed_inputs: Vec<&EventInput> = abi.inputs.iter().filter(|i| i.indexed).collect();
        let non_indexed_inputs: Vec<&EventInput> = abi.inputs.iter().filter(|i| !i.indexed).collect();

        // Decode indexed params from topics[1..].
        // topics[0] is the event selector; indexed params start at topics[1].
        for (i, input) in indexed_inputs.iter().enumerate() {
            let topic_idx = i + 1;
            if topic_idx >= topics.len() {
                break;
            }
            let value = decode_indexed_param(&input.ty, &topics[topic_idx]);
            obj.insert(input.name.clone(), value);
        }

        // Decode non-indexed params from the data field using ABI decoding.
        // All non-indexed params are packed as a single ABI-encoded tuple in `data`.
        if !non_indexed_inputs.is_empty() && !data.is_empty() {
            let types: Vec<String> = non_indexed_inputs.iter().map(|i| i.ty.clone()).collect();
            match decode_abi_data(&types, data) {
                Ok(values) => {
                    for (input, value) in non_indexed_inputs.iter().zip(values.iter()) {
                        obj.insert(input.name.clone(), dyn_sol_value_to_json(value));
                    }
                }
                Err(e) => {
                    // Decode failed — store the error and raw data so the user can
                    // investigate. This should not happen with a correct ABI.
                    warn!(event = %abi.name, err = %e, "Failed to decode non-indexed params");
                    obj.insert(
                        "_decode_error".to_string(),
                        Value::String(e.to_string()),
                    );
                    obj.insert(
                        "_raw_data".to_string(),
                        Value::String(alloy_primitives::hex::encode(data)),
                    );
                }
            }
        }

        Value::Object(obj)
    }
}

/// Decode a single indexed parameter from a 32-byte topic.
///
/// For value types (address, bool, uint, int, bytesN), the topic IS the
/// ABI-encoded value padded to 32 bytes. For reference types (bytes, string,
/// arrays, tuples), the topic is keccak256 of the encoded value — the original
/// value is NOT recoverable, so we store the raw 32-byte hash.
fn decode_indexed_param(ty: &str, topic: &B256) -> Value {
    match ty {
        "address" => {
            // Ethereum addresses are 20 bytes, right-aligned in the 32-byte topic.
            // The first 12 bytes are zero padding.
            let addr_bytes = &topic.as_slice()[12..];
            Value::String(format!("0x{}", alloy_primitives::hex::encode(addr_bytes)))
        }
        "bool" => Value::Bool(topic[31] != 0),
        t if t.starts_with("uint") || t.starts_with("int") => {
            // Return as hex string to preserve precision for large values.
            // JavaScript Number loses precision above 2^53, which is well below
            // uint256's max of ~1.16 × 10^77. Clients should parse as BigInt.
            // Special-case 0: trim_start_matches('0') on all-zeros would give "",
            // producing "0x" which is invalid. Use "0x0" instead.
            let hex = alloy_primitives::hex::encode(topic.as_slice());
            let trimmed = hex.trim_start_matches('0');
            let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
            Value::String(format!("0x{}", trimmed))
        }
        t if t.starts_with("bytes") && t.len() > 5 => {
            // Fixed-size bytes (bytes1..bytes32) — stored left-aligned in the topic.
            // e.g. bytes4 stores its 4 bytes in topic[0..4], rest is zero padding.
            let size: usize = t[5..].parse().unwrap_or(32);
            let size = size.min(32);
            Value::String(format!(
                "0x{}",
                alloy_primitives::hex::encode(&topic.as_slice()[..size])
            ))
        }
        _ => {
            // For dynamic types (bytes, string, arrays, tuples), the topic is
            // keccak256(abi_encode(value)) — the original value is NOT recoverable.
            // We store the raw 32-byte hash so it can still be used for equality checks.
            Value::String(format!("0x{}", alloy_primitives::hex::encode(topic.as_slice())))
        }
    }
}

/// Decode the log's data field containing all non-indexed parameters using `alloy-dyn-abi`.
///
/// The parameters are ABI-encoded as a tuple (head + tail for dynamic types).
/// We reconstruct a `DynSolType::Tuple` from the type strings and decode in one pass.
fn decode_abi_data(types: &[String], data: &[u8]) -> Result<Vec<DynSolValue>, AbiError> {
    let sol_types: Result<Vec<DynSolType>, _> = types
        .iter()
        .map(|t| t.parse::<DynSolType>())
        .collect();

    let sol_types = sol_types.map_err(|e: alloy_dyn_abi::Error| AbiError::Decode(e.to_string()))?;
    let tuple_type = DynSolType::Tuple(sol_types);

    let decoded = tuple_type
        .abi_decode(data)
        .map_err(|e| AbiError::Decode(e.to_string()))?;

    match decoded {
        DynSolValue::Tuple(values) => Ok(values),
        other => Ok(vec![other]),
    }
}

/// Convert an alloy `DynSolValue` to a `serde_json::Value`.
///
/// Int and Uint are stored as **decimal strings** to preserve precision —
/// JavaScript `Number` loses precision above 2^53, which is well below the
/// range of `uint256` or `int256`. Clients should parse these as `BigInt`.
fn dyn_sol_value_to_json(value: &DynSolValue) -> Value {
    match value {
        DynSolValue::Bool(b) => Value::Bool(*b),
        // Decimal string to preserve full 256-bit precision.
        DynSolValue::Int(n, _) => Value::String(n.to_string()),
        // Decimal string to preserve full 256-bit precision.
        DynSolValue::Uint(n, _) => Value::String(n.to_string()),
        DynSolValue::FixedBytes(b, _) => {
            Value::String(format!("0x{}", alloy_primitives::hex::encode(b.as_slice())))
        }
        // EIP-55 checksummed address for readability and typo detection.
        DynSolValue::Address(a) => Value::String(a.to_checksum(None)),
        DynSolValue::Bytes(b) => {
            Value::String(format!("0x{}", alloy_primitives::hex::encode(b)))
        }
        DynSolValue::String(s) => Value::String(s.clone()),
        // Arrays and fixed arrays become JSON arrays, recursively.
        DynSolValue::Array(arr) | DynSolValue::FixedArray(arr) => {
            Value::Array(arr.iter().map(dyn_sol_value_to_json).collect())
        }
        // Tuples become JSON arrays (no field names available at the DynSolValue level).
        DynSolValue::Tuple(items) => {
            Value::Array(items.iter().map(dyn_sol_value_to_json).collect())
        }
        _ => Value::String("[unknown]".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_signature_and_topic0() {
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

        let expected_topic0 =
            keccak256(b"Transfer(address,address,uint256)");
        assert_eq!(event.topic0(), expected_topic0);
    }

    #[test]
    fn swap_event_topic0() {
        let event = EventAbi {
            name: "Swap".to_string(),
            inputs: vec![
                EventInput { name: "sender".to_string(), ty: "address".to_string(), indexed: true, components: vec![] },
                EventInput { name: "recipient".to_string(), ty: "address".to_string(), indexed: true, components: vec![] },
                EventInput { name: "amount0".to_string(), ty: "int256".to_string(), indexed: false, components: vec![] },
                EventInput { name: "amount1".to_string(), ty: "int256".to_string(), indexed: false, components: vec![] },
                EventInput { name: "sqrtPriceX96".to_string(), ty: "uint160".to_string(), indexed: false, components: vec![] },
                EventInput { name: "liquidity".to_string(), ty: "uint128".to_string(), indexed: false, components: vec![] },
                EventInput { name: "tick".to_string(), ty: "int24".to_string(), indexed: false, components: vec![] },
            ],
        };

        let sig = event.signature();
        assert_eq!(sig, "Swap(address,address,int256,int256,uint160,uint128,int24)");

        let expected = keccak256(b"Swap(address,address,int256,int256,uint160,uint128,int24)");
        assert_eq!(event.topic0(), expected);
    }
}
