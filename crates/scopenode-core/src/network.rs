//! Network transport abstraction for fetching Ethereum headers and receipts.
//!
//! **Phase 1** uses [`RpcNetwork`] backed by an alloy HTTP provider (a public RPC
//! endpoint like `eth.llamarpc.com`). This is the simplest possible transport
//! and works out of the box with no configuration.
//!
//! **Phase 2** will replace this with `DevP2PNetwork` using `reth-network` +
//! `reth-eth-wire` to connect directly to mainnet full nodes via the ETH wire
//! protocol (`GetBlockHeaders`, `GetReceipts`). This removes the trusted third
//! party (RPC endpoint) from the data path — headers and receipts are fetched
//! peer-to-peer and verified cryptographically.
//!
//! The [`EthNetwork`] trait ensures the pipeline code stays unchanged across
//! both phases — only this module needs to change when the transport is swapped.

use crate::error::NetworkError;
use crate::types::ScopeHeader;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::transports::http::reqwest::Url;
use alloy_primitives::B256;
use async_trait::async_trait;
use std::sync::Arc;
use tracing::{info, warn};

/// Result of a receipt fetch attempt for a single block.
///
/// The `Ok` variant includes the block's hash so it can be passed directly to
/// [`crate::receipts::verify_receipts`] and [`crate::abi::EventDecoder`].
/// The `Failed` variant records the block number so the pipeline can mark it
/// for retry without losing track of which block failed.
pub enum ReceiptFetchResult {
    /// Receipts were retrieved successfully for this block.
    Ok {
        block_num: u64,
        block_hash: B256,
        receipts: Vec<alloy::rpc::types::TransactionReceipt>,
    },
    /// All fetch attempts failed for this block. Will be retried later.
    Failed {
        block_num: u64,
    },
}

/// Abstraction over the Ethereum data transport.
///
/// Implement this trait to provide a different data source without modifying
/// the pipeline. The pipeline is generic over `N: EthNetwork` so the compiler
/// monomorphises it for the concrete transport, avoiding runtime dispatch overhead.
///
/// Implementations must be `Send + Sync` for use across async task boundaries.
#[async_trait]
pub trait EthNetwork: Send + Sync {
    /// Fetch block headers for the inclusive range `[from, to]`.
    ///
    /// Headers contain the bloom filter (`logs_bloom`) and receipts root
    /// (`receipts_root`) needed by later pipeline stages. The returned vec
    /// may be shorter than `to - from + 1` if some blocks are missing
    /// (e.g. uncles in Phase 2).
    async fn get_headers(&self, from: u64, to: u64) -> Result<Vec<ScopeHeader>, NetworkError>;

    /// Fetch receipts for a batch of blocks.
    ///
    /// Each element of `blocks` is `(block_num, block_hash, receipts_root)`.
    /// Returns one [`ReceiptFetchResult`] per input block — `Ok` if receipts
    /// were retrieved, `Failed` if all attempts failed.
    ///
    /// Merkle verification is NOT done here — that happens in the pipeline
    /// via [`crate::receipts::verify_receipts`] using the `receipts_root`
    /// passed through from the header fetch stage.
    async fn get_receipts_for_blocks(
        &self,
        blocks: &[(u64, B256, B256)], // (block_num, block_hash, receipts_root)
    ) -> Vec<ReceiptFetchResult>;

    /// Return the current canonical chain tip block number.
    ///
    /// Used when `to_block` is omitted from the config (live sync mode).
    /// The pipeline uses this as the upper bound for the initial sync, then
    /// polls for new blocks in Phase 3a.
    async fn best_block_number(&self) -> Result<u64, NetworkError>;
}

/// Phase 1 transport: wraps an alloy HTTP provider.
///
/// All data retrieved through this transport is tagged `source = "devp2p"` in
/// the database, even though it's actually coming from an RPC endpoint. This
/// keeps the schema consistent when Phase 2 introduces real devp2p connections.
///
/// **TODO Phase 2:** Replace with `DevP2PNetwork` using `reth-network` +
/// `reth-eth-wire`. The trait abstraction here ensures the pipeline won't need
/// to change — only this struct is swapped out.
pub struct RpcNetwork {
    provider: Arc<dyn Provider>,
}

