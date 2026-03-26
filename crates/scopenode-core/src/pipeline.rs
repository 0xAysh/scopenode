//! The sync pipeline — orchestrates all stages for each configured contract.
//!
//! Stages run in order for each contract:
//! 1. **ABI fetch** — load event definitions from Sourcify or cache
//! 2. **Header sync** — fetch and store block headers in SQLite
//! 3. **Bloom scan** — check each header's bloom filter to identify candidate blocks
//! 4. **Receipt fetch + verify + decode + store** — fetch receipts for bloom candidates,
//!    verify via Merkle Patricia Trie, ABI-decode matching logs, insert into SQLite
//!
//! Progress is displayed via `indicatif` progress bars (hidden in `--quiet` mode).
//!
//! All stages are **resumable**: the `sync_cursor` table records how far each stage
//! has progressed per contract. If the process is interrupted (Ctrl+C, crash, OOM),
//! the next run picks up from where it left off. `INSERT OR IGNORE` ensures
//! already-stored data is never duplicated.

use crate::abi::{AbiCache, EventDecoder};
use crate::types::StoredEvent as CoreEvent;
use crate::config::{Config, ContractConfig};
use crate::error::CoreError;
use crate::headers::BloomScanner;
use crate::network::{EthNetwork, ReceiptFetchResult};
use crate::receipts::verify_receipts;
use scopenode_storage::SyncCursor;
use alloy_primitives::B256;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use scopenode_storage::Db;
use std::sync::Arc;
use tracing::{info, warn};

/// Orchestrates the full sync pipeline for all contracts in the config.
///
/// Generic over `N: EthNetwork` so the transport can be swapped (Phase 1: RPC,
/// Phase 2: devp2p) without changing any pipeline logic.
pub struct Pipeline<N: EthNetwork> {
    /// The loaded TOML config.
    config: Config,

    /// Data transport — [`crate::network::DevP2PNetwork`] (devp2p peers).
    /// Phase 2: extended with multi-peer agreement and ERA1 fallback.
    network: Arc<N>,

    /// SQLite database handle (Clone-safe, internally Arc-wrapped).
    db: Db,

    /// ABI cache backed by Sourcify and SQLite.
    abi_cache: AbiCache,
}

