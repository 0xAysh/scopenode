//! ERA1-backed scope indexing pipeline.
//!
//! Replaces the devp2p pipeline for historical data. Single sequential pass
//! per ERA1 file: bloom-check each header, decode receipts for hits, verify
//! the receipt Merkle root, ABI-decode matching logs, and store events.

use crate::abi::{AbiCache, DecodedEvent, EventDecoder};
use crate::config::ContractConfig;
use crate::error::CoreError;
use crate::headers::BloomScanner;
use crate::receipts::verify_era1_receipts;
use crate::source::{
    decode_era1_header, decode_era1_receipts, decode_era1_tx_hashes, iter_era1_block_tuples,
    Era1BlockTuple, SourceFileManifest,
};
use async_trait::async_trait;
use tracing::warn;

/// Sink for pipeline progress events. Implement to drive a terminal bar or suppress output.
pub trait ProgressReporter {
    fn set_total(&self, n: u64);
    fn inc(&self);
    fn finish(&self, msg: &str);
}

/// Sink that receives decoded events from the pipeline and persists them.
///
/// Implement this to store events (e.g., `DbEventSink` in `scopenode-storage`)
/// or capture them in memory for testing.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn store(&self, events: Vec<DecodedEvent>) -> Result<usize, CoreError>;
}

/// No-op reporter for tests and programmatic callers that don't need progress output.
pub struct NullReporter;

impl ProgressReporter for NullReporter {
    fn set_total(&self, _n: u64) {}
    fn inc(&self) {}
    fn finish(&self, _msg: &str) {}
}

struct PreparedScope {
    name: String,
    from: u64,
    to: u64,
    scanner: BloomScanner,
    decoder: EventDecoder,
}

