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
use crate::source::{Era1Source, SourceError};
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

/// Outcome of evaluating one block against all prepared scopes.
///
/// Preserves the distinction between safe emptiness and incomplete evaluation
/// until Coverage eligibility is decided: a block with no events is still
/// coverable, a block whose receipts failed verification is not.
#[derive(Debug)]
enum BlockOutcome {
    /// The block is outside every scope's range.
    OutsideRange,
    /// In range, but no scope's Bloom matched — safely empty.
    NoBloomMatch,
    /// Receipts verified and decoding ran to completion. Events may be empty;
    /// that is valid emptiness, not a failure.
    Evaluated(Vec<DecodedEvent>),
    /// Receipt verification failed — the block is incomplete and every
    /// affected Contract scope is ineligible for Coverage.
    VerifyFailed,
    /// Receipts verified, but one or more events could not be fully decoded.
    /// The lossy events are still storable (raw topics and data preserved),
    /// but every affected Contract scope is ineligible for Coverage.
    DecodeFailed(Vec<DecodedEvent>),
}

/// Encapsulates all per-block processing for the ERA1 pipeline.
///
/// Given one `Era1BlockFacts`, `process()` performs in order: block range
/// filtering against all scopes, Bloom pre-screening, Merkle root verification
/// (via the injected [`ReceiptVerifier`]), and per-scope event extraction.
///
/// Returns a [`BlockOutcome`] so the outer file-iteration loop can apply
/// Coverage policy without inferring completion from an empty event list.
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

    fn process(&self, facts: &Era1BlockFacts) -> BlockOutcome {
        let in_range: Vec<&PreparedScope> = self
            .scopes
            .iter()
            .filter(|s| s.contains_block(facts.block_number))
            .collect();
        if in_range.is_empty() {
            return BlockOutcome::OutsideRange;
        }

        let bloom_matches: Vec<&PreparedScope> = in_range
            .into_iter()
            .filter(|s| s.scanner.matches(&facts.header.logs_bloom))
            .collect();
        if bloom_matches.is_empty() {
            return BlockOutcome::NoBloomMatch;
        }

        if let Err(e) = self.verifier.verify(
            &facts.receipts,
            facts.header.receipts_root,
            facts.header.number,
        ) {
            warn!(block = facts.header.number, err = %e, "Receipt Merkle verify failed — block incomplete");
            return BlockOutcome::VerifyFailed;
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
        if events.iter().any(DecodedEvent::has_decode_error) {
            warn!(
                block = facts.header.number,
                "Event decode fell back to raw data — block incomplete"
            );
            return BlockOutcome::DecodeFailed(events);
        }
        BlockOutcome::Evaluated(events)
    }
}

/// Why a Contract scope did not earn Coverage in a run.
///
/// [`Self::NoSourceData`] means the blocks have no backing archive file at all
/// (empty `era_dir`, missing epoch, or a range past the available files) and is
/// kept distinct from the "present but unusable" reasons
/// ([`Self::VerifyFailure`], [`Self::DecodeFailure`], [`Self::SourceFailure`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompleteReason {
    /// ABI resolution or scope setup failed before the pipeline could run.
    PreparationFailed,
    /// An archive file could not be opened or a block could not be read.
    SourceFailure,
    /// Receipt verification failed for a block in the scope's range.
    VerifyFailure,
    /// One or more events fell back to raw data during decoding.
    DecodeFailure,
    /// The event sink failed to persist decoded events.
    StoreFailure,
    /// Coverage itself could not be persisted.
    CoverageWriteFailed,
    /// Part (or all) of the requested range had no backing archive file, so it
    /// was never read. Carries the missing block sub-range.
    NoSourceData { from_block: u64, to_block: u64 },
}

impl IncompleteReason {
    /// Human-readable cause for operator-facing output.
    pub fn describe(&self) -> String {
        match self {
            Self::PreparationFailed => "ABI resolution failure".to_string(),
            Self::SourceFailure => "archive source read failure".to_string(),
            Self::VerifyFailure => "receipt verification failure".to_string(),
            Self::DecodeFailure => "event decode failure".to_string(),
            Self::StoreFailure => "event storage failure".to_string(),
            Self::CoverageWriteFailed => "coverage record failure".to_string(),
            Self::NoSourceData {
                from_block,
                to_block,
            } => format!("no backing source data for blocks {from_block}-{to_block}"),
        }
    }
}