impl<N: EthNetwork + 'static> Pipeline<N> {
    /// Create a new pipeline.
    ///
    /// The [`AbiCache`] is constructed internally from the DB handle so the
    /// caller doesn't need to wire it up separately.
    pub fn new(config: Config, network: Arc<N>, db: Db) -> Self {
        let abi_cache = AbiCache::new(db.clone());
        Self {
            config,
            network,
            db,
            abi_cache,
        }
    }

    /// Run the full sync pipeline for all contracts in the config.
    ///
    /// Iterates over each contract and runs all four stages. If `dry_run` is true,
    /// only the bloom scan is run and an estimate is printed — no receipts are fetched.
    pub async fn run(&mut self, dry_run: bool, progress: &MultiProgress) -> Result<(), CoreError> {
        for contract in self.config.contracts.clone() {
            self.sync_contract(&contract, dry_run, progress).await?;
        }
        Ok(())
    }

    /// Re-run the receipt-fetch stage for all blocks marked `pending_retry = 1`.
    ///
    /// Skips header and bloom stages — only retries the fetch+verify+decode+store
    /// step for blocks that previously failed Merkle verification or network fetch.
    pub async fn run_retry(&mut self, progress: &MultiProgress) -> Result<(), CoreError> {
        let candidates = self.db.get_pending_retry_candidates().await?;
        if candidates.is_empty() {
            return Ok(());
        }

        for contract in self.config.contracts.clone() {
            let addr_str = contract.address.to_checksum(None);
            let blocks: Vec<(u64, B256, B256)> = candidates
                .iter()
                .filter(|(_, _, _, c)| *c == addr_str)
                .map(|&(n, h, r, _)| (n, h, r))
                .collect();
            if blocks.is_empty() {
                continue;
            }
            let events = self.abi_cache.get_or_fetch(&contract).await?;
            self.fetch_and_store(&contract, &blocks, &events, progress).await?;
        }

        Ok(())
    }

    /// Run all pipeline stages for a single contract.
    ///
    /// Stages in order:
    /// 1. ABI → event definitions + bloom targets
    /// 2. Header sync (resumable via `sync_cursor.headers_done_to`)
    /// 3. Bloom scan (resumable via `sync_cursor.bloom_done_to`)
    /// 4. Receipt fetch + verify + decode + store (if not dry_run)
    async fn sync_contract(
        &mut self,
        contract: &ContractConfig,
        dry_run: bool,
        progress: &MultiProgress,
    ) -> Result<(), CoreError> {
        let label = contract
            .name
            .as_deref()
            .unwrap_or("(unnamed)")
            .to_string();
        info!(contract = %contract.address, name = %label, "Syncing contract");

        // Stage 1: Fetch / cache ABI and build bloom targets.
        // Topic0 is derived from the ABI signature; bloom targets combine address + topic0.
        let events = self.abi_cache.get_or_fetch(contract).await?;
        info!(
            contract = %contract.address,
            events = ?events.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
            "ABI loaded"
        );

        let topic0s: Vec<B256> = events.iter().map(|e| e.topic0()).collect();
        let targets = BloomScanner::build_targets(&topic0s, contract.address);

        // Determine to_block: use configured value or query the chain tip.
        let to = match contract.to_block {
            Some(b) => b,
            None => self.network.best_block_number().await?,
        };
        let from = contract.from_block;

        // Load sync cursor to resume from last known position.
        let cursor = self
            .db
            .get_sync_cursor(&contract.address.to_checksum(None))
            .await
            .unwrap_or(None);

        // Stage 2: Header sync — resume from one block past the last stored header.
        let headers_start = cursor
            .as_ref()
            .and_then(|c| c.headers_done_to)
            .map(|n| n + 1)
            .unwrap_or(from);

        if headers_start <= to {
            self.sync_headers(contract, headers_start, to, progress)
                .await?;
        } else {
            info!(contract = %contract.address, "Headers already synced");
        }

        // Stage 3: Bloom scan — resume from one block past the last bloom-scanned block.
        let bloom_start = cursor
            .as_ref()
            .and_then(|c| c.bloom_done_to)
            .map(|n| n + 1)
            .unwrap_or(from);

        let candidates = self
            .bloom_scan(contract, &targets, bloom_start, to, progress)
            .await?;

        if dry_run {
            let total_blocks = to.saturating_sub(from) + 1;
            // When resuming, `candidates` only contains newly-scanned blocks.
            // Load the total candidate count from the DB so the report reflects
            // the full picture, not just the delta since the last run.
            let addr_str = contract.address.to_checksum(None);
            let total_candidates = self
                .db
                .count_bloom_candidates(&addr_str)
                .await
                .unwrap_or(candidates.len() as i64) as usize;
            let hit_rate = if total_blocks > 0 {
                total_candidates as f64 / total_blocks as f64 * 100.0
            } else {
                0.0
            };
            let headers_synced = headers_start > to;
            let bloom_synced = bloom_start > to;
            println!();
            println!("Dry run complete for {} ({})", label, contract.address);
            println!(
                "  Block range:     {} → {} ({} blocks)",
                from, to, total_blocks
            );
            if headers_synced {
                println!("  Headers:         already synced ✓");
            }
            if bloom_synced {
                println!("  Bloom scan:      already done ✓");
            }
            println!(
                "  Bloom matches:   {} blocks ({:.1}% hit rate)",
                total_candidates,
                hit_rate
            );
            println!("  Estimated time:  < 1 minute");
            println!();
            return Ok(());
        }

        // Stage 4: Fetch receipts, verify, decode, store.
        if !candidates.is_empty() {
            self.fetch_and_store(contract, &candidates, &events, progress)
                .await?;
        }

        info!(contract = %contract.address, "Sync complete");
        Ok(())
    }

    /// Fetch and store block headers for the range `[from, to]`.
    ///
    /// Headers are fetched in batches of 64 (matching the ETH wire protocol batch
    /// limit for Phase 2 `GetBlockHeaders` messages). Uses `INSERT OR IGNORE` so
    /// re-runs are safe. Updates the `sync_cursor` after each batch so progress
    /// survives interruption.
    async fn sync_headers(
        &self,
        contract: &ContractConfig,
        from: u64,
        to: u64,
        progress: &MultiProgress,
    ) -> Result<(), CoreError> {
        let total = to.saturating_sub(from) + 1;
        let pb = progress.add(ProgressBar::new(total));
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "Stage 1/3  Header sync      {bar:20.cyan/blue}  {pos:>5} / {len:<5}  {msg}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("█░"),
        );
        pb.set_message("syncing...");

        let addr_str = contract.address.to_checksum(None);

        // Fetch headers in batches of 64. This batch size matches the ETH wire
        // protocol's GetBlockHeaders limit, keeping Phase 2 compatibility.
        let mut chunk_start = from;
        while chunk_start <= to {
            let chunk_end = (chunk_start + 63).min(to);
            let headers = self.network.get_headers(chunk_start, chunk_end).await?;

            for header in &headers {
                // Convert from the core ScopeHeader (u64 fields, Bloom type) to the
                // storage StoredHeader (i64 fields, raw bytes). SQLite uses signed
                // integers, so block numbers > 2^63 would overflow — not a concern
                // for Ethereum which is at ~20M blocks as of 2024.
                let scope_header = scopenode_storage::models::StoredHeader {
                    number: header.number as i64,
                    hash: header.hash.to_string(),
                    parent_hash: header.parent_hash.to_string(),
                    timestamp: header.timestamp as i64,
                    receipts_root: header.receipts_root.to_string(),
                    logs_bloom: alloy_primitives::hex::encode(header.logs_bloom.as_slice()),
                    gas_used: header.gas_used as i64,
                    base_fee: header.base_fee_per_gas.map(|f| f as i64),
                };
                self.db.insert_header(&scope_header).await?;
                pb.inc(1);
            }

            // Update cursor after each batch so Ctrl+C doesn't lose a full chunk.
            let cursor = SyncCursor {
                contract: addr_str.clone(),
                from_block: contract.from_block,
                to_block: contract.to_block,
                headers_done_to: Some(chunk_end),
                bloom_done_to: None,
                receipts_done_to: None,
            };
            self.db.upsert_sync_cursor(&cursor).await?;

            chunk_start = chunk_end + 1;
        }

        pb.finish_with_message("done");
        Ok(())
    }

    /// Scan stored headers for blocks that might contain matching events.
    ///
    /// Loads all headers in `[from, to]` from SQLite, checks each bloom filter
    /// locally (pure CPU, no network). Returns `(block_num, block_hash, receipts_root)`
    /// tuples for blocks that passed the bloom check. Updates `sync_cursor` when done.
    async fn bloom_scan(
        &self,
        contract: &ContractConfig,
        targets: &[crate::types::BloomTarget],
        from: u64,
        to: u64,
        progress: &MultiProgress,
    ) -> Result<Vec<(u64, B256, B256)>, CoreError> {
        let headers = self.db.get_headers(from, to).await?;

        // Nothing new to scan — bloom was already done for this range on a prior run.
        if headers.is_empty() {
            return Ok(Vec::new());
        }

        let total = headers.len() as u64;
        let pb = progress.add(ProgressBar::new(total));
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "Stage 2/3  Bloom scan       {bar:20.cyan/blue}  {pos:>5} / {len:<5}  {msg}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("█░"),
        );
        pb.set_message("scanning...");

        let mut candidates = Vec::new();
        let addr_str = contract.address.to_checksum(None);

        for h in &headers {
            let bloom = h.logs_bloom;
            if BloomScanner::matches(&bloom, targets) {
                // This block's bloom says it might contain our events.
                // Record it as a bloom candidate for the receipt fetch stage.
                let hash: B256 = h.hash.parse().unwrap_or_default();
                let receipts_root: B256 = h.receipts_root.parse().unwrap_or_default();
                candidates.push((h.number as u64, hash, receipts_root));
                let _ = self
                    .db
                    .insert_bloom_candidate(h.number as u64, hash, &addr_str)
                    .await;
            }
            pb.inc(1);
        }

        let hits = candidates.len();
        pb.finish_with_message(format!("done ({hits} hits)"));

        // Mark bloom scan complete. Pass None for headers_done_to so the upsert
        // CASE WHEN logic in SQLite preserves the existing headers_done_to value
        // (which may be ahead of the bloom scan range).
        let cursor = SyncCursor {
            contract: addr_str.clone(),
            from_block: contract.from_block,
            to_block: contract.to_block,
            headers_done_to: None,
            bloom_done_to: Some(to),
            receipts_done_to: None,
        };
        let _ = self.db.upsert_sync_cursor(&cursor).await;

        Ok(candidates)
    }

    /// Fetch receipts for bloom-candidate blocks, verify, decode, and store events.
    ///
    /// For each bloom candidate block:
    /// 1. **Fetch** — request receipts from the network (batched, up to 16 per request)
    /// 2. **Merkle verify** — rebuild receipt trie, check root == `receipts_root` from header
    /// 3. **ABI decode** — extract matching logs and decode into named JSON fields
    /// 4. **Store** — `INSERT OR IGNORE` into events table (idempotent)
    /// 5. **Mark fetched** — update `bloom_candidates.fetched = 1`
    /// 6. **Update cursor** — advance `receipts_done_to`
    async fn fetch_and_store(
        &mut self,
        contract: &ContractConfig,
        candidates: &[(u64, B256, B256)],
        events: &[crate::abi::EventAbi],
        progress: &MultiProgress,
    ) -> Result<(), CoreError> {
        let decoder = EventDecoder::new(events, contract.address)?;
        let addr_str = contract.address.to_checksum(None);

        // Filter already-fetched blocks for resumability: if we crashed mid-batch,
        // the next run picks up from blocks where fetched=0, skipping completed ones.
        let mut remaining = Vec::new();
        for &(block_num, block_hash, receipts_root) in candidates {
            if !self.db.is_fetched(block_num, &addr_str).await? {
                remaining.push((block_num, block_hash, receipts_root));
            }
        }

        let total = remaining.len() as u64;
        let pb = progress.add(ProgressBar::new(total));
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "Stage 3/3  Receipt fetch    {bar:20.cyan/blue}  {pos:>5} / {len:<5}  {msg}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("█░"),
        );
        pb.set_message("fetching...");

        let mut total_events = 0usize;

        // Batch up to 16 blocks per network request. This matches the ETH wire
        // protocol's GetReceipts message batch limit, keeping Phase 2 compatibility.
        for chunk in remaining.chunks(16) {
            let results = self.network.get_receipts_for_blocks(chunk).await;

            for result in results {
                match result {
                    ReceiptFetchResult::Ok {
                        block_num,
                        block_hash,
                        receipts,
                    } => {
                        // Find the expected receipts_root for this block from our chunk.
                        let receipts_root = chunk
                            .iter()
                            .find(|&&(n, _, _)| n == block_num)
                            .map(|&(_, _, r)| r)
                            .unwrap_or_default();

                        // Merkle verify: rebuild the receipt trie and check the root.
                        // If this fails, the peer sent tampered data — mark for retry
                        // so `scopenode retry` can re-attempt with a different peer.
                        if let Err(e) =
                            verify_receipts(&receipts, receipts_root, block_num)
                        {
                            warn!(
                                block = block_num,
                                err = %e,
                                "Receipt Merkle verification failed — marking for retry"
                            );
                            let _ = self.db.mark_retry(block_num, &addr_str).await;
                            pb.inc(1);
                            continue;
                        }

                        // ABI decode: scan all receipt logs for our contract + events.
                        let stored_events =
                            decoder.extract_and_decode(&receipts, block_num, block_hash);

                        let storage_events: Vec<scopenode_storage::models::StoredEvent> =
                            stored_events.iter().map(core_to_storage_event).collect();

                        total_events += storage_events.len();
                        self.db.insert_events(&storage_events).await?;
                        self.db.mark_fetched(block_num, &addr_str).await?;
                        pb.inc(1);
                    }
                    ReceiptFetchResult::Failed { block_num } => {
                        warn!(
                            block = block_num,
                            "All fetch attempts failed — marking pending_retry"
                        );
                        let _ = self.db.mark_retry(block_num, &addr_str).await;
                        pb.inc(1);
                    }
                }
            }
        }

        pb.finish_with_message(format!("done ({total_events} events found)"));

        // Update cursor to mark receipt stage complete.
        let to = contract.to_block.unwrap_or(
            remaining
                .last()
                .map(|&(n, _, _)| n)
                .unwrap_or(contract.from_block),
        );
        let cursor = SyncCursor {
            contract: addr_str.clone(),
            from_block: contract.from_block,
            to_block: contract.to_block,
            headers_done_to: Some(to),
            bloom_done_to: Some(to),
            receipts_done_to: Some(to),
        };
        let _ = self.db.upsert_sync_cursor(&cursor).await;

        info!(
            contract = %contract.address,
            events = total_events,
            "Receipts processed"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ContractConfig, NodeConfig};
    use crate::error::NetworkError;
    use crate::types::ScopeHeader;
    use alloy_primitives::{keccak256, Address, Bloom, BloomInput, B256};
    use alloy_trie::EMPTY_ROOT_HASH;
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ── MockNetwork ───────────────────────────────────────────────────────────

    /// Deterministic mock implementing EthNetwork.
    ///
    /// Returns pre-configured headers. For receipt fetches, returns `Failed`
    /// for any block number in `fail_blocks` and `Ok { receipts: vec![] }`
    /// (empty block, passes MPT against EMPTY_ROOT_HASH) for all others.
    struct MockNetwork {
        headers: Vec<ScopeHeader>,
        fail_blocks: HashSet<u64>,
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
                .map(|&(block_num, block_hash, _)| {
                    if self.fail_blocks.contains(&block_num) {
                        ReceiptFetchResult::Failed { block_num }
                    } else {
                        ReceiptFetchResult::Ok { block_num, block_hash, receipts: vec![] }
                    }
                })
                .collect()
        }

        async fn best_block_number(&self) -> Result<u64, NetworkError> {
            Ok(self.headers.iter().map(|h| h.number).max().unwrap_or(0))
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn unique_db_path() -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir()
            .join(format!("scopenode_test_{}_{}.db", std::process::id(), n))
    }

    /// Build a bloom that will match for the given address + topic0.
    fn bloom_for(address: Address, topic0: B256) -> Bloom {
        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(address.as_slice()));
        bloom.accrue(BloomInput::Raw(topic0.as_slice()));
        bloom
    }

    fn transfer_topic0() -> B256 {
        keccak256(b"Transfer(address,address,uint256)")
    }

    fn abi_json_for(event_name: &str, topic0_sig: &str) -> String {
        // Use the canonical sig to build inputs from the signature string.
        // For Transfer: (address,address,uint256) = 3 params.
        let inputs = match topic0_sig {
            "Transfer(address,address,uint256)" => serde_json::json!([
                { "name": "from",  "type": "address", "indexed": true,  "components": [] },
                { "name": "to",    "type": "address", "indexed": true,  "components": [] },
                { "name": "value", "type": "uint256", "indexed": false, "components": [] }
            ]),
            _ => serde_json::json!([]),
        };
        serde_json::json!([{ "name": event_name, "inputs": inputs }]).to_string()
    }

    fn scope_header(number: u64, bloom: Bloom) -> ScopeHeader {
        ScopeHeader {
            number,
            hash: keccak256(number.to_be_bytes()),
            parent_hash: keccak256((number - 1).to_be_bytes()),
            timestamp: number * 12,
            receipts_root: EMPTY_ROOT_HASH,
            logs_bloom: bloom,
            gas_used: 0,
            base_fee_per_gas: None,
        }
    }

    fn test_config(address: Address, event: &str, from: u64, to: u64) -> Config {
        Config {
            node: NodeConfig { port: 18545, data_dir: None, consensus_rpc: vec![], reorg_buffer: 64 },
            contracts: vec![ContractConfig {
                name: Some("test".into()),
                address,
                events: vec![event.to_string()],
                from_block: from,
                to_block: Some(to),
                abi_override: None,
                impl_address: None,
            }],
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Failed receipt blocks are marked `pending_retry = 1` and the pipeline
    /// continues without aborting. Successful empty blocks are marked `fetched = 1`.
    #[tokio::test]
    async fn failed_blocks_marked_pending_retry_pipeline_continues() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let topic0 = transfer_topic0();
        let addr_str = addr.to_checksum(None);
        let bloom = bloom_for(addr, topic0);

        let db_path = unique_db_path();
        let db = scopenode_storage::Db::open(db_path.clone()).await.unwrap();

        // Pre-cache the ABI so the pipeline skips Sourcify.
        db.upsert_contract(
            &addr_str,
            Some("test"),
            &abi_json_for("Transfer", "Transfer(address,address,uint256)"),
        )
        .await
        .unwrap();

        // Build headers: blocks 100 and 101, both with a matching bloom and EMPTY_ROOT.
        let h100 = scope_header(100, bloom);
        let h101 = scope_header(101, bloom);

        // Block 101 fails; block 100 succeeds with empty receipts.
        let network = Arc::new(MockNetwork {
            headers: vec![h100.clone(), h101.clone()],
            fail_blocks: [101u64].into_iter().collect(),
        });

        let config = test_config(addr, "Transfer", 100, 101);
        let mut pipeline = Pipeline::new(config, network, db.clone());
        pipeline.run(false, &indicatif::MultiProgress::new()).await.unwrap();

        // Block 100: should be fetched.
        let fetched_100 = db.is_fetched(100, &addr_str).await.unwrap();
        assert!(fetched_100, "block 100 should be fetched");

        // Block 101: should be pending_retry.
        let retry_101 = db.is_pending_retry(101, &addr_str).await.unwrap();
        assert!(retry_101, "block 101 should be marked pending_retry");

        // No events stored (both blocks are empty).
        let event_count = db.count_events_for_contract(&addr_str).await.unwrap();
        assert_eq!(event_count, 0);

        let _ = std::fs::remove_file(&db_path);
        // WAL mode creates two extra files — clean those up too.
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }
}

