//! Ethereum JSON-RPC server for scopenode.
//!
//! Serves a standard `eth_*` JSON-RPC interface on `localhost:<port>` so any
//! Ethereum library (viem, ethers.js, web3.py, alloy, cast) can query indexed
//! events without code changes.
//!
//! Only queries for **indexed contracts** are answered. Out-of-scope calls
//! return a clear error telling the user to run `scopenode status`.
//!
//! Implemented methods:
//! - `eth_getLogs` — filtered event log query from SQLite
//! - `eth_blockNumber` — highest indexed block number
//! - `eth_chainId` — always returns `0x1` (Ethereum mainnet)

#![deny(warnings)]

pub mod server;

use jsonrpsee::server::{Server, ServerHandle};
use scopenode_storage::Db;
use server::{EthApiImpl, EthApiServer};
use tracing::info;

/// Start the JSON-RPC server and bind it to `127.0.0.1:<port>`.
///
/// Returns a [`ServerHandle`] that keeps the server alive. Drop it or call
/// `.stop()` to shut it down. The server runs in the background on a tokio task.
pub async fn start_server(port: u16, db: Db) -> anyhow::Result<ServerHandle> {
    let server = Server::builder()
        .build(format!("127.0.0.1:{port}"))
        .await?;

    let addr = server.local_addr()?;
    let api = EthApiImpl::new(db);
    let handle = server.start(api.into_rpc());

    info!(addr = %addr, "JSON-RPC server started");
    Ok(handle)
}
