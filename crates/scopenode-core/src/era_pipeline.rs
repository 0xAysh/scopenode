//! ERA1-backed scope indexing pipeline.
//!
//! Replaces the devp2p pipeline for historical data. Single sequential pass
//! per ERA1 file: bloom-check each header, decode receipts for hits, verify
//! the receipt Merkle root, ABI-decode matching logs, and store events.

use crate::abi::{AbiCache, EventDecoder};
use crate::config::ContractConfig;
use crate::error::CoreError;
use crate::headers::BloomScanner;
use crate::pipeline::core_to_storage_event;
use crate::receipts::verify_era1_receipts;
use crate::source::{
    decode_era1_header, decode_era1_receipts, decode_era1_tx_hashes, iter_era1_block_tuples,
    SourceFileManifest,
};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use scopenode_storage::Db;
use tracing::warn;

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
    let events = abi_cache.get_or_fetch(contract).await?;
    let topic0s: Vec<_> = events.iter().map(|e| e.topic0()).collect();
    let targets = BloomScanner::build_targets(&topic0s, contract.address);
    let decoder = EventDecoder::new(&events, contract.address)?;

    let from = contract.from_block;
    let to = contract.to_block.ok_or_else(|| {
        CoreError::Internal(
            "ERA1 scope requires to_block — live sync not yet supported".into(),
        )
    })?;

    // Only process files whose range overlaps the requested scope.
    let covering: Vec<&SourceFileManifest> = files
        .iter()
        .filter(|f| {
            f.ranges
                .iter()
                .any(|r| r.to_block >= from && r.from_block <= to)
        })
        .collect();

    let total_blocks: u64 = covering
        .iter()
        .flat_map(|f| &f.ranges)
        .map(|r| {
            let start = r.from_block.max(from);
            let end = r.to_block.min(to);
            if end >= start { end - start + 1 } else { 0 }
        })
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
    let addr_str = contract.address.to_checksum(None);

    for file in &covering {
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

            if tuple.block_number < from || tuple.block_number > to {
                continue;
            }

            pb.inc(1);

            let header = match decode_era1_header(&tuple.compressed_header) {
                Ok(h) => h,
                Err(e) => {
                    warn!(block = tuple.block_number, err = %e, "Header decode failed — skipping");
                    continue;
                }
            };

            if !BloomScanner::matches(&header.logs_bloom, &targets) {
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

            let core_events = decoder.extract_and_decode_era1(
                &receipts,
                &tx_hashes,
                header.number,
                header.hash,
                header.timestamp,
            );

            if !core_events.is_empty() {
                let storage_events: Vec<_> =
                    core_events.iter().map(core_to_storage_event).collect();
                if let Err(e) = db.insert_events(&storage_events).await {
                    warn!(block = header.number, contract = %addr_str, err = %e, "Event insert failed");
                } else {
                    total_events += core_events.len();
                }
            }
        }
    }

    pb.finish_with_message(format!("done ({total_events} events)"));
    Ok(())
}
