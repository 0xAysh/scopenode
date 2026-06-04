//! ERA1-backed scope indexing pipeline.
//!
//! Replaces the devp2p pipeline for historical data. Single sequential pass
//! per ERA1 file: bloom-check each header, decode receipts for hits, verify
//! the receipt Merkle root, ABI-decode matching logs, and store events.

use crate::abi::{DecodedEvent, EventDecoder};
use crate::abi_resolution::AbiResolver;
use crate::config::ContractConfig;
use crate::era1_reader::Era1BlockFacts;
use crate::error::{CoreError, VerifyError};
use crate::headers::BloomScanner;
use crate::receipts::verify_era1_receipts;
use crate::source::Era1Source;
use alloy_consensus::ReceiptEnvelope;
use alloy_primitives::{Log as PrimitiveLog, B256};
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

/// Sink that receives Coverage facts from the pipeline.
#[async_trait]
pub trait CoverageSink: Send + Sync {
    async fn record_coverage(
        &self,
        contract: &str,
        from_block: u64,
        to_block: u64,
    ) -> Result<(), CoreError>;
}

pub trait PipelineSink: EventSink + CoverageSink {}

impl<T> PipelineSink for T where T: EventSink + CoverageSink {}

/// In-memory event sink for programmatic callers and tests.
///
/// This is the second concrete adapter for the `EventSink` seam; production
/// storage still uses `DbEventSink` in `scopenode-storage`.
#[derive(Default)]
pub struct InMemoryEventSink {
    events: tokio::sync::Mutex<Vec<DecodedEvent>>,
    covered_ranges: tokio::sync::Mutex<Vec<(String, u64, u64)>>,
}

impl InMemoryEventSink {
    pub async fn events(&self) -> Vec<DecodedEvent> {
        self.events.lock().await.clone()
    }

    pub async fn covered_ranges(&self) -> Vec<(String, u64, u64)> {
        self.covered_ranges.lock().await.clone()
    }
}

#[async_trait]
impl EventSink for InMemoryEventSink {
    async fn store(&self, events: Vec<DecodedEvent>) -> Result<usize, CoreError> {
        let count = events.len();
        self.events.lock().await.extend(events);
        Ok(count)
    }
}

#[async_trait]
impl CoverageSink for InMemoryEventSink {
    async fn record_coverage(
        &self,
        contract: &str,
        from_block: u64,
        to_block: u64,
    ) -> Result<(), CoreError> {
        self.covered_ranges
            .lock()
            .await
            .push((contract.to_string(), from_block, to_block));
        Ok(())
    }
}

/// No-op reporter for tests and programmatic callers that don't need progress output.
pub struct NullReporter;

impl ProgressReporter for NullReporter {
    fn set_total(&self, _n: u64) {}
    fn inc(&self) {}
    fn finish(&self, _msg: &str) {}
}

/// Pluggable Merkle receipt verification strategy for `BlockPipeline`.
///
/// The production pipeline uses [`MerkleVerifier`] which calls the real
/// `verify_era1_receipts` function. Tests inject [`NoopVerifier`] or
/// [`AlwaysFailVerifier`] to exercise recovery paths without ERA1 files.
pub trait ReceiptVerifier: Send + Sync {
    fn verify(
        &self,
        receipts: &[ReceiptEnvelope<PrimitiveLog>],
        expected_root: B256,
        block_num: u64,
    ) -> Result<(), VerifyError>;
}

/// Production verifier — delegates to the Merkle Patricia Trie implementation.
pub struct MerkleVerifier;

impl ReceiptVerifier for MerkleVerifier {
    fn verify(
        &self,
        receipts: &[ReceiptEnvelope<PrimitiveLog>],
        expected_root: B256,
        block_num: u64,
    ) -> Result<(), VerifyError> {
        verify_era1_receipts(receipts, expected_root, block_num)
    }
}

/// Test verifier — always succeeds without checking the Merkle root.
pub struct NoopVerifier;