impl PreparedScope {
    async fn new(contract: &ContractConfig, abi_cache: &mut AbiCache) -> Result<Self, CoreError> {
        let events = abi_cache.get_or_fetch(contract).await?;
        let topic0s: Vec<_> = events.iter().map(|e| e.topic0()).collect();
        let scanner = BloomScanner::new(&topic0s, contract.address);
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
            scanner,
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

/// Encapsulates all per-block processing for the ERA1 pipeline.
///
/// Given one `Era1BlockTuple`, `process()` performs in order: block range
/// filtering against all scopes, header decoding, Bloom pre-screening, receipt
/// decoding, Merkle root verification, transaction hash extraction, and
/// per-scope event extraction.
///
/// Returns an empty `Vec` for blocks that produce no events — whether because
/// no scope's range matches, no Bloom hit, or a decode/verify failure. The
/// outer file-iteration loop is the only caller; it stays thin and free of
/// decoding logic.
struct BlockPipeline {
    scopes: Vec<PreparedScope>,
}

impl BlockPipeline {
    fn new(scopes: Vec<PreparedScope>) -> Self {
        Self { scopes }
    }

    fn process(&self, tuple: &Era1BlockTuple) -> Vec<DecodedEvent> {
        let in_range: Vec<&PreparedScope> = self
            .scopes
            .iter()
            .filter(|s| s.contains_block(tuple.block_number))
            .collect();
        if in_range.is_empty() {
            return vec![];
        }

        let header = match decode_era1_header(&tuple.compressed_header) {
            Ok(h) => h,
            Err(e) => {
                warn!(block = tuple.block_number, err = %e, "Header decode failed — skipping");
                return vec![];
            }
        };

        let bloom_matches: Vec<&PreparedScope> = in_range
            .into_iter()
            .filter(|s| s.scanner.matches(&header.logs_bloom))
            .collect();
        if bloom_matches.is_empty() {
            return vec![];
        }

        let receipts = match decode_era1_receipts(&tuple.compressed_receipts) {
            Ok(r) => r,
            Err(e) => {
                warn!(block = header.number, err = %e, "Receipt decode failed — skipping");
                return vec![];
            }
        };

        if let Err(e) = verify_era1_receipts(&receipts, header.receipts_root, header.number) {
            warn!(block = header.number, err = %e, "Receipt Merkle verify failed — skipping");
            return vec![];
        }

        let tx_hashes = decode_era1_tx_hashes(&tuple.compressed_body).unwrap_or_default();
        let mut events = Vec::new();
        for scope in bloom_matches {
            events.extend(scope.decoder.extract_and_decode(
                &receipts,
                &tx_hashes,
                header.number,
                header.hash,
                header.timestamp,
                "era1",
            ));
        }
        events
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
    sink: &dyn EventSink,
    reporter: &dyn ProgressReporter,
) -> Result<(), CoreError> {
    run_era1_scopes(
        files,
        std::slice::from_ref(contract),
        abi_cache,
        sink,
        reporter,
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
    sink: &dyn EventSink,
    reporter: &dyn ProgressReporter,
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

    reporter.set_total(total_blocks.max(1));

    let scope_names: Vec<String> = scopes.iter().map(|s| s.name.clone()).collect();
    let pipeline = BlockPipeline::new(scopes);
    let mut total_events = 0usize;

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

            reporter.inc();

            let block_events = pipeline.process(&tuple);
            if !block_events.is_empty() {
                match sink.store(block_events).await {
                    Ok(n) => total_events += n,
                    Err(e) => warn!(block = tuple.block_number, err = %e, "Event insert failed"),
                }
            }
        }
    }

    reporter.finish(&format!(
        "done ({total_events} events across {})",
        scope_names.join(", ")
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{EventAbi, EventDecoder};
    use alloy_primitives::{address, keccak256, Address, Bloom, BloomInput, B256};
    use alloy_rlp::Encodable;

    fn snappy_compress(data: &[u8]) -> Vec<u8> {
        use snap::write::FrameEncoder;
        use std::io::Write;
        let mut out = Vec::new();
        {
            let mut enc = FrameEncoder::new(&mut out);
            enc.write_all(data).unwrap();
            enc.flush().unwrap();
        }
        out
    }

    fn make_scope(from: u64, to: u64) -> PreparedScope {
        let addr = address!("0000000000000000000000000000000000000001");
        PreparedScope {
            name: "test".to_string(),
            from,
            to,
            scanner: BloomScanner::new(&[], addr),
            decoder: EventDecoder::new(&[], addr).unwrap(),
        }
    }

    fn make_scope_with_target(from: u64, to: u64, addr: Address, topic0: B256) -> PreparedScope {
        PreparedScope {
            name: "test".to_string(),
            from,
            to,
            scanner: BloomScanner::new(&[topic0], addr),
            decoder: EventDecoder::new(&[], addr).unwrap(),
        }
    }

    fn empty_tuple(block_number: u64) -> Era1BlockTuple {
        Era1BlockTuple {
            block_number,
            compressed_header: vec![],
            compressed_body: vec![],
            compressed_receipts: vec![],
            total_difficulty: vec![],
        }
    }

    fn make_compressed_header(block_number: u64, bloom: Bloom, receipts_root: B256) -> Vec<u8> {
        use alloy_consensus::Header;
        let header = Header {
            number: block_number,
            logs_bloom: bloom,
            receipts_root,
            ..Default::default()
        };
        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        snappy_compress(&rlp)
    }

    #[test]
    fn prepared_scope_contains_block_boundary() {
        let scope = make_scope(100, 200);
        assert!(!scope.contains_block(99));
        assert!(scope.contains_block(100));
        assert!(scope.contains_block(150));
        assert!(scope.contains_block(200));
        assert!(!scope.contains_block(201));
    }

    #[test]
    fn block_out_of_range_returns_empty() {
        let pipeline = BlockPipeline::new(vec![make_scope(100, 200)]);
        assert!(pipeline.process(&empty_tuple(99)).is_empty());
        assert!(pipeline.process(&empty_tuple(201)).is_empty());
    }

    #[test]
    fn malformed_header_returns_empty() {
        let pipeline = BlockPipeline::new(vec![make_scope(100, 200)]);
        let tuple = Era1BlockTuple {
            block_number: 150,
            compressed_header: vec![0xAA, 0xBB, 0xCC],
            ..empty_tuple(150)
        };
        assert!(pipeline.process(&tuple).is_empty());
    }

    #[test]
    fn bloom_no_match_returns_empty() {
        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline =
            BlockPipeline::new(vec![make_scope_with_target(100, 200, addr, topic0)]);

        // Header with empty bloom — scanner's address/topic0 are not present.
        let compressed_header =
            make_compressed_header(150, Bloom::default(), B256::ZERO);
        let tuple = Era1BlockTuple {
            block_number: 150,
            compressed_header,
            ..empty_tuple(150)
        };
        assert!(pipeline.process(&tuple).is_empty());
    }

    #[test]
    fn receipt_decode_failure_returns_empty() {
        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline =
            BlockPipeline::new(vec![make_scope_with_target(100, 200, addr, topic0)]);

        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(addr.as_slice()));
        bloom.accrue(BloomInput::Raw(topic0.as_slice()));
        let compressed_header = make_compressed_header(150, bloom, B256::ZERO);

        let tuple = Era1BlockTuple {
            block_number: 150,
            compressed_header,
            compressed_receipts: vec![0xFF, 0xFE, 0xFD], // garbage
            ..empty_tuple(150)
        };
        assert!(pipeline.process(&tuple).is_empty());
    }

    #[test]
    fn merkle_verify_failure_returns_empty() {
        use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom};
        use alloy_primitives::Log as PrimitiveLog;

        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline =
            BlockPipeline::new(vec![make_scope_with_target(100, 200, addr, topic0)]);

        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(addr.as_slice()));
        bloom.accrue(BloomInput::Raw(topic0.as_slice()));

        // Header with wrong receipts_root — mismatches the receipts below.
        let wrong_root = B256::from([0xAA; 32]);
        let compressed_header = make_compressed_header(150, bloom, wrong_root);

        let receipt = ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::default(),
        };
        let mut receipt_rlp = Vec::new();
        receipt.encode(&mut receipt_rlp);
        let outer = alloy_rlp::Header {
            list: true,
            payload_length: receipt_rlp.len(),
        };
        let mut receipts_buf = Vec::new();
        outer.encode(&mut receipts_buf);
        receipts_buf.extend_from_slice(&receipt_rlp);

        let tuple = Era1BlockTuple {
            block_number: 150,
            compressed_header,
            compressed_receipts: snappy_compress(&receipts_buf),
            ..empty_tuple(150)
        };
        assert!(pipeline.process(&tuple).is_empty());
    }

    #[test]
    fn no_scopes_returns_empty() {
        let pipeline = BlockPipeline::new(vec![]);
        assert!(pipeline.process(&empty_tuple(100)).is_empty());
    }

    #[test]
    fn event_abi_with_empty_inputs_constructs_decoder() {
        let addr = address!("0000000000000000000000000000000000000001");
        let events = vec![EventAbi {
            name: "Transfer".to_string(),
            inputs: vec![],
        }];
        assert!(EventDecoder::new(&events, addr).is_ok());
    }
}
