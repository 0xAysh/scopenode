//! Servers for scopenode: Ethereum JSON-RPC at `:8545` and REST API at `:8546`.
//!
//! **JSON-RPC** (`:8545`): serves `eth_getLogs`, `eth_blockNumber`, `eth_chainId`,
//! and `net_peerCount` (always `0x0`).
//!
//! **REST** (`:8546`): JSON HTTP API for apps that don't speak Ethereum JSON-RPC.

#![deny(warnings)]

pub mod filter_plan;
pub mod projection;
pub mod rest;
pub mod server;

pub use rest::start_rest_server;

use jsonrpsee::server::{Server, ServerHandle};
use scopenode_storage::Db;
use server::{EthApiImpl, EthApiServer, NetApiServer};
use tracing::info;

/// Start the JSON-RPC server and bind it to `127.0.0.1:<port>`.
///
/// Returns a [`ServerHandle`] that keeps the server alive. Drop it or call
/// `.stop()` to shut it down.
pub async fn start_server(db: Db, port: u16) -> anyhow::Result<ServerHandle> {
    let server = Server::builder().build(format!("127.0.0.1:{port}")).await?;

    let addr = server.local_addr()?;
    let api = EthApiImpl::new(db);
    let mut methods = EthApiServer::into_rpc(api.clone());
    methods
        .merge(NetApiServer::into_rpc(api))
        .expect("net/* merge failed");
    let handle = server.start(methods);

    info!(addr = %addr, "JSON-RPC server started");
    Ok(handle)
}
