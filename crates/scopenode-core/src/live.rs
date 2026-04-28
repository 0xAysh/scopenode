//! Live sync — continuous tail of the chain tip after historical sync completes.
//!
//! [`LiveSyncer`] polls for new block headers every 6 seconds (half a slot).
//! For each new block it:
//! 1. Verifies the block header against the Helios beacon light client (when
//!    `consensus_rpc` is configured), then:
//! 2. Stores the header (needed to seed [`ReorgDetector`] across restarts)
//! 3. Runs the reorg detector — orphaned events are soft-deleted in SQLite
//! 4. Bloom-scans against every configured live contract
//! 5. For bloom hits: fetches receipts, Merkle-verifies, ABI-decodes, stores,
//!    and broadcasts each event on the [`tokio::sync::broadcast`] channel
//!
//! "Live contracts" are those with `to_block = None` in the config. Fixed-range
//! contracts are ignored here — they were fully handled by the historical pipeline.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Bloom, B256};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::abi::{AbiCache, EventAbi, EventDecoder};
use crate::beacon::BeaconStatusTx;
use crate::config::{Config, ContractConfig};
use crate::error::CoreError;
use crate::headers::BloomScanner;
use crate::helios_client::{HeliosGuard, VerifyDecision};
use crate::network::{EthNetwork, ReceiptFetchResult};
use crate::pipeline::{core_to_storage_event, scope_header_to_stored};
use crate::receipts::verify_receipts;
use crate::reorg::ReorgDetector;
use crate::types::{BloomTarget, ScopeHeader, StoredEvent};
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
    /// Fires for every processed block — used by `eth_subscribe "newHeads"`.
    /// Tuple is (block_number, block_hash, timestamp).
    headers_tx: broadcast::Sender<(u64, B256, u64)>,
    reorg_detector: ReorgDetector,
    beacon_status_tx: BeaconStatusTx,
    data_dir: Option<PathBuf>,
}

