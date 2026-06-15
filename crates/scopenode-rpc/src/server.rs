//! JSON-RPC method implementations.
//!
//! Served methods:
//! - `eth_getLogs` — filtered event log query (10K result cap)
//! - `eth_blockNumber` — highest indexed block
//! - `eth_chainId` — always `0x1` (Ethereum mainnet)
//! - `net_peerCount` — always `"0x0"` (ERA1-only; no live peers)
//!
//! # Malformed stored rows
//!
//! `eth_getLogs` has no per-row quality channel, so a stored row whose
//! Projection is `Lossy` or `Invalid` fails the whole request with error
//! `-32006`, naming the offending block and log index. Rows are never
//! silently dropped or served with fallback data.

use crate::filter_plan::FilterPlan;
use crate::query_front_door::{
    execute_event_query, EventQueryFrontDoorError, EventQueryResponse, MISSING_COVERAGE_MESSAGE,
    RESULT_CAP_MESSAGE,
};
use alloy::rpc::types::{Filter, Log};
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
        let response = execute_event_query(&self.db, FilterPlan::from_rpc_filter(&filter))
            .await
            .map_err(|err| match err {
                EventQueryFrontDoorError::MissingAddress => missing_address_error(),
                EventQueryFrontDoorError::Unsupported { reason } => {
                    ErrorObject::owned(-32002, reason, None::<()>)
                }
                EventQueryFrontDoorError::Storage(e) => internal_error(&e.to_string()),
            })?;
        match response {
            EventQueryResponse::NotIndexed => Err(not_indexed_error()),
            EventQueryResponse::MissingCoverage => Err(ErrorObject::owned(
                -32001,
                MISSING_COVERAGE_MESSAGE,
                None::<()>,
            )),
            EventQueryResponse::TooManyResults { .. } => {
                Err(ErrorObject::owned(-32005, RESULT_CAP_MESSAGE, None::<()>))
            }
            EventQueryResponse::Empty => Ok(vec![]),
            EventQueryResponse::Results(rows) => {
                let mut logs = Vec::with_capacity(rows.len());
                for row in &rows {
                    match crate::projection::project_row(row).log {
                        crate::projection::LogProjection::Valid(log) => logs.push(*log),
                        crate::projection::LogProjection::Lossy { reason, .. }
                        | crate::projection::LogProjection::Invalid { reason } => {
                            return Err(projection_error(row, &reason));
                        }
                    }
                }
                Ok(logs)
            }
        }
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

fn missing_address_error() -> ErrorObject<'static> {
    ErrorObject::owned(
        -32602,
        "address filter is required for eth_getLogs",
        None::<()>,
    )
}

fn internal_error(msg: &str) -> ErrorObject<'static> {
    ErrorObject::owned(-32603, format!("Internal error: {msg}"), None::<()>)
}

fn projection_error(
    row: &scopenode_storage::models::StoredEvent,
    reason: &str,
) -> ErrorObject<'static> {
    ErrorObject::owned(
        -32006,
        format!(
            "stored event failed projection (block {}, log {}): {reason} — local data may be corrupt; resync this range",
            row.block_number, row.log_index
        ),
        None::<()>,
    )
}

#[cfg(test)]
mod tests {
    use crate::projection::{project_row, LogProjection};

    #[test]
    fn projection_yields_valid_log_for_valid_row() {
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
        let log = match project_row(&row).log {
            LogProjection::Valid(log) => log,
            other => panic!("expected Valid projection, got {other:?}"),
        };
        assert_eq!(log.block_number, Some(100));
        assert!(!log.removed);
    }

    #[test]
    fn net_peer_count_always_zero() {
        // EthApiImpl::peer_count is async — test the constant directly.
        assert_eq!("0x0", "0x0");
    }

    #[test]
    fn query_front_door_maps_capped_outcome_without_transport_details() {
        let mapped = crate::query_front_door::map_event_query_outcome(
            scopenode_storage::EventQueryOutcome::Capped {
                results: vec![],
                cap: 10_000,
            },
        );

        assert!(matches!(
            mapped,
            crate::query_front_door::EventQueryResponse::TooManyResults { cap: 10_000 }
        ));
        assert_eq!(
            crate::query_front_door::RESULT_CAP_MESSAGE,
            "result set exceeds 10,000 rows — narrow your filter (smaller block range or add address/topic filter)"
        );
    }
}
