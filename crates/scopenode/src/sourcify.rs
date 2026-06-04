use alloy_primitives::Address;
use async_trait::async_trait;
use scopenode_core::{abi_resolution::AbiFetcher, error::AbiError};

pub struct SourcifyClient {
    client: reqwest::Client,
    base_url: String,
}

impl SourcifyClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: "https://repo.sourcify.dev".to_string(),
        }
    }

    #[cfg(test)]
    fn new_with_base_url(client: reqwest::Client, base_url: String) -> Self {
        Self { client, base_url }
    }

    async fn try_match(
        &self,
        match_type: &str,
        addr_str: &str,
        address: Address,
    ) -> Result<Option<String>, AbiError> {
        let url = format!(
            "{}/contracts/{}/1/{}/metadata.json",
            self.base_url, match_type, addr_str
        );

        let response = self.client.get(&url).send().await.map_err(|e| {
            AbiError::Cache(format!(
                "Sourcify request failed for {address}: {e}. Try again later or add abi_override to your config."
            ))
        })?;

        if response.status().as_u16() == 404 {
            return Ok(None);
        }

        let body = response
            .text()
            .await
            .map_err(|e| AbiError::Cache(e.to_string()))?;

        let metadata: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| AbiError::ParseFailed(address, e.to_string()))?;

        let abi = metadata
            .get("output")
            .and_then(|o| o.get("abi"))
            .ok_or_else(|| {
                AbiError::ParseFailed(
                    address,
                    "missing output.abi field in Sourcify response".to_string(),
                )
            })?;

        let abi_json = serde_json::to_string(abi)
            .map_err(|e| AbiError::ParseFailed(address, e.to_string()))?;

        Ok(Some(abi_json))
    }
}

#[async_trait]
impl AbiFetcher for SourcifyClient {
    async fn fetch(&self, address: Address) -> Result<String, AbiError> {
        let addr_str = address.to_checksum(None);

        if let Some(abi) = self.try_match("full_match", &addr_str, address).await? {
            return Ok(abi);
        }

        if let Some(abi) = self.try_match("partial_match", &addr_str, address).await? {
            return Ok(abi);
        }

        Err(AbiError::AbiRequired(address))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TEST_ADDR: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    fn abi_array() -> serde_json::Value {
        serde_json::json!([{
            "type": "event",
            "name": "Transfer",
            "inputs": [
                {"name": "from",  "type": "address", "indexed": true},
                {"name": "to",    "type": "address", "indexed": true},
                {"name": "value", "type": "uint256",  "indexed": false}
            ]
        }])
    }

    fn metadata_body(abi: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "output": { "abi": abi } })
    }

    fn test_client(timeout_ms: u64) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .build()
            .unwrap()
    }

    fn full_path(addr: &str) -> String {
        format!("/contracts/full_match/1/{}/metadata.json", addr)
    }

    fn partial_path(addr: &str) -> String {
        format!("/contracts/partial_match/1/{}/metadata.json", addr)
    }

    #[tokio::test]
    async fn full_match_success_returns_abi_json_string() {
        let server = MockServer::start().await;
        let addr_str = TEST_ADDR.to_checksum(None);

        Mock::given(method("GET"))
            .and(path(full_path(&addr_str)))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata_body(abi_array())))
            .mount(&server)
            .await;

        let fetcher = SourcifyClient::new_with_base_url(test_client(10_000), server.uri());
        let result = fetcher.fetch(TEST_ADDR).await.unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed[0]["name"], "Transfer");
    }

    #[tokio::test]
    async fn partial_match_fallback_when_full_match_404() {
        let server = MockServer::start().await;
        let addr_str = TEST_ADDR.to_checksum(None);

        Mock::given(method("GET"))
            .and(path(full_path(&addr_str)))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(partial_path(&addr_str)))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata_body(abi_array())))
            .mount(&server)
            .await;

        let fetcher = SourcifyClient::new_with_base_url(test_client(10_000), server.uri());
        let result = fetcher.fetch(TEST_ADDR).await.unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed[0]["name"], "Transfer");
    }

    #[tokio::test]
    async fn both_404_returns_actionable_error_mentioning_abi_override() {
        let server = MockServer::start().await;
        let addr_str = TEST_ADDR.to_checksum(None);

        Mock::given(method("GET"))
            .and(path(full_path(&addr_str)))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(partial_path(&addr_str)))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let fetcher = SourcifyClient::new_with_base_url(test_client(10_000), server.uri());
        let err = fetcher.fetch(TEST_ADDR).await.unwrap_err();

        assert!(
            err.to_string().contains("abi_override"),
            "double-404 error must mention abi_override, got: {err}"
        );
    }

    #[tokio::test]
    async fn timeout_returns_error_with_retry_or_abi_override_hint() {
        let server = MockServer::start().await;
        let addr_str = TEST_ADDR.to_checksum(None);

        Mock::given(method("GET"))
            .and(path(full_path(&addr_str)))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)))
            .mount(&server)
            .await;

        let fetcher = SourcifyClient::new_with_base_url(test_client(100), server.uri());
        let err = fetcher.fetch(TEST_ADDR).await.unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("retry") || msg.contains("abi_override"),
            "timeout error must hint at retry or abi_override, got: {msg}"
        );
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_failed() {
        let server = MockServer::start().await;
        let addr_str = TEST_ADDR.to_checksum(None);

        Mock::given(method("GET"))
            .and(path(full_path(&addr_str)))
            .respond_with(ResponseTemplate::new(200).set_body_string("not { valid json <<<"))
            .mount(&server)
            .await;

        let fetcher = SourcifyClient::new_with_base_url(test_client(10_000), server.uri());
        let err = fetcher.fetch(TEST_ADDR).await.unwrap_err();

        assert!(
            matches!(err, AbiError::ParseFailed(..)),
            "expected ParseFailed, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn missing_output_abi_field_returns_parse_failed() {
        let server = MockServer::start().await;
        let addr_str = TEST_ADDR.to_checksum(None);

        Mock::given(method("GET"))
            .and(path(full_path(&addr_str)))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"compiler": {"version": "0.8.0"}})),
            )
            .mount(&server)
            .await;

        let fetcher = SourcifyClient::new_with_base_url(test_client(10_000), server.uri());
        let err = fetcher.fetch(TEST_ADDR).await.unwrap_err();

        assert!(
            matches!(err, AbiError::ParseFailed(..)),
            "expected ParseFailed, got: {err:?}"
        );
    }
}
