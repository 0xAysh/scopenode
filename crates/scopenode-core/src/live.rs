//! Live sync — continuous tail of the chain tip after historical sync completes.
//!
//! [`LiveSyncer`] polls for new block headers every 6 seconds (half a slot).
//! For each new block it:
//! 1. Stores the header (needed to seed [`ReorgDetector`] across restarts)
//! 2. Runs the reorg detector — orphaned events are soft-deleted in SQLite
//! 3. Bloom-scans against every configured live contract
//! 4. For bloom hits: fetches receipts, Merkle-verifies, ABI-decodes, stores,
//!    and broadcasts each event on the channel returned by [`channel`]
//!
//! "Live contracts" are those with `to_block = None` in the config. Fixed-range
//! contracts are ignored here — they were fully handled by the historical pipeline.

use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::B256;
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

/// Create the broadcast channel for live events.
///
/// The `Sender` goes to [`LiveSyncer::new`]; the `Receiver` should be passed to
/// any consumers (REST SSE endpoint, webhooks). Additional receivers can be
/// created at any time via [`broadcast::Sender::subscribe`].
///
/// `buf` controls how many unread events are buffered per receiver before older
/// ones are dropped with [`broadcast::error::RecvError::Lagged`].
pub fn channel(buf: usize) -> (broadcast::Sender<StoredEvent>, broadcast::Receiver<StoredEvent>) {
    broadcast::channel(buf)
}

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
        let live: Vec<ContractConfig> = self.config.contracts.iter()
            .filter(|c| c.to_block.is_none())
            .cloned()
            .collect();

        if live.is_empty() {
            return Ok(());
        }

        // Preload ABIs and build bloom targets once — not on every block.
        let mut contract_data: Vec<(ContractConfig, Vec<EventAbi>, Vec<BloomTarget>)> = Vec::new();
        for contract in &live {
            let events = self.abi_cache.get_or_fetch(contract).await?;
            let topic0s: Vec<B256> = events.iter().map(|e| e.topic0()).collect();
            let targets = BloomScanner::build_targets(&topic0s, contract.address);
            contract_data.push((contract.clone(), events, targets));
        }

        // Seed reorg detector from the most recent stored headers so a restart
        // can still detect reorgs that started before we came back up.
        let tip = self.db.latest_block_number().await?.unwrap_or(0);
        if tip > 0 {
            let seed_from = tip.saturating_sub(self.reorg_detector.capacity() as u64 - 1);
            let headers = self.db.get_headers(seed_from, tip).await?;
            self.reorg_detector.seed(headers.iter().map(|h| {
                let hash: B256 = h.hash.parse().unwrap_or_default();
                (h.number as u64, hash)
            }));
        }

        info!(tip, contracts = live.len(), "Live sync started");

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
        let header = headers.into_iter().next().ok_or_else(|| {
            CoreError::Network(crate::error::NetworkError::HeadersFailed(block_num, block_num))
        })?;

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

        for (contract, events, targets) in contracts {
            if !BloomScanner::matches(&header.logs_bloom, targets) {
                continue;
            }

            let addr_str = contract.address.to_checksum(None);
            let batch = [(header.number, header.hash, header.receipts_root)];

            for result in self.network.get_receipts_for_blocks(&batch).await {
                match result {
                    ReceiptFetchResult::Ok { block_num, block_hash, receipts } => {
                        if let Err(e) = verify_receipts(&receipts, header.receipts_root, block_num) {
                            warn!(block = block_num, err = %e, "Live receipt verification failed");
                            let _ = self.db.mark_retry(block_num, &addr_str).await;
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
                    ReceiptFetchResult::Failed { block_num } => {
                        warn!(block = block_num, "Live receipt fetch failed");
                        let _ = self.db.mark_retry(block_num, &addr_str).await;
                    }
                }
            }
        }

        Ok(())
    }
}