impl<N: EthNetwork + 'static> LiveSyncer<N> {
    pub fn new(
        config: Config,
        network: Arc<N>,
        db: Db,
        broadcast: broadcast::Sender<StoredEvent>,
        headers_tx: broadcast::Sender<(u64, B256, u64)>,
        beacon_status_tx: BeaconStatusTx,
        data_dir: Option<PathBuf>,
    ) -> Self {
        let capacity = config.node.reorg_buffer as usize;
        let abi_cache = AbiCache::new(db.clone());
        Self {
            config,
            network,
            db,
            abi_cache,
            broadcast,
            headers_tx,
            reorg_detector: ReorgDetector::new(capacity),
            beacon_status_tx,
            data_dir,
        }
    }

    /// Seed the reorg detector from stored headers, then poll for new blocks.
    ///
    /// Processes all live contracts (those with `to_block = None`) for every
    /// new block. In verified mode, each block header is checked against
    /// Helios before processing. Errors on individual blocks are logged and
    /// skipped — the loop continues to the next block rather than aborting.
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

        // R4 unverified warning.
        if self.config.node.consensus_rpc.is_empty() && !self.config.node.beacon_unverified_ack {
            warn!(
                "no consensus_rpc configured — live headers will be trusted from peers, \
                 not beacon-verified.\n\n      \
                 To enable trustless verification, add to config.toml:\n        \
                 consensus_rpc = [\"https://www.lightclientdata.org\"]\n        \
                 execution_rpc = \"https://<your-rpc-provider>\"\n\n      \
                 To acknowledge unverified mode and suppress this message:\n        \
                 beacon_unverified_ack = true"
            );
        }

        // Bootstrap the Helios beacon client (returns None in unverified mode).
        let reqwest_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        let mut helios: Option<HeliosGuard> = HeliosGuard::start(
            &self.config.node,
            &self.beacon_status_tx,
            self.data_dir.as_deref(),
            &reqwest_client,
        )
        .await?;

        let mut our_tip = tip;
        let mut unverified_block_count: u64 = 0;

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
                if let Some(ref mut guard) = helios {
                    // Verified mode: pre-fetch header to obtain peer_hash, then
                    // verify before processing.
                    let headers = match self.network.get_headers(block_num, block_num).await {
                        Ok(h) => h,
                        Err(e) => {
                            warn!(block = block_num, "Header fetch failed before verification: {e}");
                            break; // retry this block on the next poll tick
                        }
                    };
                    let header = match headers.into_iter().next() {
                        Some(h) => h,
                        None => {
                            warn!(block = block_num, "No header returned for block");
                            break;
                        }
                    };
                    let peer_hash = header.hash;
                    let current_status = self.beacon_status_tx.borrow().clone();

                    match guard
                        .verify_block(
                            block_num,
                            peer_hash,
                            &current_status,
                            &self.beacon_status_tx,
                            self.data_dir.as_deref(),
                        )
                        .await?
                    {
                        VerifyDecision::Accept => {
                            if let Err(e) =
                                self.process_block(block_num, Some(header), &contract_data).await
                            {
                                warn!(block = block_num, "Live block error: {e}");
                            }
                            our_tip = block_num;
                        }
                        VerifyDecision::Discard => break, // retry this block next tick
                        VerifyDecision::Halt => {
                            return Err(CoreError::Internal(
                                "helios verification halted live sync".to_string(),
                            ));
                        }
                    }
                } else {
                    // Unverified mode: process without beacon check.
                    if let Err(e) = self.process_block(block_num, None, &contract_data).await {
                        warn!(block = block_num, "Live block error: {e}");
                    }
                    our_tip = block_num;

                    unverified_block_count += 1;
                    if unverified_block_count % 100 == 0
                        && !self.config.node.beacon_unverified_ack
                    {
                        warn!(
                            "no consensus_rpc configured — live headers still trusted from peers \
                             (block {block_num}, {} blocks since last warning). \
                             Set beacon_unverified_ack = true to suppress.",
                            unverified_block_count
                        );
                    }
                }
            }
        }
    }

    /// Fetch, verify, decode, store, and broadcast all events for one live block.
    ///
    /// `prefetched_header`: when `Some`, the header was already fetched for
    /// beacon verification — reuse it to avoid a second devp2p round-trip.
    async fn process_block(
        &mut self,
        block_num: u64,
        prefetched_header: Option<ScopeHeader>,
        contracts: &[(ContractConfig, Vec<EventAbi>, Vec<BloomTarget>)],
    ) -> Result<(), CoreError> {
        let header = if let Some(h) = prefetched_header {
            h
        } else {
            let headers = self.network.get_headers(block_num, block_num).await?;
            headers.into_iter().next().ok_or(CoreError::Network(
                crate::error::NetworkError::HeadersFailed(block_num, block_num),
            ))?
        };

        self.db.insert_header(&scope_header_to_stored(&header)).await?;
        let _ = self.headers_tx.send((header.number, header.hash, header.timestamp));

        if let Some(reorg) = self
            .reorg_detector
            .advance(header.number, header.hash, header.parent_hash)
        {
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
                    if let Err(e) =
                        verify_receipts(&receipts, header.receipts_root, block_num)
                    {
                        warn!(block = block_num, err = %e, "Live receipt verification failed");
                        mark_retry_matching(
                            &self.db,
                            block_num,
                            &header.logs_bloom,
                            contracts,
                        )
                        .await;
                        return Ok(());
                    }

                    for (contract, events, targets) in contracts {
                        if !BloomScanner::matches(&header.logs_bloom, targets) {
                            continue;
                        }

                        let decoder = EventDecoder::new(events, contract.address)?;
                        let decoded = decoder.extract_and_decode(
                            &receipts,
                            block_num,
                            block_hash,
                            header.timestamp,
                        );
                        let storage: Vec<_> =
                            decoded.iter().map(core_to_storage_event).collect();

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
                    mark_retry_matching(
                        &self.db,
                        block_num,
                        &header.logs_bloom,
                        contracts,
                    )
                    .await;
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
    use crate::beacon::BeaconStatus;
    use crate::config::{Config, ContractConfig, NodeConfig};
    use crate::error::NetworkError;
    use crate::types::ScopeHeader;
    use alloy_primitives::{keccak256, Address, Bloom, Bytes, B256};
    use alloy_trie::EMPTY_ROOT_HASH;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use tokio::sync::watch;

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
                execution_rpc: None,
                beacon_unverified_ack: false,
                beacon_fallback_unverified: false,
                allow_http_consensus_rpc: false,
                consensus_checkpoint: None,
                beacon_sync_timeout_secs: 300,
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

    fn dummy_beacon_tx() -> BeaconStatusTx {
        let (tx, _rx) = watch::channel(BeaconStatus::NotConfigured);
        Arc::new(tx)
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
            timestamp: 0,
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
        let beacon_tx = dummy_beacon_tx();

        let (headers_tx, _) = broadcast::channel(32);
        let syncer = LiveSyncer::new(live_config(addr), network, db.clone(), tx, headers_tx, beacon_tx, None);

        // run() loops forever — interrupt after the first batch completes.
        let _ = tokio::time::timeout(Duration::from_secs(1), syncer.run()).await;

        let stored = db.get_headers(1, 5).await.unwrap();
        assert_eq!(stored.len(), 5);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }

    /// Header broadcast fires for every processed block, including bloom-miss blocks.
    #[tokio::test]
    async fn header_broadcast_fires_for_every_block() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let addr_str = addr.to_checksum(None);

        let db_path = unique_db_path();
        let db = scopenode_storage::Db::open(db_path.clone()).await.unwrap();
        db.upsert_contract(&addr_str, Some("test"), &transfer_abi_json())
            .await
            .unwrap();

        // All 5 headers have empty bloom — no events, so event broadcast never fires.
        let headers: Vec<ScopeHeader> = (1u64..=5).map(scope_header).collect();
        let network = Arc::new(MockNetwork { headers });

        let (tx, _rx) = broadcast::channel(32);
        let (headers_tx, mut headers_rx) = broadcast::channel::<(u64, B256, u64)>(32);
        let beacon_tx = dummy_beacon_tx();

        let syncer = LiveSyncer::new(live_config(addr), network, db.clone(), tx, headers_tx, beacon_tx, None);

        let _ = tokio::time::timeout(Duration::from_secs(1), syncer.run()).await;

        let mut received_blocks = Vec::new();
        while let Ok((block_num, _, _)) = headers_rx.try_recv() {
            received_blocks.push(block_num);
        }
        received_blocks.sort();
        assert_eq!(received_blocks, vec![1, 2, 3, 4, 5], "header broadcast must fire for all 5 blocks");

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }

    /// After historical sync completes the live syncer picks up from the stored
    /// tip and processes the next 10 blocks.
    #[tokio::test]
    async fn live_syncer_processes_ten_blocks_after_historical() {
        use crate::pipeline::scope_header_to_stored;

        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let addr_str = addr.to_checksum(None);

        let db_path = unique_db_path();
        let db = scopenode_storage::Db::open(db_path.clone()).await.unwrap();
        db.upsert_contract(&addr_str, Some("test"), &transfer_abi_json())
            .await
            .unwrap();

        // Simulate historical sync: insert headers 1–100 into the DB.
        for n in 1u64..=100 {
            let h = scope_header(n);
            db.insert_header(&scope_header_to_stored(&h)).await.unwrap();
        }

        // Network serves blocks 101–110 (10 new live blocks).
        let live_headers: Vec<ScopeHeader> = (101u64..=110).map(scope_header).collect();
        let network = Arc::new(MockNetwork { headers: live_headers });

        let (tx, _rx) = broadcast::channel(32);
        let beacon_tx = dummy_beacon_tx();

        let (headers_tx, _) = broadcast::channel(32);
        let syncer = LiveSyncer::new(live_config(addr), network, db.clone(), tx, headers_tx, beacon_tx, None);

        // run() loops forever — interrupt after the batch is processed.
        let _ = tokio::time::timeout(Duration::from_secs(2), syncer.run()).await;

        let stored = db.get_headers(101, 110).await.unwrap();
        assert_eq!(stored.len(), 10, "expected 10 live blocks stored after historical tip");

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