/// Completion summary for one pipeline run.
///
/// Coverage is recorded per scope only when the scope completes without a
/// read, verification, decode, or storage failure. `incomplete` names every
/// scope that did not earn Coverage and why; a successful rerun recovers.
#[derive(Debug)]
pub struct SyncReport {
    /// Events written to the sink across all scopes.
    pub total_events: usize,
    /// Contract addresses whose Coverage was recorded this run.
    pub covered: Vec<String>,
    /// Contract addresses that did not earn Coverage, with the reason.
    pub incomplete: Vec<(String, IncompleteReason)>,
}

impl SyncReport {
    /// True when every prepared scope earned Coverage.
    pub fn is_complete(&self) -> bool {
        self.incomplete.is_empty()
    }
}

/// Run the ERA1 indexing pipeline for one contract scope.
///
/// The source's manifest determines which files overlap
/// `[contract.from_block, contract.to_block]`.
///
/// Each file is read sequentially in one pass. Bloom hits trigger full receipt
/// decode and Merkle verification. A verification failure makes every affected
/// Contract scope ineligible for Coverage for this run; a rerun can recover.
pub async fn run_era1_scope(
    source: &Era1Source,
    contract: &ContractConfig,
    abi_resolver: &AbiResolver,
    sink: &dyn PipelineSink,
    reporter: &dyn ProgressReporter,
) -> Result<SyncReport, CoreError> {
    run_era1_scopes(
        source,
        std::slice::from_ref(contract),
        abi_resolver,
        sink,
        reporter,
    )
    .await
}

/// Selected Block facts ready for one pipeline run — the Archive source's
/// traversal product.
///
/// The production adapter is `Era1BlockFactSelection`: it owns file selection,
/// file lifecycle, and `.era1`/`.ere` format dispatch. Tests substitute an
/// in-memory stream so pipeline failure paths need no real archive files.
pub trait BlockFactStream {
    /// Total blocks the stream will attempt, for progress reporting.
    fn total_blocks(&self) -> u64;
    /// Stream every Block fact across all selected files in order. Errors
    /// carry file path context; traversal continues after a failed file.
    fn blocks(&self) -> Box<dyn Iterator<Item = Result<Era1BlockFacts, SourceError>> + '_>;
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
) -> Result<SyncReport, CoreError> {
    let (scopes, prep_failures) = prepare_scopes(contracts, abi_resolver).await?;
    if scopes.is_empty() {
        return Ok(SyncReport {
            total_events: 0,
            covered: vec![],
            incomplete: prep_failures,
        });
    }

    let ranges: Vec<(u64, u64)> = scopes.iter().map(|scope| (scope.from, scope.to)).collect();
    let selection = source.block_facts_for_ranges(&ranges);
    let mut report = run_prepared_scopes(scopes, &selection, sink, reporter).await?;
    report.incomplete.extend(prep_failures);
    Ok(report)
}

/// Run the indexing pipeline over an already-selected Block fact stream.
///
/// This is the Archive source seam: production callers go through
/// [`run_era1_scopes`], which selects files from an [`Era1Source`]; tests
/// drive the same pipeline with an in-memory [`BlockFactStream`] adapter.
pub async fn run_scopes_over_blocks(
    blocks: &dyn BlockFactStream,
    contracts: &[ContractConfig],
    abi_resolver: &AbiResolver,
    sink: &dyn PipelineSink,
    reporter: &dyn ProgressReporter,
) -> Result<SyncReport, CoreError> {
    let (scopes, prep_failures) = prepare_scopes(contracts, abi_resolver).await?;
    if scopes.is_empty() {
        return Ok(SyncReport {
            total_events: 0,
            covered: vec![],
            incomplete: prep_failures,
        });
    }
    let mut report = run_prepared_scopes(scopes, blocks, sink, reporter).await?;
    report.incomplete.extend(prep_failures);
    Ok(report)
}

