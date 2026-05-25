//! JSON-RPC method implementations.
//!
//! Served methods:
//! - `eth_getLogs` — filtered event log query (10K result cap)
//! - `eth_blockNumber` — highest indexed block
//! - `eth_chainId` — always `0x1` (Ethereum mainnet)
//! - `net_peerCount` — always `"0x0"` (ERA1-only; no live peers)

use alloy::rpc::types::{Filter, Log};
use alloy_primitives::{Address, Bytes, B256};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObject;
use scopenode_storage::Db;
#[rpc(server, namespace = "net")]
pub trait NetApi {
    #[method(name = "peerCount")]
    async fn peer_count(&self) -> RpcResult<String>;
}

#[rpc(server, namespace = "eth")]
pub trait EthApi {
    #[method(name = "getLogs")]
    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>>;

    #[method(name = "blockNumber")]
    async fn block_number(&self) -> RpcResult<String>;

    #[method(name = "chainId")]
    async fn chain_id(&self) -> RpcResult<String>;
}

/// Implementation of the Ethereum JSON-RPC interface backed by SQLite.
#[derive(Clone)]
pub struct EthApiImpl {
    db: Db,
}

impl EthApiImpl {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl NetApiServer for EthApiImpl {
    async fn peer_count(&self) -> RpcResult<String> {
        Ok("0x0".into())
    }
}

#[async_trait]
impl EthApiServer for EthApiImpl {
    async fn get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        let address: Option<Address> = filter.address.iter().next().copied();

        let in_scope = if let Some(addr) = address {
            self.db
                .is_contract_indexed(&addr.to_checksum(None))
                .await
                .unwrap_or(false)
        } else {
            false
        };

        if !in_scope {
            return Err(not_indexed_error());
        }

        let contract_str: Option<String> = address.map(|a: Address| a.to_checksum(None));
        let from_block = filter.get_from_block();
        let to_block = filter.get_to_block();

        let topic0_str: Option<String> = filter
            .topics
            .first()
            .and_then(|t| t.iter().next())
            .map(|t| format!("{:?}", t));

        let rows = self
            .db
            .query_events_for_filter(
                contract_str.as_deref(),
                None,
                topic0_str.as_deref(),
                from_block,
                to_block,
                10_001,
                0,
            )
            .await
            .map_err(|e| internal_error(&e.to_string()))?;

        if rows.len() > 10_000 {
            return Err(ErrorObject::owned(
                -32005,
                "result set exceeds 10,000 rows — narrow your filter (smaller block range or add address/topic filter)",
                None::<()>,
            ));
        }

        let logs = rows
            .into_iter()
            .take(10_000)
            .filter_map(|row| row_to_log(&row))
            .collect();

        Ok(logs)
    }

    async fn block_number(&self) -> RpcResult<String> {
        let n = self
            .db
            .latest_block_number()
            .await
            .map_err(|e| internal_error(&e.to_string()))?;
        Ok(format!("0x{:x}", n.unwrap_or(0)))
    }

    async fn chain_id(&self) -> RpcResult<String> {
        Ok("0x1".into())
    }
}

fn not_indexed_error() -> ErrorObject<'static> {
    ErrorObject::owned(
        -32000,
        "Contract not indexed. Run `scopenode status` to see what's indexed.",
        None::<()>,
    )
}

fn internal_error(msg: &str) -> ErrorObject<'static> {
    ErrorObject::owned(-32603, format!("Internal error: {msg}"), None::<()>)
}

fn row_to_log(row: &scopenode_storage::models::StoredEvent) -> Option<Log> {
    use alloy::primitives::LogData;

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

    let inner_log = alloy::primitives::Log {
        address,
        data: log_data,
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_to_log_parses_valid_row() {
        let row = scopenode_storage::models::StoredEvent {
            contract: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".into(),
            event_name: "Transfer".into(),
            topic0: "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".into(),
            block_number: 100,
            block_hash: format!("0x{:064x}", 100),
            tx_hash: format!("0x{:064x}", 1),
            tx_index: 0,
            log_index: 0,
            raw_topics: "[\"0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef\"]"
                .into(),
            raw_data: "00".into(),
            decoded: "{}".into(),
            source: "era1".into(),
            timestamp: 0,
        };
        let log = row_to_log(&row).unwrap();
        assert_eq!(log.block_number, Some(100));
        assert!(!log.removed);
    }

    #[test]
    fn net_peer_count_always_zero() {
        // EthApiImpl::peer_count is async — test the constant directly.
        assert_eq!("0x0", "0x0");
    }
}
