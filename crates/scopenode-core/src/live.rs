//! Live sync — continuous tail of the chain tip after historical sync completes.
//!
//! [`LiveSyncer`] polls for new block headers every 6 seconds (half a slot).
//! For each new block it:
//! 1. Stores the header (needed to seed [`ReorgDetector`] across restarts)
//! 2. Runs the reorg detector — orphaned events are soft-deleted in SQLite
//! 3. Bloom-scans against every configured live contract
//! 4. For bloom hits: fetches receipts, Merkle-verifies, ABI-decodes, stores,
//!    and broadcasts each event on the [`tokio::sync::broadcast`] channel
//!
//! "Live contracts" are those with `to_block = None` in the config. Fixed-range
//! contracts are ignored here — they were fully handled by the historical pipeline.

use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Bloom, B256};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::abi::{AbiCache, EventAbi, EventDecoder};
use crate::config::{Config, ContractConfig};
use crate::error::CoreError;
use crate::headers::BloomScanner;
use crate::network::{EthNetwork, ReceiptFetchResult};
use crate::pipeline::{core_to_storage_event, scope_header_to_stored};
use crate::receipts::verify_receipts;
use crate::reorg::ReorgDetector;
use crate::types::{BloomTarget, StoredEvent};
use scopenode_storage::Db;

/// Continuous live-sync loop for contracts with `to_block = None`.
///
/// Call [`LiveSyncer::run`] after the historical pipeline finishes. It never
/// returns on success — wrap it with `tokio::select!` against a shutdown signal.
pub struct LiveSyncer<N: EthNetwork> {
    config: Config,
    network: Arc<N>,
    db: Db,
    abi_cache: AbiCache,
    broadcast: broadcast::Sender<StoredEvent>,
    reorg_detector: ReorgDetector,
}

