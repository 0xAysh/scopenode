//! ERA1-backed scope indexing pipeline.
//!
//! Replaces the devp2p pipeline for historical data. Single sequential pass
//! per ERA1 file: bloom-check each header, decode receipts for hits, verify
//! the receipt Merkle root, ABI-decode matching logs, and store events.

use crate::abi::{AbiCache, EventDecoder};
use crate::config::ContractConfig;
use crate::error::CoreError;
use crate::headers::BloomScanner;
use crate::receipts::verify_era1_receipts;
use crate::source::{
    decode_era1_header, decode_era1_receipts, decode_era1_tx_hashes, iter_era1_block_tuples,
    SourceFileManifest,
};
use crate::types::StoredEvent as CoreEvent;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use scopenode_storage::Db;
use tracing::warn;

struct PreparedScope {
    name: String,
    from: u64,
    to: u64,
    targets: Vec<crate::types::BloomTarget>,
    decoder: EventDecoder,
}

impl PreparedScope {
    async fn new(contract: &ContractConfig, abi_cache: &mut AbiCache) -> Result<Self, CoreError> {
        let events = abi_cache.get_or_fetch(contract).await?;
        let topic0s: Vec<_> = events.iter().map(|e| e.topic0()).collect();
        let targets = BloomScanner::build_targets(&topic0s, contract.address);
        let decoder = EventDecoder::new(&events, contract.address)?;
        let to = contract.to_block.ok_or_else(|| {
            CoreError::Internal("ERA1 scope requires to_block — live sync not yet supported".into())
        })?;

        Ok(Self {
            name: contract
                .name
                .clone()
                .unwrap_or_else(|| contract.address.to_checksum(None)),
            from: contract.from_block,
            to,
            targets,
            decoder,
        })
    }

    fn overlaps_file(&self, file: &SourceFileManifest) -> bool {
        file.ranges
            .iter()
            .any(|r| r.to_block >= self.from && r.from_block <= self.to)
    }

    fn contains_block(&self, block_number: u64) -> bool {
        block_number >= self.from && block_number <= self.to
    }
}

/// Run the ERA1 indexing pipeline for one contract scope.
///
/// `files` must be the manifest entries from the source scan. Only files whose
/// covered range overlaps `[contract.from_block, contract.to_block]` are processed.
///
/// Each file is read sequentially in one pass. Bloom hits trigger full receipt
/// decode and Merkle verification. Verification failures are logged and skipped.
pub async fn run_era1_scope(
    files: &[SourceFileManifest],
    contract: &ContractConfig,
    abi_cache: &mut AbiCache,
    db: &Db,
    progress: &MultiProgress,
) -> Result<(), CoreError> {
    run_era1_scopes(
        files,
        std::slice::from_ref(contract),
        abi_cache,
        db,
        progress,
    )
    .await
}

/// Run the ERA1 indexing pipeline for all configured contract scopes in one pass.
///
/// Files are scanned once. For each block, the header is decoded once, bloom
/// matches are evaluated for all active scopes, and receipts/body data are
/// decompressed and verified once when at least one scope might match.
pub async fn run_era1_scopes(
    files: &[SourceFileManifest],
    contracts: &[ContractConfig],
    abi_cache: &mut AbiCache,
    db: &Db,
    progress: &MultiProgress,
) -> Result<(), CoreError> {
    let mut scopes = Vec::new();
    for contract in contracts {
        match PreparedScope::new(contract, abi_cache).await {
            Ok(scope) => scopes.push(scope),
            Err(err) if contracts.len() == 1 => return Err(err),
            Err(err) => {
                warn!(contract = %contract.address, err = %err, "Skipping scope preparation failure")
            }
        }
    }

    if scopes.is_empty() {
        return Ok(());
    }

    let covering: Vec<&SourceFileManifest> = files
        .iter()
        .filter(|file| scopes.iter().any(|scope| scope.overlaps_file(file)))
        .collect();

    let total_blocks: u64 = covering
        .iter()
        .flat_map(|f| &f.ranges)
        .map(|r| r.to_block.saturating_sub(r.from_block) + 1)
        .sum();

    let pb = progress.add(ProgressBar::new(total_blocks.max(1)));
    pb.set_style(
        ProgressStyle::default_bar()
            .template("ERA1 index     {bar:20.cyan/blue}  {pos:>7} / {len:<7}  {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("█░"),
    );
    pb.set_message("scanning...");

    let mut total_events = 0usize;

    for file in &covering {
        let active_scope_indices: Vec<usize> = scopes
            .iter()
            .enumerate()
            .filter_map(|(idx, scope)| scope.overlaps_file(file).then_some(idx))
            .collect();
        if active_scope_indices.is_empty() {
            continue;
        }

        let tuples = match iter_era1_block_tuples(&file.path) {
            Ok(it) => it,
            Err(e) => {
                warn!(path = %file.path.display(), err = %e, "Failed to open ERA1 file — skipping");
                continue;
            }
        };

        for tuple_result in tuples {
            let tuple = match tuple_result {
                Ok(t) => t,
                Err(e) => {
                    warn!(err = %e, "ERA1 read error — skipping block");
                    continue;
                }
            };

            pb.inc(1);

            let matching_scope_indices_by_range: Vec<usize> = active_scope_indices
                .iter()
                .copied()
                .filter(|idx| scopes[*idx].contains_block(tuple.block_number))
                .collect();
            if matching_scope_indices_by_range.is_empty() {
                continue;
            }

            let header = match decode_era1_header(&tuple.compressed_header) {
                Ok(h) => h,
                Err(e) => {
                    warn!(block = tuple.block_number, err = %e, "Header decode failed — skipping");
                    continue;
                }
            };

            let matching_scope_indices: Vec<usize> = matching_scope_indices_by_range
                .into_iter()
                .filter(|idx| BloomScanner::matches(&header.logs_bloom, &scopes[*idx].targets))
                .collect();
            if matching_scope_indices.is_empty() {
                continue;
            }

            let receipts = match decode_era1_receipts(&tuple.compressed_receipts) {
                Ok(r) => r,
                Err(e) => {
                    warn!(block = header.number, err = %e, "Receipt decode failed — skipping");
                    continue;
                }
            };

            if let Err(e) = verify_era1_receipts(&receipts, header.receipts_root, header.number) {
                warn!(block = header.number, err = %e, "Receipt Merkle verify failed — skipping");
                continue;
            }

            let tx_hashes = decode_era1_tx_hashes(&tuple.compressed_body).unwrap_or_default();
            let mut storage_events = Vec::new();

            for idx in matching_scope_indices {
                let scope = &scopes[idx];
                let core_events = scope.decoder.extract_and_decode_era1(
                    &receipts,
                    &tx_hashes,
                    header.number,
                    header.hash,
                    header.timestamp,
                );
                storage_events.extend(core_events.iter().map(core_to_storage_event));
            }

            if !storage_events.is_empty() {
                if let Err(e) = db.insert_events(&storage_events).await {
                    warn!(block = header.number, err = %e, "Event insert failed");
                } else {
                    total_events += storage_events.len();
                }
            }
        }
    }

    let scope_names = scopes
        .iter()
        .map(|scope| scope.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    pb.finish_with_message(format!("done ({total_events} events across {scope_names})"));
    Ok(())
}

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
        raw_topics: serde_json::to_string(
            &e.raw_topics
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default(),
        raw_data: alloy_primitives::hex::encode(&e.raw_data),
        decoded: e.decoded.to_string(),
        source: e.source.clone(),
    }
}