impl ReceiptVerifier for NoopVerifier {
    fn verify(
        &self,
        _receipts: &[ReceiptEnvelope<PrimitiveLog>],
        _expected_root: B256,
        _block_num: u64,
    ) -> Result<(), VerifyError> {
        Ok(())
    }
}

/// Test verifier — always fails, simulating a corrupt or tampered receipt set.
pub struct AlwaysFailVerifier;

impl ReceiptVerifier for AlwaysFailVerifier {
    fn verify(
        &self,
        _receipts: &[ReceiptEnvelope<PrimitiveLog>],
        _expected_root: B256,
        block_num: u64,
    ) -> Result<(), VerifyError> {
        Err(VerifyError::RootMismatch {
            block_num,
            expected: B256::ZERO,
            computed: B256::from([0xAA; 32]),
        })
    }
}

struct PreparedScope {
    contract: String,
    name: String,
    from: u64,
    to: u64,
    scanner: BloomScanner,
    decoder: EventDecoder,
}

impl PreparedScope {
    async fn new(contract: &ContractConfig, abi_resolver: &AbiResolver) -> Result<Self, CoreError> {
        let events = abi_resolver.resolve_events(contract).await?;
        let topic0s: Vec<_> = events.iter().map(|e| e.topic0()).collect();
        let scanner = BloomScanner::new(&topic0s, contract.address);
        let decoder = EventDecoder::new(&events, contract.address)?;
        let to = contract.to_block.ok_or_else(|| {
            CoreError::Internal("ERA1 scope requires to_block — live sync not yet supported".into())
        })?;

        Ok(Self {
            contract: contract.address.to_checksum(None),
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

    fn contains_block(&self, block_number: u64) -> bool {
        block_number >= self.from && block_number <= self.to
    }
}

/// Encapsulates all per-block processing for the ERA1 pipeline.
///
/// Given one `Era1BlockFacts`, `process()` performs in order: block range
/// filtering against all scopes, Bloom pre-screening, Merkle root verification
/// (via the injected [`ReceiptVerifier`]), and per-scope event extraction.
///
/// Returns an empty `Vec` for blocks that produce no events — whether because
/// no scope's range matches, no Bloom hit, or a verify failure. The outer
/// file-iteration loop is the only caller; it stays thin and free of
/// decoding logic.
struct BlockPipeline {
    scopes: Vec<PreparedScope>,
    verifier: Box<dyn ReceiptVerifier>,
}

impl BlockPipeline {
    fn new(scopes: Vec<PreparedScope>) -> Self {
        Self {
            scopes,
            verifier: Box::new(MerkleVerifier),
        }
    }

    #[cfg(test)]
    fn with_verifier(scopes: Vec<PreparedScope>, verifier: Box<dyn ReceiptVerifier>) -> Self {
        Self { scopes, verifier }
    }

    fn process(&self, facts: &Era1BlockFacts) -> Vec<DecodedEvent> {
        let in_range: Vec<&PreparedScope> = self
            .scopes
            .iter()
            .filter(|s| s.contains_block(facts.block_number))
            .collect();
        if in_range.is_empty() {
            return vec![];
        }

        let bloom_matches: Vec<&PreparedScope> = in_range
            .into_iter()
            .filter(|s| s.scanner.matches(&facts.header.logs_bloom))
            .collect();
        if bloom_matches.is_empty() {
            return vec![];
        }

        if let Err(e) = self.verifier.verify(
            &facts.receipts,
            facts.header.receipts_root,
            facts.header.number,
        ) {
            warn!(block = facts.header.number, err = %e, "Receipt Merkle verify failed — skipping");
            return vec![];
        }

        let mut events = Vec::new();
        for scope in bloom_matches {
            events.extend(scope.decoder.extract_and_decode(
                &facts.receipts,
                &facts.tx_hashes,
                facts.header.number,
                facts.header.hash,
                facts.header.timestamp,
                "era1",
            ));
        }
        events
    }
}

/// Run the ERA1 indexing pipeline for one contract scope.
///
/// The source's manifest determines which files overlap
/// `[contract.from_block, contract.to_block]`.
///
/// Each file is read sequentially in one pass. Bloom hits trigger full receipt
/// decode and Merkle verification. Verification failures are logged and skipped.
pub async fn run_era1_scope(
    source: &Era1Source,
    contract: &ContractConfig,
    abi_resolver: &AbiResolver,
    sink: &dyn PipelineSink,
    reporter: &dyn ProgressReporter,
) -> Result<(), CoreError> {
    run_era1_scopes(
        source,
        std::slice::from_ref(contract),
        abi_resolver,
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
    source: &Era1Source,
    contracts: &[ContractConfig],
    abi_resolver: &AbiResolver,
    sink: &dyn PipelineSink,
    reporter: &dyn ProgressReporter,
) -> Result<(), CoreError> {
    let mut scopes = Vec::new();
    for contract in contracts {
        match PreparedScope::new(contract, abi_resolver).await {
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

    let ranges: Vec<(u64, u64)> = scopes.iter().map(|scope| (scope.from, scope.to)).collect();
    let selected_files = source.block_facts_for_ranges(&ranges);

    reporter.set_total(selected_files.total_blocks().max(1));

    let scope_names: Vec<String> = scopes.iter().map(|s| s.name.clone()).collect();
    let pipeline = BlockPipeline::new(scopes);
    let mut total_events = 0usize;

    for file in selected_files.files() {
        let facts_iter = match file.block_facts() {
            Ok(it) => it,
            Err(e) => {
                warn!(path = %file.path().display(), err = %e, "Failed to open ERA1 file — skipping");
                continue;
            }
        };

        for facts_result in facts_iter {
            let facts = match facts_result {
                Ok(f) => f,
                Err(e) => {
                    warn!(err = %e, "ERA1 read error — skipping block");
                    continue;
                }
            };

            reporter.inc();

            let block_number = facts.block_number;
            let block_events = pipeline.process(&facts);
            if !block_events.is_empty() {
                match sink.store(block_events).await {
                    Ok(n) => total_events += n,
                    Err(e) => warn!(block = block_number, err = %e, "Event insert failed"),
                }
            }
        }
    }

    for scope in pipeline.scopes.iter() {
        if let Err(e) = sink
            .record_coverage(&scope.contract, scope.from, scope.to)
            .await
        {
            warn!(contract = %scope.contract, err = %e, "Coverage record failed");
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
    use crate::types::ScopeHeader;
    use alloy_primitives::{address, keccak256, Address, Bloom, BloomInput, B256};

    fn make_scope(from: u64, to: u64) -> PreparedScope {
        let addr = address!("0000000000000000000000000000000000000001");
        PreparedScope {
            contract: addr.to_checksum(None),
            name: "test".to_string(),
            from,
            to,
            scanner: BloomScanner::new(&[], addr),
            decoder: EventDecoder::new(&[], addr).unwrap(),
        }
    }

    fn make_scope_with_target(from: u64, to: u64, addr: Address, topic0: B256) -> PreparedScope {
        PreparedScope {
            contract: addr.to_checksum(None),
            name: "test".to_string(),
            from,
            to,
            scanner: BloomScanner::new(&[topic0], addr),
            decoder: EventDecoder::new(&[], addr).unwrap(),
        }
    }

    fn empty_facts(block_number: u64) -> Era1BlockFacts {
        Era1BlockFacts {
            block_number,
            header: ScopeHeader {
                number: block_number,
                hash: B256::ZERO,
                parent_hash: B256::ZERO,
                timestamp: 0,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 0,
                base_fee_per_gas: None,
            },
            receipts: vec![],
            tx_hashes: vec![],
        }
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
        assert!(pipeline.process(&empty_facts(99)).is_empty());
        assert!(pipeline.process(&empty_facts(201)).is_empty());
    }

    #[test]
    fn bloom_no_match_returns_empty() {
        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline = BlockPipeline::new(vec![make_scope_with_target(100, 200, addr, topic0)]);

        // Facts with empty bloom — scanner's address/topic0 are not present.
        let facts = empty_facts(150);
        assert!(pipeline.process(&facts).is_empty());
    }

    #[test]
    fn bloom_match_with_empty_receipts_returns_empty() {
        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline = BlockPipeline::new(vec![make_scope_with_target(100, 200, addr, topic0)]);

        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(addr.as_slice()));
        bloom.accrue(BloomInput::Raw(topic0.as_slice()));

        let facts = Era1BlockFacts {
            header: ScopeHeader {
                logs_bloom: bloom,
                ..empty_facts(150).header
            },
            ..empty_facts(150)
        };
        // Empty receipts list → no events extracted (Merkle verify passes for empty list
        // only if receipts_root matches — MerkleVerifier may reject; either way, no events).
        // The important thing is: no panic, and result is empty.
        assert!(pipeline.process(&facts).is_empty());
    }

    #[test]
    fn merkle_verify_failure_returns_empty() {
        use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom};
        use alloy_primitives::Log as PrimitiveLog;

        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline = BlockPipeline::new(vec![make_scope_with_target(100, 200, addr, topic0)]);

        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(addr.as_slice()));
        bloom.accrue(BloomInput::Raw(topic0.as_slice()));

        let receipt = ReceiptEnvelope::Legacy(ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::default(),
        });

        // Wrong receipts_root — will not match what MerkleVerifier computes.
        let wrong_root = B256::from([0xAA; 32]);
        let facts = Era1BlockFacts {
            header: ScopeHeader {
                logs_bloom: bloom,
                receipts_root: wrong_root,
                ..empty_facts(150).header
            },
            receipts: vec![receipt],
            ..empty_facts(150)
        };
        assert!(pipeline.process(&facts).is_empty());
    }

    #[test]
    fn always_fail_verifier_returns_empty_for_in_range_bloom_hit() {
        use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom};
        use alloy_primitives::Log as PrimitiveLog;

        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline = BlockPipeline::with_verifier(
            vec![make_scope_with_target(100, 200, addr, topic0)],
            Box::new(AlwaysFailVerifier),
        );

        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(addr.as_slice()));
        bloom.accrue(BloomInput::Raw(topic0.as_slice()));

        let receipt = ReceiptEnvelope::Legacy(ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::default(),
        });

        let facts = Era1BlockFacts {
            header: ScopeHeader {
                logs_bloom: bloom,
                ..empty_facts(150).header
            },
            receipts: vec![receipt],
            ..empty_facts(150)
        };
        assert!(pipeline.process(&facts).is_empty());
    }

    #[test]
    fn no_scopes_returns_empty() {
        let pipeline = BlockPipeline::new(vec![]);
        assert!(pipeline.process(&empty_facts(100)).is_empty());
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

    #[tokio::test]
    async fn in_memory_event_sink_captures_events_and_coverage() {
        let sink = InMemoryEventSink::default();
        let events = vec![DecodedEvent {
            contract: address!("0000000000000000000000000000000000000001"),
            event_name: "Transfer".to_string(),
            topic0: B256::ZERO,
            block_number: 100,
            block_hash: B256::ZERO,
            tx_hash: B256::ZERO,
            tx_index: 0,
            log_index: 0,
            raw_topics: vec![B256::ZERO],
            raw_data: alloy_primitives::Bytes::new(),
            decoded: serde_json::json!({}),
            source: "era1".to_string(),
            timestamp: 0,
        }];

        let stored = sink.store(events.clone()).await.unwrap();
        sink.record_coverage("0xabc", 100, 200).await.unwrap();

        assert_eq!(stored, 1);
        let captured = sink.events().await;
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].event_name, "Transfer");
        assert_eq!(captured[0].block_number, 100);
        assert_eq!(
            sink.covered_ranges().await,
            vec![("0xabc".to_string(), 100, 200)]
        );
    }
}
