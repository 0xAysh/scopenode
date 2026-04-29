//! Servers for scopenode: Ethereum JSON-RPC at `:8545` and REST API at `:8546`.
//!
//! **JSON-RPC** (`:8545`): serves `eth_getLogs`, `eth_blockNumber`, `eth_chainId`,
//! and `eth_subscribe` (WebSocket push for live events and block headers) so any
//! Ethereum library (viem, ethers.js, web3.py, alloy, cast) can query and subscribe
//! to indexed events without code changes.
//!
//! **REST** (`:8546`): JSON HTTP API + SSE stream for apps that don't speak
//! Ethereum JSON-RPC. Supports filtering by contract, event name, topic0, and
//! block range. The `/stream/events` endpoint pushes live events via SSE.
//!
//! Only queries for **indexed contracts** are answered. Out-of-scope calls
//! return a clear error telling the user to run `scopenode status`.

#![deny(warnings)]

pub mod rest;
pub mod server;

pub use rest::start_rest_server;

use alloy_primitives::B256;
use jsonrpsee::server::{Server, ServerHandle};
use scopenode_core::types::StoredEvent;
use scopenode_storage::Db;
use server::{EthApiImpl, EthApiServer};
use tokio::sync::broadcast;
use tracing::info;

/// Start the JSON-RPC server and bind it to `127.0.0.1:<port>`.
///
/// `broadcast` receives live indexed events — drives `eth_subscribe "logs"`.
/// `headers` receives block headers for every processed block — drives `eth_subscribe "newHeads"`.
///
/// Returns a [`ServerHandle`] that keeps the server alive. Drop it or call
/// `.stop()` to shut it down. The server runs in the background on a tokio task.
pub async fn start_server(
    port: u16,
    db: Db,
    broadcast: broadcast::Sender<StoredEvent>,
    headers: broadcast::Sender<(u64, B256, u64)>,
) -> anyhow::Result<ServerHandle> {
    let server = Server::builder()
        .build(format!("127.0.0.1:{port}"))
        .await?;

    let addr = server.local_addr()?;
    let api = EthApiImpl::new(db, broadcast, headers);
    let handle = server.start(api.into_rpc());

    info!(addr = %addr, "JSON-RPC server started (HTTP + WebSocket)");
    Ok(handle)
}