async fn prepare_scopes(
    contracts: &[ContractConfig],
    abi_resolver: &AbiResolver,
) -> Result<(Vec<PreparedScope>, Vec<(String, IncompleteReason)>), CoreError> {
    let mut scopes = Vec::new();
    let mut failed: Vec<(String, IncompleteReason)> = Vec::new();
    for contract in contracts {
        match PreparedScope::new(contract, abi_resolver).await {
            Ok(scope) => scopes.push(scope),
            Err(err) if contracts.len() == 1 => return Err(err),
            Err(err) => {
                let label = contract
                    .name
                    .clone()
                    .unwrap_or_else(|| contract.address.to_checksum(None));
                warn!(contract = %contract.address, err = %err, "Skipping scope preparation failure");
                failed.push((label, IncompleteReason::PreparationFailed));
            }
        }
    }
    Ok((scopes, failed))
}

/// Extend the trailing interval with `block`, or open a new one. Assumes blocks
/// arrive in non-descending order (the archive stream's natural order); a block
/// adjacent to or inside the last interval coalesces, otherwise it starts a gap.
fn push_present_block(intervals: &mut Vec<(u64, u64)>, block: u64) {
    match intervals.last_mut() {
        Some((_, end)) if block == *end + 1 => *end = block,
        Some((_, end)) if block <= *end => {} // already covered (duplicate)
        _ => intervals.push((block, block)),
    }
}

/// Sub-ranges of `[from, to]` not covered by `present` (which is ascending,
/// non-overlapping, and contained in `[from, to]`).
fn missing_subranges(from: u64, to: u64, present: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    let mut cursor = from;
    for &(start, end) in present {
        if start > cursor {
            gaps.push((cursor, start - 1));
        }
        cursor = cursor.max(end.saturating_add(1));
    }
    if cursor <= to {
        gaps.push((cursor, to));
    }
    gaps
}

