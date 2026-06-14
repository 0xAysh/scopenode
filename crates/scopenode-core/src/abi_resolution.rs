//! ABI resolution module.
//!
//! This module owns cache/local-file/remote ABI resolution. Event decoding
//! stays in `abi` and receives already-resolved event ABI definitions.

use crate::abi::{EventAbi, EventInput};
use crate::config::ContractConfig;
use crate::error::AbiError;
use alloy_primitives::Address;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

/// Seam between ABI resolution and its storage adapter.
#[async_trait]
pub trait AbiStore: Send + Sync {
    async fn load(&self, address: &str) -> Result<Option<String>, AbiError>;
    async fn save(&self, address: &str, name: Option<&str>, abi_json: &str)
        -> Result<(), AbiError>;
}

/// Seam between the network-free core library and a remote ABI fetch adapter.
#[async_trait]
pub trait AbiFetcher: Send + Sync {
    async fn fetch(&self, address: Address) -> Result<String, AbiError>;
}

pub struct AbiResolver {
    store: Arc<dyn AbiStore>,
    fetcher: Option<Arc<dyn AbiFetcher>>,
}

impl AbiResolver {
    pub fn new(store: Arc<dyn AbiStore>, fetcher: Option<Arc<dyn AbiFetcher>>) -> Self {
        Self { store, fetcher }
    }

    pub async fn resolve_events(
        &self,
        contract: &ContractConfig,
    ) -> Result<Vec<EventAbi>, AbiError> {
        let addr_str = contract.address.to_checksum(None);

        if let Ok(Some(cached)) = self.store.load(&addr_str).await {
            let abi_array = parse_abi_json(contract.address, &cached)?;
            let all_events = parse_event_entries(&abi_array);
            return filter_events(all_events, &contract.events, contract.address);
        }

        if let Some(override_path) = &contract.abi_override {
            match load_abi_override(override_path, contract.address) {
                Ok(all_events) => {
                    let json = serialize_events_to_json(&all_events);
                    if let Err(e) = self
                        .store
                        .save(&addr_str, contract.name.as_deref(), &json)
                        .await
                    {
                        warn!(address = %contract.address, error = %e, "failed to write ABI to cache");
                    }
                    return filter_events(all_events, &contract.events, contract.address);
                }
                Err(_) => {
                    warn!(address = %contract.address, "abi_override file invalid, falling back to remote ABI fetch");
                }
            }
        }

        let fetch_addr = contract.impl_address.unwrap_or(contract.address);
        if let Some(fetcher) = &self.fetcher {
            if contract.abi_override.is_none() {
                warn!(address = %fetch_addr, "no abi_override set, falling back to remote ABI fetch");
            }
            if let Ok(raw_json) = fetcher.fetch(fetch_addr).await {
                let abi_array = parse_abi_json(contract.address, &raw_json)?;
                let all_events = parse_event_entries(&abi_array);
                let normalized = serialize_events_to_json(&all_events);
                if let Err(e) = self
                    .store
                    .save(&addr_str, contract.name.as_deref(), &normalized)
                    .await
                {
                    warn!(address = %contract.address, error = %e, "failed to write ABI to cache");
                }
                return filter_events(all_events, &contract.events, contract.address);
            }
        }

        Err(AbiError::AbiRequired(contract.address))
    }
}

/// Read an ABI from a local JSON file configured as `abi_override`.
pub fn load_abi_override(
    path: &std::path::Path,
    address: Address,
) -> Result<Vec<EventAbi>, AbiError> {
    let content = std::fs::read_to_string(path)?;
    let abi_array = parse_abi_json(address, &content)?;
    Ok(parse_event_entries(&abi_array))
}

/// Parse a JSON string into an ABI entry array, regardless of whether it
/// originated from an override file, a Sourcify response, or the ABI cache.
fn parse_abi_json(address: Address, json: &str) -> Result<Vec<Value>, AbiError> {
    serde_json::from_str(json).map_err(|e| AbiError::ParseFailed(address, e.to_string()))
}

/// Extract event definitions from ABI entries. Used for ABI override files,
/// Sourcify responses, and the round-tripped ABI cache alike — all three
/// represent events as `{"type": "event", "name": ..., "inputs": [...]}`.
fn parse_event_entries(abi_array: &[Value]) -> Vec<EventAbi> {
    abi_array
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
        .collect()
}

fn serialize_event_input(i: &EventInput) -> Value {
    serde_json::json!({
        "name": i.name,
        "type": i.ty,
        "indexed": i.indexed,
        "components": i.components.iter().map(serialize_event_input).collect::<Vec<_>>(),
    })
}

