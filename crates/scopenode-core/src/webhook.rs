//! Webhook delivery — HTTP push of live events to user-configured endpoints.

use crate::config::ContractConfig;
use crate::types::StoredEvent;
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use url::Url;

type HmacSha256 = Hmac<Sha256>;

/// Compute an HMAC-SHA256 signature over `body` using `secret`.
///
/// Returns a string in the format `sha256=<64 hex chars>`, suitable for
/// use in the `X-Scopenode-Signature` header.
pub fn sign_body(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(body);
    let result = mac.finalize().into_bytes();
    let hex: String = result.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

/// JSON body POSTed to a webhook endpoint for each live event.
#[derive(Debug, Serialize)]
pub struct WebhookPayload {
    pub event:        String,
    pub contract:     String,
    pub block_number: u64,
    pub block_hash:   String,
    pub tx_hash:      String,
    pub tx_index:     u64,
    pub log_index:    u64,
    pub timestamp:    u64,
    pub decoded:      serde_json::Value,
}

impl From<&StoredEvent> for WebhookPayload {
    fn from(e: &StoredEvent) -> Self {
        Self {
            event:        e.event_name.clone(),
            contract:     e.contract.to_checksum(None),
            block_number: e.block_number,
            block_hash:   e.block_hash.to_string(),
            tx_hash:      e.tx_hash.to_string(),
            tx_index:     e.tx_index,
            log_index:    e.log_index,
            timestamp:    e.timestamp,
            decoded:      e.decoded.clone(),
        }
    }
}

/// Return the webhook URL for a given event on a given contract.
///
/// Resolution order:
/// 1. `contract.webhook_events[event_name]` — per-event override
/// 2. `contract.webhook` — contract-level fallback
/// 3. `None` — no webhook configured
pub fn resolve_url<'a>(contract: &'a ContractConfig, event_name: &str) -> Option<&'a Url> {
    contract
        .webhook_events
        .get(event_name)
        .or(contract.webhook.as_ref())
}

use std::time::Duration;
use tracing::warn;

/// POST `body` to `url` with `headers`, retrying up to 3 times on failure.
///
/// Delays: 1s after attempt 1, 2s after attempt 2. Attempt 3 is the last.
/// Non-2xx responses count as failures. Failures after 3 attempts are logged
/// and dropped — live sync is never blocked.
pub async fn deliver_with_retry(
    client: &reqwest::Client,
    url: &Url,
    body: Vec<u8>,
    headers: reqwest::header::HeaderMap,
) {
    let delays = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)];

    for (attempt, delay) in delays.iter().enumerate() {
        match client
            .post(url.as_str())
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return,
            Ok(r) => warn!(
                status = %r.status(),
                attempt,
                url = %url,
                "Webhook non-2xx response"
            ),
            Err(e) => warn!(
                err = %e,
                attempt,
                url = %url,
                "Webhook request failed"
            ),
        }
        if attempt < 2 {
            tokio::time::sleep(*delay).await;
        }
    }
    warn!(url = %url, "Webhook giving up after 3 attempts");
}

use crate::config::Config;
use tokio::sync::broadcast;

/// Subscribes to the live-event broadcast channel and delivers events to
/// configured webhook URLs. Runs as a long-lived tokio task.
///
/// - Builds one shared `reqwest::Client` (connection pool reuse).
/// - Each delivery is spawned in its own task (never blocks the loop).
/// - Exits cleanly when the broadcast channel closes.
pub struct WebhookDispatcher {
    config: Config,
    rx:     broadcast::Receiver<StoredEvent>,
    client: reqwest::Client,
}

impl WebhookDispatcher {
    pub fn new(config: Config, rx: broadcast::Receiver<StoredEvent>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client creation should not fail");
        Self { config, rx, client }
    }