impl RpcNetwork {
    /// Construct an `RpcNetwork` from an explicit RPC URL string.
    pub fn new(rpc_url: &str) -> Result<Self, NetworkError> {
        let url: Url = rpc_url
            .parse()
            .map_err(|e: url::ParseError| NetworkError::Rpc(e.to_string()))?;
        let provider = ProviderBuilder::new().connect_http(url);
        Ok(Self {
            provider: Arc::new(provider),
        })
    }

    /// Construct from the `SCOPENODE_RPC_URL` environment variable, or fall back
    /// to `https://eth.llamarpc.com` (a free public endpoint).
    ///
    /// Set `SCOPENODE_RPC_URL` to use a different endpoint during development,
    /// e.g. your own node, Alchemy, or Infura. A private endpoint will be faster
    /// and have higher rate limits than the public fallback.
    pub fn from_env() -> Result<Self, NetworkError> {
        let url = std::env::var("SCOPENODE_RPC_URL")
            .unwrap_or_else(|_| "https://eth.llamarpc.com".to_string());
        info!(rpc_url = %url, "Using RPC endpoint (Phase 1 transport)");
        Self::new(&url)
    }
}

#[async_trait]
impl EthNetwork for RpcNetwork {
    async fn get_headers(&self, from: u64, to: u64) -> Result<Vec<ScopeHeader>, NetworkError> {
        use alloy::eips::BlockNumberOrTag;

        let mut headers = Vec::new();

        // Fetch blocks one at a time. Phase 2 will replace this with a single
        // GetBlockHeaders devp2p message that returns up to 1024 headers in one round trip.
        for block_num in from..=to {
            let block = self
                .provider
                .get_block_by_number(BlockNumberOrTag::Number(block_num))
                .await
                .map_err(|e| NetworkError::Rpc(e.to_string()))?;

            match block {
                Some(b) => {
                    // Extract the fields we need from the full RPC block into our
                    // minimal ScopeHeader representation. We discard transaction bodies,
                    // uncle hashes, extra data, etc. — we only need bloom + receipts_root.
                    let h = &b.header;
                    let scope_header = ScopeHeader {
                        number: h.number,
                        hash: h.hash,
                        parent_hash: h.parent_hash,
                        timestamp: h.timestamp,
                        receipts_root: h.receipts_root,
                        logs_bloom: h.logs_bloom,
                        gas_used: h.gas_used,
                        base_fee_per_gas: h.base_fee_per_gas.map(|f| f as u128),
                    };
                    headers.push(scope_header);
                }
                None => {
                    warn!(block_num, "Block not found on RPC");
                }
            }
        }
        if headers.is_empty() {
            return Err(NetworkError::HeadersFailed(from, to));
        }
        Ok(headers)
    }

    async fn get_receipts_for_blocks(
        &self,
        blocks: &[(u64, B256, B256)],
    ) -> Vec<ReceiptFetchResult> {
        let mut results = Vec::new();

        for &(block_num, block_hash, _receipts_root) in blocks {
            // Note: _receipts_root is ignored here — it is passed through by the
            // pipeline to verify_receipts() after this function returns. We don't
            // verify inside the network layer; that responsibility belongs to the pipeline.
            match self
                .provider
                .get_block_receipts(alloy::eips::BlockId::Number(alloy::eips::BlockNumberOrTag::Number(block_num)))
                .await
            {
                Ok(Some(receipts)) => {
                    results.push(ReceiptFetchResult::Ok {
                        block_num,
                        block_hash,
                        receipts,
                    });
                }
                Ok(None) => {
                    warn!(block_num, "No receipts returned for block");
                    results.push(ReceiptFetchResult::Failed { block_num });
                }
                Err(e) => {
                    warn!(block_num, err = %e, "Failed to fetch receipts");
                    results.push(ReceiptFetchResult::Failed { block_num });
                }
            }
        }
        results
    }

    async fn best_block_number(&self) -> Result<u64, NetworkError> {
        self.provider
            .get_block_number()
            .await
            .map_err(|e| NetworkError::Rpc(e.to_string()))
    }
}