/// Convert a core [`StoredEvent`] (alloy types, u64) to a storage [`StoredEvent`] (strings, i64).
///
/// SQLite stores hashes as `0x`-prefixed hex strings and block numbers as i64.
/// Topics are stored as a JSON array of hex strings. Raw data is stored as a hex string.
/// U256 values in `decoded` are already stringified by the ABI decoder (JS precision safe).
fn core_to_storage_event(e: &CoreEvent) -> scopenode_storage::models::StoredEvent {
    scopenode_storage::models::StoredEvent {
        contract: e.contract.to_checksum(None),
        event_name: e.event_name.clone(),
        topic0: e.topic0.to_string(),
        block_number: e.block_number as i64,
        block_hash: e.block_hash.to_string(),
        tx_hash: e.tx_hash.to_string(),
        tx_index: e.tx_index as i64,
        log_index: e.log_index as i64,
        // JSON array of 0x-prefixed hex strings, e.g. ["0xddf252...", "0x0000...sender"]
        raw_topics: serde_json::to_string(
            &e.raw_topics
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default(),
        // Hex-encoded bytes (no 0x prefix) for compact SQLite TEXT storage.
        raw_data: alloy_primitives::hex::encode(&e.raw_data),
        decoded: e.decoded.to_string(),
        source: e.source.clone(),
    }
}