    /// Consume events from the broadcast channel until it closes.
    pub async fn run(mut self) {
        loop {
            match self.rx.recv().await {
                Ok(event) => self.dispatch(event).await,
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "Webhook dispatcher lagged — events dropped");
                }
            }
        }
    }

    async fn dispatch(&self, event: StoredEvent) {
        let event_addr = event.contract.to_checksum(None);

        for contract in &self.config.contracts {
            if contract.address.to_checksum(None) != event_addr {
                continue;
            }
            let url = match resolve_url(contract, &event.event_name) {
                Some(u) => u.clone(),
                None => continue,
            };

            let payload = WebhookPayload::from(&event);
            let body = match serde_json::to_vec(&payload) {
                Ok(b) => b,
                Err(e) => {
                    warn!(err = %e, "Failed to serialize webhook payload");
                    continue;
                }
            };

            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap());
            if let Ok(v) = event.event_name.parse() {
                headers.insert("x-scopenode-event", v);
            }
            if let Ok(v) = event_addr.parse() {
                headers.insert("x-scopenode-contract", v);
            }
            if let Some(secret) = &contract.webhook_secret {
                let sig = sign_body(secret, &body);
                if let Ok(v) = sig.parse() {
                    headers.insert("x-scopenode-signature", v);
                }
            }

            let client = self.client.clone();
            tokio::spawn(async move {
                deliver_with_retry(&client, &url, body, headers).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_body_produces_sha256_prefix() {
        let sig = sign_body("key", b"data");
        assert!(sig.starts_with("sha256="), "got: {sig}");
        assert_eq!(sig.len(), 7 + 64, "sha256= prefix + 64 hex chars");
    }

    #[test]
    fn sign_body_matches_independent_hmac() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let secret = "my-secret";
        let body = b"hello world";
        let sig = sign_body(secret, body);

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sig, format!("sha256={hex}"));
    }

    #[test]
    fn sign_body_different_secrets_differ() {
        let body = b"same body";
        assert_ne!(sign_body("secret1", body), sign_body("secret2", body));
    }

    mod routing {
        use super::super::*;
        use crate::config::ContractConfig;
        use url::Url;

        fn make_url(s: &str) -> Url { s.parse().unwrap() }

        fn make_contract(webhook: Option<&str>, overrides: &[(&str, &str)]) -> ContractConfig {
            ContractConfig {
                name: None,
                address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap(),
                events: vec!["Swap".into(), "Mint".into(), "Burn".into()],
                from_block: 0,
                to_block: None,
                abi_override: None,
                impl_address: None,
                webhook: webhook.map(make_url),
                webhook_secret: None,
                webhook_events: overrides
                    .iter()
                    .map(|(k, v)| (k.to_string(), make_url(v)))
                    .collect(),
            }
        }

        #[test]
        fn event_level_wins_over_contract_level() {
            let c = make_contract(
                Some("https://example.com/all"),
                &[("Swap", "https://example.com/swaps")],
            );
            assert_eq!(
                resolve_url(&c, "Swap").unwrap().as_str(),
                "https://example.com/swaps"
            );
        }

        #[test]
        fn falls_back_to_contract_level() {
            let c = make_contract(Some("https://example.com/all"), &[]);
            assert_eq!(
                resolve_url(&c, "Swap").unwrap().as_str(),
                "https://example.com/all"
            );
        }

        #[test]
        fn no_webhook_returns_none() {
            let c = make_contract(None, &[]);
            assert!(resolve_url(&c, "Swap").is_none());
        }

        #[test]
        fn unmatched_event_falls_back_to_contract_level() {
            let c = make_contract(
                Some("https://example.com/all"),
                &[("Swap", "https://example.com/swaps")],
            );
            assert_eq!(
                resolve_url(&c, "Burn").unwrap().as_str(),
                "https://example.com/all"
            );
        }

        #[test]
        fn no_contract_no_event_level_returns_none() {
            let c = make_contract(None, &[("Swap", "https://example.com/swaps")]);
            assert!(resolve_url(&c, "Swap").is_some());
            assert!(resolve_url(&c, "Mint").is_none());
        }
    }

    mod delivery {
        use super::super::*;
        use wiremock::{
            matchers::{header, method, path},
            Mock, MockServer, ResponseTemplate,
        };

        async fn make_server_and_url(response: u16, expected_calls: u64)
            -> (MockServer, url::Url)
        {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/hook"))
                .respond_with(ResponseTemplate::new(response))
                .expect(expected_calls)
                .mount(&server)
                .await;
            let url = format!("{}/hook", server.uri()).parse().unwrap();
            (server, url)
        }

        fn client() -> reqwest::Client {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap()
        }

        #[tokio::test]
        async fn success_on_first_attempt_sends_one_request() {
            let (_server, url) = make_server_and_url(200, 1).await;
            deliver_with_retry(&client(), &url, b"payload".to_vec(), Default::default()).await;
            // wiremock asserts exact call count on drop
        }

        #[tokio::test]
        async fn non_2xx_retries_three_times() {
            // Note: this test takes ~3s due to 1s + 2s backoff delays.
            let (_server, url) = make_server_and_url(500, 3).await;
            deliver_with_retry(&client(), &url, b"payload".to_vec(), Default::default()).await;
        }

        #[tokio::test]
        async fn success_on_second_attempt_sends_two_requests() {
            let server = MockServer::start().await;
            // First call → 500, second call → 200
            Mock::given(method("POST"))
                .and(path("/hook"))
                .respond_with(ResponseTemplate::new(500))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/hook"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            let url: url::Url = format!("{}/hook", server.uri()).parse().unwrap();
            deliver_with_retry(&client(), &url, b"payload".to_vec(), Default::default()).await;
        }

        #[tokio::test]
        async fn content_type_header_is_sent() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/hook"))
                .and(header("content-type", "application/json"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            let url: url::Url = format!("{}/hook", server.uri()).parse().unwrap();
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            deliver_with_retry(&client(), &url, b"{}".to_vec(), headers).await;
        }
    }

    mod dispatcher {
        use super::super::*;
        use crate::config::{Config, ContractConfig, NodeConfig};
        use crate::types::StoredEvent;
        use alloy_primitives::{Address, Bytes, B256};
        use serde_json::json;
        use tokio::sync::broadcast;
        use wiremock::{matchers::{header_regex, method, path}, Mock, MockServer, ResponseTemplate};

        fn make_event(contract: Address) -> StoredEvent {
            StoredEvent {
                contract,
                event_name:   "Swap".to_string(),
                topic0:       B256::ZERO,
                block_number: 100,
                block_hash:   B256::ZERO,
                tx_hash:      B256::ZERO,
                tx_index:     0,
                log_index:    0,
                raw_topics:   vec![],
                raw_data:     Bytes::default(),
                decoded:      json!({}),
                source:       "devp2p".to_string(),
                timestamp:    1_700_000_000,
            }
        }

        fn config_with_webhook(address: Address, webhook_url: &str) -> Config {
            Config {
                node: NodeConfig {
                    port: 18545,
                    data_dir: None,
                    consensus_rpc: vec![],
                    reorg_buffer: 64,
                },
                contracts: vec![ContractConfig {
                    name: None,
                    address,
                    events: vec!["Swap".into()],
                    from_block: 0,
                    to_block: None,
                    abi_override: None,
                    impl_address: None,
                    webhook: Some(webhook_url.parse().unwrap()),
                    webhook_secret: None,
                    webhook_events: std::collections::HashMap::new(),
                }],
            }
        }

        #[tokio::test]
        async fn event_matching_contract_triggers_post() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/hook"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            let address: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
            let url = format!("{}/hook", server.uri());
            let config = config_with_webhook(address, &url);

            let (tx, rx) = broadcast::channel(16);
            let dispatcher = WebhookDispatcher::new(config, rx);
            let handle = tokio::spawn(dispatcher.run());

            tx.send(make_event(address)).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(tx);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
            // wiremock asserts call count on drop
        }

        #[tokio::test]
        async fn event_for_different_contract_sends_no_post() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;

            let configured: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
            let other:      Address = "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse().unwrap();
            let url = format!("{}/hook", server.uri());
            let config = config_with_webhook(configured, &url);

            let (tx, rx) = broadcast::channel(16);
            let dispatcher = WebhookDispatcher::new(config, rx);
            let handle = tokio::spawn(dispatcher.run());

            tx.send(make_event(other)).unwrap(); // wrong contract
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(tx);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        }

        #[tokio::test]
        async fn dispatcher_exits_cleanly_when_channel_closes() {
            let config = Config {
                node: NodeConfig {
                    port: 18545,
                    data_dir: None,
                    consensus_rpc: vec![],
                    reorg_buffer: 64,
                },
                contracts: vec![],
            };
            let (tx, rx) = broadcast::channel::<StoredEvent>(16);
            let dispatcher = WebhookDispatcher::new(config, rx);
            drop(tx); // close immediately
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                dispatcher.run(),
            )
            .await
            .expect("dispatcher must exit when channel closes, not hang");
        }

        #[tokio::test]
        async fn signature_header_present_when_secret_set() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/hook"))
                .and(header_regex("x-scopenode-signature", r"^sha256=[0-9a-f]{64}$"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            let address: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
            let url = format!("{}/hook", server.uri());
            let mut config = config_with_webhook(address, &url);
            config.contracts[0].webhook_secret = Some("secret123".to_string());

            let (tx, rx) = broadcast::channel(16);
            let dispatcher = WebhookDispatcher::new(config, rx);
            let handle = tokio::spawn(dispatcher.run());

            tx.send(make_event(address)).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(tx);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        }
    }

    mod payload {
        use super::super::*;
        use crate::types::StoredEvent;
        use alloy_primitives::{Bytes, B256};
        use serde_json::json;

        fn make_event() -> StoredEvent {
            StoredEvent {
                contract:     "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap(),
                event_name:   "Transfer".to_string(),
                topic0:       B256::ZERO,
                block_number: 19_000_000,
                block_hash:   B256::ZERO,
                tx_hash:      B256::ZERO,
                tx_index:     5,
                log_index:    12,
                raw_topics:   vec![],
                raw_data:     Bytes::default(),
                decoded:      json!({"from": "0xaaa", "value": "1000"}),
                source:       "devp2p".to_string(),
                timestamp:    1_700_000_000,
            }
        }

        #[test]
        fn payload_serializes_all_fields() {
            let event = make_event();
            let payload = WebhookPayload::from(&event);
            let json = serde_json::to_value(&payload).unwrap();

            assert_eq!(json["event"],        "Transfer");
            assert_eq!(json["block_number"], 19_000_000u64);
            assert_eq!(json["tx_index"],     5u64);
            assert_eq!(json["log_index"],    12u64);
            assert_eq!(json["timestamp"],    1_700_000_000u64);
            assert_eq!(json["decoded"]["value"], "1000");
        }

        #[test]
        fn payload_contract_is_checksummed() {
            let event = make_event();
            let payload = WebhookPayload::from(&event);
            let json = serde_json::to_value(&payload).unwrap();
            let contract = json["contract"].as_str().unwrap();
            assert!(contract.starts_with("0x"));
            assert!(contract.chars().any(|c| c.is_uppercase()), "expected checksummed: {contract}");
        }
    }
}