impl<N: EthNetwork + 'static> LiveSyncer<N> {
    pub fn new(
        config: Config,
        network: Arc<N>,
        db: Db,
        broadcast: broadcast::Sender<StoredEvent>,
    ) -> Self {
        let capacity = config.node.reorg_buffer as usize;
        let abi_cache = AbiCache::new(db.clone());
        Self {
            config,
            network,
            db,
            abi_cache,
            broadcast,
            reorg_detector: ReorgDetector::new(capacity),
        }
    }

    /// Seed the reorg detector from stored headers, then poll for new blocks.
    ///
    /// Processes all live contracts (those with `to_block = None`) for every
    /// new block. Errors on individual blocks are logged and skipped — the loop
    /// continues to the next block rather than aborting.
    pub async fn run(mut self) -> Result<(), CoreError> {
        // Preload ABIs and build bloom targets once.
        let mut contract_data: Vec<(ContractConfig, Vec<EventAbi>, Vec<BloomTarget>)> = Vec::new();
        for contract in self.config.contracts.iter().filter(|c| c.to_block.is_none()) {
            let events = self.abi_cache.get_or_fetch(contract).await?;
            let topic0s: Vec<B256> = events.iter().map(|e| e.topic0()).collect();
            let targets = BloomScanner::build_targets(&topic0s, contract.address);
            contract_data.push((contract.clone(), events, targets));
        }

        if contract_data.is_empty() {
            return Ok(());
        }

        // Seed reorg detector from the most recent stored headers so a restart
        // can still detect reorgs that started before we came back up.
        let tip = self.db.latest_block_number().await?.unwrap_or(0);
        if tip > 0 {
            let seed_from = tip.saturating_sub(self.reorg_detector.capacity() as u64 - 1);
            let headers = self.db.get_headers(seed_from, tip).await?;
            self.reorg_detector.seed(
                headers.iter().filter_map(|h| {
                    match h.hash.parse::<B256>() {
                        Ok(hash) => Some((h.number as u64, hash)),
                        Err(e) => {
                            warn!(
                                block = h.number,
                                err = %e,
                                "Malformed header hash in DB — skipping for reorg seed"
                            );
                            None
                        }
                    }
                }),
            );
        }

        info!(tip, contracts = contract_data.len(), "Live sync started");

        let mut our_tip = tip;
        let mut interval = tokio::time::interval(Duration::from_secs(6));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let best = match self.network.best_block_number().await {
                Ok(n) => n,
                Err(e) => {
                    warn!("best_block_number failed: {e}");
                    continue;
                }
            };

            if best <= our_tip {
                continue;
            }

            for block_num in (our_tip + 1)..=best {
                if let Err(e) = self.process_block(block_num, &contract_data).await {
                    warn!(block = block_num, "Live block error: {e}");
                }
            }

            our_tip = best;
        }
    }

    /// Fetch, verify, decode, store, and broadcast all events for one live block.
    async fn process_block(
        &mut self,
        block_num: u64,
        contracts: &[(ContractConfig, Vec<EventAbi>, Vec<BloomTarget>)],
    ) -> Result<(), CoreError> {
        let headers = self.network.get_headers(block_num, block_num).await?;
        let header = headers.into_iter().next().ok_or(
            CoreError::Network(crate::error::NetworkError::HeadersFailed(block_num, block_num))
        )?;

        self.db.insert_header(&scope_header_to_stored(&header)).await?;

        if let Some(reorg) = self.reorg_detector.advance(header.number, header.hash, header.parent_hash) {
            let count = self.db.mark_reorged_by_hash(&reorg.orphaned_hashes).await?;
            warn!(
                depth = reorg.orphaned_hashes.len(),
                ancestor = reorg.common_ancestor,
                reorged_events = count,
                "Chain reorg detected"
            );
        }

        let any_match = contracts
            .iter()
            .any(|(_, _, targets)| BloomScanner::matches(&header.logs_bloom, targets));
        if !any_match {
            return Ok(());
        }

        let batch = [(header.number, header.hash, header.receipts_root)];
        let results: Vec<_> = self.network.get_receipts_for_blocks(&batch).await;

        for result in results {
            match result {
                ReceiptFetchResult::Ok { block_num, block_hash, receipts } => {
                    if let Err(e) = verify_receipts(&receipts, header.receipts_root, block_num) {
                        warn!(block = block_num, err = %e, "Live receipt verification failed");
                        mark_retry_matching(&self.db, block_num, &header.logs_bloom, contracts).await;
                        return Ok(());
                    }

                    for (contract, events, targets) in contracts {
                        if !BloomScanner::matches(&header.logs_bloom, targets) {
                            continue;
                        }

                        let decoder = EventDecoder::new(events, contract.address)?;
                        let decoded = decoder.extract_and_decode(&receipts, block_num, block_hash);
                        let storage: Vec<_> = decoded.iter().map(core_to_storage_event).collect();

                        if !storage.is_empty() {
                            info!(
                                block = block_num,
                                contract = %contract.address,
                                count = storage.len(),
                                "Live events"
                            );
                            self.db.insert_events(&storage).await?;
                        }

                        for event in decoded {
                            let _ = self.broadcast.send(event);
                        }
                    }
                }
                ReceiptFetchResult::Failed { block_num } => {
                    warn!(block = block_num, "Live receipt fetch failed");
                    mark_retry_matching(&self.db, block_num, &header.logs_bloom, contracts).await;
                }
            }
        }

        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ContractConfig, NodeConfig};
    use crate::error::NetworkError;
    use crate::types::ScopeHeader;
    use alloy_primitives::{keccak256, Address, Bloom, Bytes, B256};
    use alloy_trie::EMPTY_ROOT_HASH;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    // ── MockNetwork ───────────────────────────────────────────────────────────

    struct MockNetwork {
        headers: Vec<ScopeHeader>,
    }

    #[async_trait]
    impl EthNetwork for MockNetwork {
        async fn get_headers(&self, from: u64, to: u64) -> Result<Vec<ScopeHeader>, NetworkError> {
            Ok(self
                .headers
                .iter()
                .filter(|h| h.number >= from && h.number <= to)
                .cloned()
                .collect())
        }

        async fn get_receipts_for_blocks(
            &self,
            blocks: &[(u64, B256, B256)],
        ) -> Vec<ReceiptFetchResult> {
            blocks
                .iter()
                .map(|&(block_num, block_hash, _)| ReceiptFetchResult::Ok {
                    block_num,
                    block_hash,
                    receipts: vec![],
                })
                .collect()
        }

        async fn best_block_number(&self) -> Result<u64, NetworkError> {
            Ok(self.headers.iter().map(|h| h.number).max().unwrap_or(0))
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_db_path() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("scopenode_live_test_{}_{}.db", std::process::id(), n))
    }

    fn scope_header(number: u64) -> ScopeHeader {
        ScopeHeader {
            number,
            hash: keccak256(number.to_be_bytes()),
            parent_hash: keccak256((number.saturating_sub(1)).to_be_bytes()),
            timestamp: number * 12,
            receipts_root: EMPTY_ROOT_HASH,
            logs_bloom: Bloom::default(), // empty — no receipt fetch triggered
            gas_used: 0,
            base_fee_per_gas: None,
        }
    }

    fn live_config(address: Address) -> Config {
        Config {
            node: NodeConfig {
                port: 18545,
                data_dir: None,
                consensus_rpc: vec![],
                reorg_buffer: 64,
            },
            contracts: vec![ContractConfig {
                name: Some("test".into()),
                address,
                events: vec!["Transfer".to_string()],
                from_block: 1,
                to_block: None,
                abi_override: None,
                impl_address: None,
            }],
        }
    }

    fn transfer_abi_json() -> String {
        serde_json::json!([{
            "name": "Transfer",
            "inputs": [
                { "name": "from",  "type": "address", "indexed": true  },
                { "name": "to",    "type": "address", "indexed": true  },
                { "name": "value", "type": "uint256", "indexed": false }
            ]
        }])
        .to_string()
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// The broadcast channel fans a single event out to all active subscribers.
    #[tokio::test]
    async fn broadcast_fan_out_multiple_receivers() {
        let (tx, mut rx1) = broadcast::channel::<StoredEvent>(32);
        let mut rx2 = tx.subscribe();
        let mut rx3 = tx.subscribe();

        let event = StoredEvent {
            contract: Address::ZERO,
            event_name: "Transfer".to_string(),
            topic0: B256::ZERO,
            block_number: 42,
            block_hash: B256::ZERO,
            tx_hash: B256::ZERO,
            tx_index: 0,
            log_index: 0,
            raw_topics: vec![],
            raw_data: Bytes::default(),
            decoded: serde_json::json!({}),
            source: "devp2p".to_string(),
        };

        tx.send(event).unwrap();

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        let e3 = rx3.recv().await.unwrap();

        assert_eq!(e1.block_number, 42);
        assert_eq!(e1.event_name, "Transfer");
        assert_eq!(e2.block_number, e1.block_number);
        assert_eq!(e3.block_number, e1.block_number);
    }

    /// LiveSyncer stores a header row for every block it processes.
    #[tokio::test]
    async fn live_syncer_stores_headers_for_processed_blocks() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let addr_str = addr.to_checksum(None);

        let db_path = unique_db_path();
        let db = scopenode_storage::Db::open(db_path.clone()).await.unwrap();
        db.upsert_contract(&addr_str, Some("test"), &transfer_abi_json())
            .await
            .unwrap();

        let headers: Vec<ScopeHeader> = (1u64..=5).map(scope_header).collect();
        let network = Arc::new(MockNetwork { headers });
        let (tx, _rx) = broadcast::channel(32);

        let syncer = LiveSyncer::new(live_config(addr), network, db.clone(), tx);

        // run() loops forever — interrupt after the first batch completes.
        let _ = tokio::time::timeout(Duration::from_secs(1), syncer.run()).await;

        let stored = db.get_headers(1, 5).await.unwrap();
        assert_eq!(stored.len(), 5);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }
}

/// Call `mark_retry` for every contract whose bloom targets match `bloom`.
async fn mark_retry_matching(
    db: &scopenode_storage::Db,
    block_num: u64,
    bloom: &Bloom,
    contracts: &[(ContractConfig, Vec<EventAbi>, Vec<BloomTarget>)],
) {
    for (contract, _, targets) in contracts {
        if BloomScanner::matches(bloom, targets) {
            let _ = db.mark_retry(block_num, &contract.address.to_checksum(None)).await;
        }
    }
}