async fn run_prepared_scopes(
    scopes: Vec<PreparedScope>,
    blocks: &dyn BlockFactStream,
    sink: &dyn PipelineSink,
    reporter: &dyn ProgressReporter,
) -> Result<SyncReport, CoreError> {
    reporter.set_total(blocks.total_blocks().max(1));

    let scope_names: Vec<String> = scopes.iter().map(|s| s.name.clone()).collect();
    let pipeline = BlockPipeline::new(scopes);
    let mut total_events = 0usize;
    let mut had_source_failure = false;
    let mut had_store_failure = false;
    // Scopes that saw an incomplete block (e.g. Receipt verification failure)
    // and are therefore ineligible for Coverage this run. The first failure
    // reason per scope is kept for the run report.
    let mut scope_ineligible: Vec<Option<IncompleteReason>> = vec![None; pipeline.scopes.len()];
    // Per scope, the contiguous block sub-ranges actually present in the source
    // and read this run. Coverage is recorded only for these — never for blocks
    // that no archive file backed. Blocks arrive in ascending order, so each new
    // in-range block either extends the last interval or opens a new one.
    let mut scope_present: Vec<Vec<(u64, u64)>> = vec![Vec::new(); pipeline.scopes.len()];

    for item in blocks.blocks() {
        let facts = match item {
            Ok(f) => f,
            Err(e) => {
                had_source_failure = true;
                warn!(err = %e, "Archive source read error — skipping");
                continue;
            }
        };

        reporter.inc();

        let block_number = facts.block_number;
        let (block_events, block_failure) = match pipeline.process(&facts) {
            BlockOutcome::OutsideRange | BlockOutcome::NoBloomMatch => (vec![], None),
            BlockOutcome::Evaluated(events) => (events, None),
            BlockOutcome::VerifyFailed => (vec![], Some(IncompleteReason::VerifyFailure)),
            BlockOutcome::DecodeFailed(events) => (events, Some(IncompleteReason::DecodeFailure)),
        };

        for (idx, scope) in pipeline.scopes.iter().enumerate() {
            if !scope.contains_block(block_number) {
                continue;
            }
            push_present_block(&mut scope_present[idx], block_number);
            if let Some(reason) = block_failure {
                if scope_ineligible[idx].is_none() {
                    scope_ineligible[idx] = Some(reason);
                }
            }
        }

        if !block_events.is_empty() {
            match sink.store(block_events).await {
                Ok(n) => total_events += n,
                Err(e) => {
                    had_store_failure = true;
                    warn!(block = block_number, err = %e, "Event insert failed");
                }
            }
        }
    }

    let run_failure = if had_source_failure {
        Some(IncompleteReason::SourceFailure)
    } else if had_store_failure {
        Some(IncompleteReason::StoreFailure)
    } else {
        None
    };

    let mut covered = Vec::new();
    let mut incomplete = Vec::new();
    for (idx, scope) in pipeline.scopes.iter().enumerate() {
        // "Present but unusable" failures (verify/decode/source/store) take
        // precedence over the "no source data" verdict and withhold all coverage.
        if let Some(reason) = scope_ineligible[idx].or(run_failure) {
            warn!(contract = %scope.contract, reason = reason.describe(), "Skipping Coverage record — scope run incomplete");
            incomplete.push((scope.contract.clone(), reason));
            continue;
        }

        // Record coverage only for the sub-ranges actually read this run.
        let present = &scope_present[idx];
        let mut write_failed = false;
        for &(from, to) in present {
            if let Err(e) = sink.record_coverage(&scope.contract, from, to).await {
                warn!(contract = %scope.contract, err = %e, "Coverage record failed");
                incomplete.push((scope.contract.clone(), IncompleteReason::CoverageWriteFailed));
                write_failed = true;
                break;
            }
        }
        if write_failed {
            continue;
        }

        // Any portion of the requested range with no backing source is reported
        // as incomplete so `sync` exits non-zero and the operator backfills it.
        let missing = missing_subranges(scope.from, scope.to, present);
        match missing.first() {
            None => covered.push(scope.contract.clone()),
            Some(&(from_block, to_block)) => {
                warn!(contract = %scope.contract, from_block, to_block, "No source data for sub-range — scope incomplete");
                incomplete.push((
                    scope.contract.clone(),
                    IncompleteReason::NoSourceData {
                        from_block,
                        to_block,
                    },
                ));
            }
        }
    }

    let status = if incomplete.is_empty() {
        "done"
    } else {
        "incomplete"
    };
    reporter.finish(&format!(
        "{status} ({total_events} events across {})",
        scope_names.join(", ")
    ));
    Ok(SyncReport {
        total_events,
        covered,
        incomplete,
    })
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
    fn push_present_block_coalesces_adjacent_and_opens_gaps() {
        let mut intervals = Vec::new();
        for b in [100, 101, 102] {
            push_present_block(&mut intervals, b);
        }
        // A gap (skip 103, 104) opens a new interval.
        push_present_block(&mut intervals, 105);
        push_present_block(&mut intervals, 106);
        // A duplicate inside the last interval is ignored.
        push_present_block(&mut intervals, 106);
        assert_eq!(intervals, vec![(100, 102), (105, 106)]);
    }

    #[test]
    fn missing_subranges_full_coverage_has_no_gaps() {
        assert!(missing_subranges(100, 110, &[(100, 110)]).is_empty());
    }

    #[test]
    fn missing_subranges_empty_present_is_whole_range() {
        assert_eq!(missing_subranges(100, 110, &[]), vec![(100, 110)]);
    }

    #[test]
    fn missing_subranges_partial_prefix_reports_tail_gap() {
        assert_eq!(missing_subranges(100, 110, &[(100, 105)]), vec![(106, 110)]);
    }

    #[test]
    fn missing_subranges_interior_gap_between_two_present_ranges() {
        assert_eq!(
            missing_subranges(100, 200, &[(100, 120), (160, 200)]),
            vec![(121, 159)]
        );
    }

    #[test]
    fn missing_subranges_leading_and_trailing_gaps() {
        assert_eq!(
            missing_subranges(100, 200, &[(120, 150)]),
            vec![(100, 119), (151, 200)]
        );
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
    fn block_out_of_range_is_outside_range() {
        let pipeline = BlockPipeline::new(vec![make_scope(100, 200)]);
        assert!(matches!(
            pipeline.process(&empty_facts(99)),
            BlockOutcome::OutsideRange
        ));
        assert!(matches!(
            pipeline.process(&empty_facts(201)),
            BlockOutcome::OutsideRange
        ));
    }

    #[test]
    fn bloom_no_match_is_distinguished_from_outside_range() {
        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline = BlockPipeline::new(vec![make_scope_with_target(100, 200, addr, topic0)]);

        // Facts with empty bloom — scanner's address/topic0 are not present.
        let facts = empty_facts(150);
        assert!(matches!(
            pipeline.process(&facts),
            BlockOutcome::NoBloomMatch
        ));
    }

    #[test]
    fn bloom_match_with_unverifiable_receipts_is_verify_failed() {
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
        // Empty receipts against a zero receipts_root: the empty trie root is
        // not B256::ZERO, so the real MerkleVerifier must report VerifyFailed —
        // not silently produce a valid empty evaluation.
        assert!(matches!(
            pipeline.process(&facts),
            BlockOutcome::VerifyFailed
        ));
    }

    #[test]
    fn bloom_match_with_verified_empty_receipts_is_valid_emptiness() {
        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Transfer(address,address,uint256)");
        let pipeline = BlockPipeline::with_verifier(
            vec![make_scope_with_target(100, 200, addr, topic0)],
            Box::new(NoopVerifier),
        );

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
        // A verified block with no matching events is valid emptiness —
        // it must stay eligible for Coverage.
        assert!(matches!(
            pipeline.process(&facts),
            BlockOutcome::Evaluated(events) if events.is_empty()
        ));
    }

    #[test]
    fn merkle_verify_failure_is_verify_failed_not_valid_emptiness() {
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
        assert!(matches!(
            pipeline.process(&facts),
            BlockOutcome::VerifyFailed
        ));
    }

    #[test]
    fn always_fail_verifier_is_verify_failed_for_in_range_bloom_hit() {
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
        assert!(matches!(
            pipeline.process(&facts),
            BlockOutcome::VerifyFailed
        ));
    }

    #[test]
    fn decode_fallback_is_decode_failed_not_clean_evaluation() {
        use crate::abi::EventInput;
        use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom};
        use alloy_primitives::{Bytes, Log as PrimitiveLog, LogData};

        let addr = Address::repeat_byte(0xAB);
        let input = |name: &str, ty: &str, indexed: bool| EventInput {
            name: name.to_string(),
            ty: ty.to_string(),
            indexed,
            components: vec![],
        };
        let transfer = EventAbi {
            name: "Transfer".to_string(),
            inputs: vec![
                input("from", "address", true),
                input("to", "address", true),
                input("value", "uint256", false),
            ],
        };
        let topic0 = transfer.topic0();
        let scope = PreparedScope {
            contract: addr.to_checksum(None),
            name: "test".to_string(),
            from: 100,
            to: 200,
            scanner: BloomScanner::new(&[topic0], addr),
            decoder: EventDecoder::new(&[transfer], addr).unwrap(),
        };
        let pipeline = BlockPipeline::with_verifier(vec![scope], Box::new(NoopVerifier));

        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(addr.as_slice()));
        bloom.accrue(BloomInput::Raw(topic0.as_slice()));

        // 3 bytes of data cannot decode as the ABI's non-indexed uint256.
        let log = PrimitiveLog {
            address: addr,
            data: LogData::new_unchecked(
                vec![topic0, B256::from([0x11; 32]), B256::from([0x22; 32])],
                Bytes::from(vec![0xDE, 0xAD, 0xBE]),
            ),
        };
        let receipt = ReceiptEnvelope::Legacy(ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![log],
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
        assert!(matches!(
            pipeline.process(&facts),
            BlockOutcome::DecodeFailed(events)
                if events.len() == 1 && events[0].has_decode_error()
        ));
    }

    #[test]
    fn no_scopes_is_outside_range() {
        let pipeline = BlockPipeline::new(vec![]);
        assert!(matches!(
            pipeline.process(&empty_facts(100)),
            BlockOutcome::OutsideRange
        ));
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