fn serialize_events_to_json(events: &[EventAbi]) -> String {
    serde_json::to_string(
        &events
            .iter()
            .map(|e| {
                serde_json::json!({
                    "type": "event",
                    "name": e.name,
                    "inputs": e.inputs.iter().map(serialize_event_input).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default()
}

fn filter_events(
    all_events: Vec<EventAbi>,
    wanted: &[String],
    address: Address,
) -> Result<Vec<EventAbi>, AbiError> {
    if wanted == ["*"] {
        if all_events.is_empty() {
            warn!(%address, "wildcard events requested but ABI has no event entries");
        }
        return Ok(all_events);
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContractConfig;
    use alloy_primitives::{address, Address};
    use std::io::Write;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    struct StubAbiStore {
        data: Mutex<HashMap<String, String>>,
    }

    impl StubAbiStore {
        fn empty() -> Arc<Self> {
            Arc::new(Self {
                data: Mutex::new(HashMap::new()),
            })
        }

        fn with_entry(address: &str, json: &str) -> Arc<Self> {
            let mut data = HashMap::new();
            data.insert(address.to_owned(), json.to_owned());
            Arc::new(Self {
                data: Mutex::new(data),
            })
        }

        fn get(&self, address: &str) -> Option<String> {
            self.data.lock().unwrap().get(address).cloned()
        }
    }

    #[async_trait]
    impl AbiStore for StubAbiStore {
        async fn load(&self, address: &str) -> Result<Option<String>, AbiError> {
            Ok(self.data.lock().unwrap().get(address).cloned())
        }

        async fn save(
            &self,
            address: &str,
            _name: Option<&str>,
            abi_json: &str,
        ) -> Result<(), AbiError> {
            self.data
                .lock()
                .unwrap()
                .insert(address.to_owned(), abi_json.to_owned());
            Ok(())
        }
    }

    struct StubAbiFetcher {
        fetched_address: Mutex<Option<Address>>,
        result: Result<String, String>,
    }

    impl StubAbiFetcher {
        fn ok(json: &str) -> Arc<Self> {
            Arc::new(Self {
                fetched_address: Mutex::new(None),
                result: Ok(json.to_owned()),
            })
        }

        fn err_result(msg: &str) -> Arc<Self> {
            Arc::new(Self {
                fetched_address: Mutex::new(None),
                result: Err(msg.to_owned()),
            })
        }

        fn last_address(&self) -> Option<Address> {
            *self.fetched_address.lock().unwrap()
        }
    }

    #[async_trait]
    impl AbiFetcher for StubAbiFetcher {
        async fn fetch(&self, address: Address) -> Result<String, AbiError> {
            *self.fetched_address.lock().unwrap() = Some(address);
            self.result.clone().map_err(AbiError::Cache)
        }
    }

    struct PanicFetcher;

    #[async_trait]
    impl AbiFetcher for PanicFetcher {
        async fn fetch(&self, _: Address) -> Result<String, AbiError> {
            panic!("fetcher must not be called");
        }
    }

    const ADDR: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const IMPL: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

    fn transfer_raw_abi() -> &'static str {
        r#"[{"type":"event","name":"Transfer","inputs":[
            {"name":"from","type":"address","indexed":true,"components":[]},
            {"name":"to","type":"address","indexed":true,"components":[]},
            {"name":"value","type":"uint256","indexed":false,"components":[]}
        ]}]"#
    }

    fn cached_transfer_json() -> String {
        serde_json::json!([{
            "type": "event",
            "name": "Transfer",
            "inputs": [
                {"name": "from", "type": "address", "indexed": true, "components": []},
                {"name": "to",   "type": "address", "indexed": true, "components": []},
                {"name": "value","type": "uint256",  "indexed": false,"components": []}
            ]
        }])
        .to_string()
    }

    fn contract_cfg(events: Vec<&str>) -> ContractConfig {
        ContractConfig {
            name: None,
            address: ADDR,
            events: events.into_iter().map(String::from).collect(),
            from_block: 1,
            to_block: Some(100),
            abi_override: None,
            impl_address: None,
        }
    }

    fn resolver(store: Arc<dyn AbiStore>, fetcher: Option<Arc<dyn AbiFetcher>>) -> AbiResolver {
        AbiResolver::new(store, fetcher)
    }

    #[tokio::test]
    async fn cache_hit_returns_events_without_calling_fetcher() {
        let store = StubAbiStore::with_entry(&ADDR.to_checksum(None), &cached_transfer_json());
        let resolver = resolver(store, Some(Arc::new(PanicFetcher)));

        let events = resolver
            .resolve_events(&contract_cfg(vec!["Transfer"]))
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "Transfer");
    }

    #[tokio::test]
    async fn valid_abi_override_loads_caches_and_does_not_call_fetcher() {
        let store = StubAbiStore::empty();
        let resolver = resolver(
            Arc::clone(&store) as Arc<dyn AbiStore>,
            Some(Arc::new(PanicFetcher)),
        );
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(transfer_raw_abi().as_bytes()).unwrap();

        let mut cfg = contract_cfg(vec!["Transfer"]);
        cfg.abi_override = Some(tmp.path().to_path_buf());

        let events = resolver.resolve_events(&cfg).await.unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "Transfer");
        assert!(
            store.get(&ADDR.to_checksum(None)).is_some(),
            "ABI must be cached after file load"
        );
    }

    #[tokio::test]
    async fn invalid_abi_override_calls_fetcher_and_caches_result() {
        let store = StubAbiStore::empty();
        let fetcher = StubAbiFetcher::ok(transfer_raw_abi());
        let resolver = resolver(
            Arc::clone(&store) as Arc<dyn AbiStore>,
            Some(Arc::clone(&fetcher) as Arc<dyn AbiFetcher>),
        );
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"not valid json at all").unwrap();

        let mut cfg = contract_cfg(vec!["Transfer"]);
        cfg.abi_override = Some(tmp.path().to_path_buf());

        let events = resolver.resolve_events(&cfg).await.unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "Transfer");
        assert_eq!(fetcher.last_address(), Some(ADDR));
        assert!(
            store.get(&ADDR.to_checksum(None)).is_some(),
            "ABI must be cached after fetcher call"
        );
    }

    #[tokio::test]
    async fn all_sources_fail_returns_abi_required() {
        let fetcher = StubAbiFetcher::err_result("network down");
        let resolver = resolver(StubAbiStore::empty(), Some(fetcher));

        let err = resolver
            .resolve_events(&contract_cfg(vec!["Transfer"]))
            .await
            .unwrap_err();

        assert!(matches!(err, AbiError::AbiRequired(_)));
    }

    #[tokio::test]
    async fn wildcard_events_returns_all_abi_events() {
        let resolver = resolver(StubAbiStore::empty(), None);
        let mut tmp = NamedTempFile::new().unwrap();
        let abi = r#"[
            {"type":"event","name":"Transfer","inputs":[]},
            {"type":"event","name":"Approval","inputs":[]}
        ]"#;
        tmp.write_all(abi.as_bytes()).unwrap();

        let mut cfg = contract_cfg(vec!["*"]);
        cfg.abi_override = Some(tmp.path().to_path_buf());

        let events = resolver.resolve_events(&cfg).await.unwrap();
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"Transfer"));
        assert!(names.contains(&"Approval"));
    }

    #[tokio::test]
    async fn wildcard_on_no_events_abi_returns_empty_vec() {
        let resolver = resolver(StubAbiStore::empty(), None);
        let mut tmp = NamedTempFile::new().unwrap();
        let abi = r#"[{"type":"function","name":"transfer","inputs":[]}]"#;
        tmp.write_all(abi.as_bytes()).unwrap();

        let mut cfg = contract_cfg(vec!["*"]);
        cfg.abi_override = Some(tmp.path().to_path_buf());

        let events = resolver.resolve_events(&cfg).await.unwrap();

        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn missing_requested_event_returns_event_not_found() {
        let resolver = resolver(StubAbiStore::empty(), None);
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(transfer_raw_abi().as_bytes()).unwrap();

        let mut cfg = contract_cfg(vec!["Transfer", "Mint"]);
        cfg.abi_override = Some(tmp.path().to_path_buf());

        let err = resolver.resolve_events(&cfg).await.unwrap_err();

        assert!(matches!(err, AbiError::EventNotFound(ref name, _) if name == "Mint"));
    }

    #[test]
    fn cache_round_trip_preserves_events() {
        let events = vec![EventAbi {
            name: "Transfer".to_string(),
            inputs: vec![
                EventInput {
                    name: "from".to_string(),
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
        }];

        let json = serialize_events_to_json(&events);
        let abi_array = parse_abi_json(ADDR, &json).unwrap();
        let parsed = parse_event_entries(&abi_array);

        assert_eq!(parsed, events);
    }

    #[test]
    fn parse_event_entries_filters_non_events_and_malformed() {
        let abi_array: Vec<Value> = serde_json::from_str(
            r#"[
                {"type":"function","name":"transfer","inputs":[]},
                {"type":"event","name":"Transfer","inputs":[
                    {"name":"from","type":"address","indexed":true,"components":[]}
                ]},
                {"type":"event","inputs":[]}
            ]"#,
        )
        .unwrap();

        let events = parse_event_entries(&abi_array);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "Transfer");
    }

    #[tokio::test]
    async fn impl_address_is_passed_to_fetcher_instead_of_contract_address() {
        let fetcher = StubAbiFetcher::ok(transfer_raw_abi());
        let resolver = resolver(
            StubAbiStore::empty(),
            Some(Arc::clone(&fetcher) as Arc<dyn AbiFetcher>),
        );
        let mut cfg = contract_cfg(vec!["Transfer"]);
        cfg.impl_address = Some(IMPL);

        let _ = resolver.resolve_events(&cfg).await;

        assert_eq!(fetcher.last_address(), Some(IMPL));
    }
}
