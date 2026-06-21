use alloy::eips::eip2718::Encodable2718;
use alloy_consensus::{Eip658Value, Header, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{
    address, keccak256, Address, Bloom, BloomInput, Bytes, Log as PrimitiveLog, LogData, B256,
};
use alloy_rlp::{Encodable, Header as RlpHeader};
use alloy_trie::{HashBuilder, Nibbles};
use async_trait::async_trait;
use scopenode_core::{
    abi::DecodedEvent,
    abi_resolution::{AbiResolver, AbiStore},
    config::{Config, ContractConfig, NodeConfig},
    era1_reader::Era1BlockFacts,
    era_pipeline::{
        run_era1_scope, run_era1_scopes, run_scopes_over_blocks, BlockFactStream, CoverageSink,
        EventSink, InMemoryEventSink, IncompleteReason, NullReporter,
    },
    error::{AbiError, CoreError},
    source::{Era1Source, SourceError},
    types::ScopeHeader,
};
use std::collections::HashMap;
use scopenode_storage::{Db, DbAbiStore, DbEventSink};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Mutex;

#[derive(Default)]
struct FailingStoreSink {
    covered_ranges: Mutex<Vec<(String, u64, u64)>>,
}

#[async_trait]
impl EventSink for FailingStoreSink {
    async fn store(&self, _events: Vec<DecodedEvent>) -> Result<usize, CoreError> {
        Err(CoreError::Storage("synthetic insert failure".to_string()))
    }
}

#[async_trait]
impl CoverageSink for FailingStoreSink {
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

/// Sink that persists events but fails to persist Coverage.
#[derive(Default)]
struct FailingCoverageSink {
    events: Mutex<Vec<DecodedEvent>>,
}

#[async_trait]
impl EventSink for FailingCoverageSink {
    async fn store(&self, events: Vec<DecodedEvent>) -> Result<usize, CoreError> {
        let count = events.len();
        self.events.lock().await.extend(events);
        Ok(count)
    }
}

#[async_trait]
impl CoverageSink for FailingCoverageSink {
    async fn record_coverage(
        &self,
        _contract: &str,
        _from_block: u64,
        _to_block: u64,
    ) -> Result<(), CoreError> {
        Err(CoreError::Storage(
            "synthetic coverage write failure".to_string(),
        ))
    }
}

/// ABI store stub with per-address entries — used in tests that need different
/// ABIs for different contracts in the same pipeline run.
struct MapAbiStore(HashMap<String, String>);

#[async_trait]
impl AbiStore for MapAbiStore {
    async fn load(&self, address: &str) -> Result<Option<String>, AbiError> {
        Ok(self.0.get(address).cloned())
    }
    async fn save(&self, _: &str, _: Option<&str>, _: &str) -> Result<(), AbiError> {
        Ok(())
    }
}

/// ABI store stub returning a fixed cached ABI — no SQLite needed.
struct StaticAbiStore(String);

#[async_trait]
impl AbiStore for StaticAbiStore {
    async fn load(&self, _address: &str) -> Result<Option<String>, AbiError> {
        Ok(Some(self.0.clone()))
    }
    async fn save(
        &self,
        _address: &str,
        _name: Option<&str>,
        _abi_json: &str,
    ) -> Result<(), AbiError> {
        Ok(())
    }
}

/// In-memory Block fact stream — the second adapter for the pipeline's
/// traversal seam, so failure paths need no real archive files.
struct StaticBlockFacts {
    total: u64,
    items: std::sync::Mutex<Vec<Result<Era1BlockFacts, SourceError>>>,
}

impl BlockFactStream for StaticBlockFacts {
    fn total_blocks(&self) -> u64 {
        self.total
    }
    fn blocks(&self) -> Box<dyn Iterator<Item = Result<Era1BlockFacts, SourceError>> + '_> {
        let items: Vec<_> = self.items.lock().unwrap().drain(..).collect();
        Box::new(items.into_iter())
    }
}

fn eventless_block_facts(block_number: u64) -> Era1BlockFacts {
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

#[tokio::test]
async fn pipeline_records_coverage_over_in_memory_block_fact_stream() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let addr_str = contract.to_checksum(None);

    let stream = StaticBlockFacts {
        total: 1,
        items: std::sync::Mutex::new(vec![Ok(eventless_block_facts(100))]),
    };
    let config = test_config(contract);
    let abi_resolver = AbiResolver::new(Arc::new(StaticAbiStore(transfer_abi_json())), None);
    let sink = InMemoryEventSink::default();

    let report = run_scopes_over_blocks(
        &stream,
        &config.contracts,
        &abi_resolver,
        &sink,
        &NullReporter,
    )
    .await
    .unwrap();

    assert!(report.is_complete());
    assert_eq!(report.total_events, 0);
    assert_eq!(report.covered, vec![addr_str.clone()]);
    assert_eq!(sink.covered_ranges().await, vec![(addr_str, 100, 100)]);
}

/// Config with a custom block range, for coverage-correctness tests that need
/// a range wider than a single block.
fn test_config_range(contract_address: Address, from_block: u64, to_block: u64) -> Config {
    Config {
        node: NodeConfig {
            port: 18545,
            rest_port: 8546,
            data_dir: None,
            era_dir: PathBuf::from("/tmp/era1"),
        },
        contracts: vec![ContractConfig {
            name: Some("USDT".into()),
            address: contract_address,
            events: vec!["Transfer".into()],
            from_block,
            to_block: Some(to_block),
            abi_override: None,
            impl_address: None,
        }],
    }
}

fn block_stream(blocks: std::ops::RangeInclusive<u64>) -> StaticBlockFacts {
    let items: Vec<_> = blocks.clone().map(|n| Ok(eventless_block_facts(n))).collect();
    StaticBlockFacts {
        total: (blocks.end() - blocks.start() + 1),
        items: std::sync::Mutex::new(items),
    }
}

#[tokio::test]
async fn pipeline_zero_blocks_present_reports_no_source_data_without_coverage() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let addr_str = contract.to_checksum(None);

    // Requested range [100, 110], but the source streams no blocks at all
    // (empty era_dir / missing epoch). Coverage must NOT be recorded.
    let stream = StaticBlockFacts {
        total: 0,
        items: std::sync::Mutex::new(vec![]),
    };
    let config = test_config_range(contract, 100, 110);
    let abi_resolver = AbiResolver::new(Arc::new(StaticAbiStore(transfer_abi_json())), None);
    let sink = InMemoryEventSink::default();

    let report = run_scopes_over_blocks(
        &stream,
        &config.contracts,
        &abi_resolver,
        &sink,
        &NullReporter,
    )
    .await
    .unwrap();

    assert!(
        !report.is_complete(),
        "a scope with no backing source data must not report complete"
    );
    assert!(report.covered.is_empty());
    assert_eq!(
        report.incomplete,
        vec![(
            addr_str,
            IncompleteReason::NoSourceData {
                from_block: 100,
                to_block: 110,
            }
        )]
    );
    assert!(
        sink.covered_ranges().await.is_empty(),
        "no coverage row may be written for blocks that were never read"
    );
}

#[tokio::test]
async fn pipeline_partial_overlap_covers_verified_subrange_and_reports_gap() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let addr_str = contract.to_checksum(None);

    // Requested [100, 110]; the source only has [100, 105].
    let stream = block_stream(100..=105);
    let config = test_config_range(contract, 100, 110);
    let abi_resolver = AbiResolver::new(Arc::new(StaticAbiStore(transfer_abi_json())), None);
    let sink = InMemoryEventSink::default();

    let report = run_scopes_over_blocks(
        &stream,
        &config.contracts,
        &abi_resolver,
        &sink,
        &NullReporter,
    )
    .await
    .unwrap();

    assert!(!report.is_complete(), "a partially-sourced scope is incomplete");
    assert!(report.covered.is_empty());
    assert_eq!(
        report.incomplete,
        vec![(
            addr_str.clone(),
            IncompleteReason::NoSourceData {
                from_block: 106,
                to_block: 110,
            }
        )],
        "the gap [106, 110] must be reported as the missing sub-range"
    );
    assert_eq!(
        sink.covered_ranges().await,
        vec![(addr_str, 100, 105)],
        "only the verified sub-range [100, 105] earns coverage"
    );
}

#[tokio::test]
async fn pipeline_full_overlap_records_full_coverage() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let addr_str = contract.to_checksum(None);

    let stream = block_stream(100..=110);
    let config = test_config_range(contract, 100, 110);
    let abi_resolver = AbiResolver::new(Arc::new(StaticAbiStore(transfer_abi_json())), None);
    let sink = InMemoryEventSink::default();

    let report = run_scopes_over_blocks(
        &stream,
        &config.contracts,
        &abi_resolver,
        &sink,
        &NullReporter,
    )
    .await
    .unwrap();

    assert!(report.is_complete());
    assert_eq!(report.covered, vec![addr_str.clone()]);
    assert_eq!(sink.covered_ranges().await, vec![(addr_str, 100, 110)]);
}

#[tokio::test]
async fn pipeline_rerun_with_filled_gap_converges_to_full_coverage() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let addr_str = contract.to_checksum(None);

    let config = test_config_range(contract, 100, 110);
    let abi_resolver = AbiResolver::new(Arc::new(StaticAbiStore(transfer_abi_json())), None);
    let sink = InMemoryEventSink::default();

    // First run: only [100, 105] available → incomplete, partial coverage.
    let first = run_scopes_over_blocks(
        &block_stream(100..=105),
        &config.contracts,
        &abi_resolver,
        &sink,
        &NullReporter,
    )
    .await
    .unwrap();
    assert!(!first.is_complete());

    // Second run after the missing epoch is added → the full range streams.
    let second = run_scopes_over_blocks(
        &block_stream(100..=110),
        &config.contracts,
        &abi_resolver,
        &sink,
        &NullReporter,
    )
    .await
    .unwrap();

    assert!(
        second.is_complete(),
        "a rerun with the gap filled must converge to complete coverage"
    );
    assert_eq!(second.covered, vec![addr_str.clone()]);
    assert!(
        sink.covered_ranges().await.contains(&(addr_str, 100, 110)),
        "the rerun records a coverage row spanning the full requested range"
    );
}

#[tokio::test]
async fn pipeline_reports_source_failure_from_in_memory_block_fact_stream() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let addr_str = contract.to_checksum(None);

    let stream = StaticBlockFacts {
        total: 1,
        items: std::sync::Mutex::new(vec![Err(SourceError::InvalidE2Store {
            path: PathBuf::from("/era/mainnet-00000.era1"),
            message: "synthetic mid-stream read failure".into(),
        })]),
    };
    let config = test_config(contract);
    let abi_resolver = AbiResolver::new(Arc::new(StaticAbiStore(transfer_abi_json())), None);
    let sink = InMemoryEventSink::default();

    let report = run_scopes_over_blocks(
        &stream,
        &config.contracts,
        &abi_resolver,
        &sink,
        &NullReporter,
    )
    .await
    .unwrap();

    assert!(!report.is_complete());
    assert_eq!(
        report.incomplete,
        vec![(addr_str, IncompleteReason::SourceFailure)]
    );
    assert!(sink.covered_ranges().await.is_empty());
}

fn unique_db_path() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "scopenode_era1_e2e_{}_{}.db",
        std::process::id(),
        n
    ))
}

fn snappy_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = snap::write::FrameEncoder::new(&mut out);
        enc.write_all(data).unwrap();
        enc.flush().unwrap();
    }
    out
}

fn e2store_entry(entry_type: [u8; 2], data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + data.len());
    bytes.extend_from_slice(&entry_type);
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(data);
    bytes
}

fn rlp_encode_index(i: usize) -> Vec<u8> {
    if i == 0 {
        vec![0x80]
    } else if i < 0x80 {
        vec![i as u8]
    } else {
        let bytes = i.to_be_bytes();
        let nz = bytes.iter().position(|&b| b != 0).unwrap_or(7);
        let trimmed = &bytes[nz..];
        let mut out = vec![0x80 + trimmed.len() as u8];
        out.extend_from_slice(trimmed);
        out
    }
}

fn build_synthetic_era1_with_logs(logs: Vec<PrimitiveLog>, bloom_inputs: Vec<Vec<u8>>) -> Vec<u8> {
    build_synthetic_era1_with_logs_and_root(logs, bloom_inputs, None)
}

fn build_synthetic_era1_with_logs_and_root(
    logs: Vec<PrimitiveLog>,
    bloom_inputs: Vec<Vec<u8>>,
    receipts_root_override: Option<B256>,
) -> Vec<u8> {
    let mut bloom = Bloom::default();
    for input in bloom_inputs {
        bloom.accrue(BloomInput::Raw(&input));
    }

    let receipt_body = ReceiptWithBloom::<Receipt<PrimitiveLog>> {
        receipt: Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 21_000,
            logs,
        },
        logs_bloom: bloom,
    };

    let envelope = ReceiptEnvelope::<PrimitiveLog>::Legacy(receipt_body.clone());
    let key = rlp_encode_index(0);
    let mut value = Vec::new();
    envelope.encode_2718(&mut value);
    let mut hb = HashBuilder::default();
    hb.add_leaf(Nibbles::unpack(&key), &value);
    let receipts_root = receipts_root_override.unwrap_or_else(|| hb.root());

    let header = Header {
        number: 100,
        parent_hash: B256::ZERO,
        receipts_root,
        logs_bloom: bloom,
        timestamp: 1_000_000,
        gas_used: 21_000,
        ..Default::default()
    };

    let mut header_rlp = Vec::new();
    header.encode(&mut header_rlp);
    let compressed_header = snappy_compress(&header_rlp);

    let empty_list: Vec<u8> = {
        let h = RlpHeader {
            list: true,
            payload_length: 0,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        buf
    };
    let body_payload_len = empty_list.len() + empty_list.len();
    let body_outer = RlpHeader {
        list: true,
        payload_length: body_payload_len,
    };
    let mut body_buf = Vec::new();
    body_outer.encode(&mut body_buf);
    body_buf.extend_from_slice(&empty_list);
    body_buf.extend_from_slice(&empty_list);
    let compressed_body = snappy_compress(&body_buf);

    let mut receipt_item = Vec::new();
    receipt_body.encode(&mut receipt_item);
    let outer_h = RlpHeader {
        list: true,
        payload_length: receipt_item.len(),
    };
    let mut receipts_buf = Vec::new();
    outer_h.encode(&mut receipts_buf);
    receipts_buf.extend_from_slice(&receipt_item);
    let compressed_receipts = snappy_compress(&receipts_buf);

    let td = [0u8; 32];
    let mut index_data = Vec::new();
    index_data.extend_from_slice(&100u64.to_le_bytes());
    index_data.extend_from_slice(&0i64.to_le_bytes());
    index_data.extend_from_slice(&1i64.to_le_bytes());

    let mut file = Vec::new();
    file.extend(e2store_entry([0x65, 0x32], &[]));
    file.extend(e2store_entry([0x03, 0x00], &compressed_header));
    file.extend(e2store_entry([0x04, 0x00], &compressed_body));
    file.extend(e2store_entry([0x05, 0x00], &compressed_receipts));
    file.extend(e2store_entry([0x06, 0x00], &td));
    file.extend(e2store_entry([0x66, 0x32], &index_data));
    file
}

fn build_synthetic_era1(contract: Address, transfer_topic0: B256) -> Vec<u8> {
    // Build the log
    let log = PrimitiveLog {
        address: contract,
        data: LogData::new_unchecked(
            vec![
                transfer_topic0,
                B256::from([0x11u8; 32]), // from (indexed)
                B256::from([0x22u8; 32]), // to (indexed)
            ],
            Bytes::from(vec![0u8; 32]), // value (32 zero bytes)
        ),
    };

    // Build receipt with bloom containing contract address and topic0
    let mut bloom = Bloom::default();
    bloom.accrue(BloomInput::Raw(contract.as_slice()));
    bloom.accrue(BloomInput::Raw(transfer_topic0.as_slice()));

    let receipt_body = ReceiptWithBloom::<Receipt<PrimitiveLog>> {
        receipt: Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 21_000,
            logs: vec![log],
        },
        logs_bloom: bloom,
    };

    // Compute receipts_root: MPT of EIP-2718 encoded receipts.
    // Must encode exactly as ReceiptEnvelope::Legacy wrapping ReceiptWithBloom<Receipt<PrimitiveLog>>
    // since that is what decode_era1_receipts produces and verify_era1_receipts uses.
    let envelope = ReceiptEnvelope::<PrimitiveLog>::Legacy(receipt_body.clone());
    let key = rlp_encode_index(0);
    let mut value = Vec::new();
    envelope.encode_2718(&mut value);
    let mut hb = HashBuilder::default();
    hb.add_leaf(Nibbles::unpack(&key), &value);
    let receipts_root = hb.root();

    // Build header with matching bloom and receipts_root
    let header = Header {
        number: 100,
        parent_hash: B256::ZERO,
        receipts_root,
        logs_bloom: bloom,
        timestamp: 1_000_000,
        gas_used: 21_000,
        ..Default::default()
    };

    // Encode header as snappy(RLP(header))
    let mut header_rlp = Vec::new();
    header.encode(&mut header_rlp);
    let compressed_header = snappy_compress(&header_rlp);

    // Encode body as snappy(RLP([[],[]])) — empty txs and uncles
    let empty_list: Vec<u8> = {
        let h = RlpHeader {
            list: true,
            payload_length: 0,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        buf
    };
    let body_payload_len = empty_list.len() + empty_list.len();
    let body_outer = RlpHeader {
        list: true,
        payload_length: body_payload_len,
    };
    let mut body_buf = Vec::new();
    body_outer.encode(&mut body_buf);
    body_buf.extend_from_slice(&empty_list);
    body_buf.extend_from_slice(&empty_list);
    let compressed_body = snappy_compress(&body_buf);

    // Encode receipts as snappy(RLP([receipt_rlp]))
    let mut receipt_item = Vec::new();
    receipt_body.encode(&mut receipt_item);
    let outer_h = RlpHeader {
        list: true,
        payload_length: receipt_item.len(),
    };
    let mut receipts_buf = Vec::new();
    outer_h.encode(&mut receipts_buf);
    receipts_buf.extend_from_slice(&receipt_item);
    let compressed_receipts = snappy_compress(&receipts_buf);

    // Total difficulty (dummy 32 bytes)
    let td = [0u8; 32];

    // Block index: starting_number=100, one offset entry=0, count=1
    // Format: u64 starting_number + [i64 offsets per block] + i64 count
    let mut index_data = Vec::new();
    index_data.extend_from_slice(&100u64.to_le_bytes()); // starting_number
    index_data.extend_from_slice(&0i64.to_le_bytes()); // offset for block 100
    index_data.extend_from_slice(&1i64.to_le_bytes()); // count

    // Assemble ERA1 file
    let mut file = Vec::new();
    file.extend(e2store_entry([0x65, 0x32], &[])); // version
    file.extend(e2store_entry([0x03, 0x00], &compressed_header)); // header
    file.extend(e2store_entry([0x04, 0x00], &compressed_body)); // body
    file.extend(e2store_entry([0x05, 0x00], &compressed_receipts)); // receipts
    file.extend(e2store_entry([0x06, 0x00], &td)); // total difficulty
    file.extend(e2store_entry([0x66, 0x32], &index_data)); // block index
    file
}

/// ABI JSON in the pinned ABI cache format (see `abi_resolution::parse_event_entries`).
fn transfer_abi_json() -> String {
    r#"[{"type":"event","name":"Transfer","inputs":[
        {"name":"from","type":"address","indexed":true,"components":[]},
        {"name":"to","type":"address","indexed":true,"components":[]},
        {"name":"value","type":"uint256","indexed":false,"components":[]}
    ]}]"#
        .to_string()
}

fn test_config(contract_address: Address) -> Config {
    Config {
        node: NodeConfig {
            port: 18545,
            rest_port: 8546,
            data_dir: None,
            era_dir: PathBuf::from("/tmp/era1"),
        },
        contracts: vec![ContractConfig {
            name: Some("USDT".into()),
            address: contract_address,
            events: vec!["Transfer".into()],
            from_block: 100,
            to_block: Some(100),
            abi_override: None,
            impl_address: None,
        }],
    }
}

#[tokio::test]
async fn era1_pipeline_indexes_transfer_event() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");

    // Build synthetic ERA1 file
    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");
    let era1_bytes = build_synthetic_era1(contract, transfer_topic0);
    std::fs::write(&era1_path, &era1_bytes).unwrap();

    // Set up DB
    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();

    // Pre-cache ABI so the test does not need a local override file.
    let addr_str = contract.to_checksum(None);
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();

    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = DbEventSink::new(db.clone());

    run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    // Assert exactly one Transfer event was stored
    let count = db.count_events_for_contract(&addr_str).await.unwrap();
    assert_eq!(count, 1, "expected exactly one Transfer event in DB");

    // Cleanup
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_does_not_record_coverage_when_event_store_fails() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");

    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");
    let era1_bytes = build_synthetic_era1(contract, transfer_topic0);
    std::fs::write(&era1_path, &era1_bytes).unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    let addr_str = contract.to_checksum(None);
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = FailingStoreSink::default();

    run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert!(
        sink.covered_ranges.lock().await.is_empty(),
        "failed event storage must not record Coverage for the Contract scope"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_does_not_record_coverage_when_selected_file_cannot_open() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");

    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");
    let era1_bytes = build_synthetic_era1(contract, transfer_topic0);
    std::fs::write(&era1_path, &era1_bytes).unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    let addr_str = contract.to_checksum(None);
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    std::fs::remove_file(&era1_path).unwrap();

    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = InMemoryEventSink::default();

    run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert!(
        sink.covered_ranges().await.is_empty(),
        "unopened ERA1 source files must not record Coverage for the Contract scope"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_does_not_record_coverage_when_receipt_verification_fails() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");

    // Bloom matches the contract scope, but the header carries a receipts_root
    // that cannot match the receipts — Receipt verification must fail.
    let log = PrimitiveLog {
        address: contract,
        data: LogData::new_unchecked(
            vec![
                transfer_topic0,
                B256::from([0x11u8; 32]),
                B256::from([0x22u8; 32]),
            ],
            Bytes::from(vec![0u8; 32]),
        ),
    };
    let era1_bytes = build_synthetic_era1_with_logs_and_root(
        vec![log],
        vec![
            contract.as_slice().to_vec(),
            transfer_topic0.as_slice().to_vec(),
        ],
        Some(B256::from([0xAA; 32])),
    );

    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");
    std::fs::write(&era1_path, &era1_bytes).unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    let addr_str = contract.to_checksum(None);
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = InMemoryEventSink::default();

    run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert!(
        sink.events().await.is_empty(),
        "unverified receipts must not produce stored events"
    );
    assert!(
        sink.covered_ranges().await.is_empty(),
        "Receipt verification failure must not record Coverage for the Contract scope"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_stores_events_but_withholds_coverage_when_decode_fails() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");

    // Receipts are internally consistent (verification passes), but the log
    // data is 3 bytes — it cannot decode as the ABI's non-indexed uint256.
    let log = PrimitiveLog {
        address: contract,
        data: LogData::new_unchecked(
            vec![
                transfer_topic0,
                B256::from([0x11u8; 32]),
                B256::from([0x22u8; 32]),
            ],
            Bytes::from(vec![0xDE, 0xAD, 0xBE]),
        ),
    };
    let era1_bytes = build_synthetic_era1_with_logs(
        vec![log],
        vec![
            contract.as_slice().to_vec(),
            transfer_topic0.as_slice().to_vec(),
        ],
    );

    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");
    std::fs::write(&era1_path, &era1_bytes).unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    let addr_str = contract.to_checksum(None);
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = InMemoryEventSink::default();

    run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    // The lossy event is still stored — raw topics and data are preserved.
    let events = sink.events().await;
    assert_eq!(events.len(), 1, "lossy decoded event must still be stored");
    assert!(
        events[0].decoded.get("_decode_error").is_some(),
        "stored event must carry its decode error marker"
    );
    // But the Contract scope must not earn Coverage for the attempted range.
    assert!(
        sink.covered_ranges().await.is_empty(),
        "decode failure must not record Coverage for the Contract scope"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_report_distinguishes_complete_and_incomplete_runs() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");
    let addr_str = contract.to_checksum(None);

    // Clean run → complete report with the scope covered.
    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");
    std::fs::write(&era1_path, build_synthetic_era1(contract, transfer_topic0)).unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = InMemoryEventSink::default();

    let report = run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert!(report.is_complete(), "clean run must report complete");
    assert_eq!(report.total_events, 1);
    assert_eq!(report.covered, vec![addr_str.clone()]);
    assert!(report.incomplete.is_empty());

    // Corrupt receipts_root → incomplete report naming the scope and reason.
    let log = PrimitiveLog {
        address: contract,
        data: LogData::new_unchecked(
            vec![
                transfer_topic0,
                B256::from([0x11u8; 32]),
                B256::from([0x22u8; 32]),
            ],
            Bytes::from(vec![0u8; 32]),
        ),
    };
    std::fs::write(
        &era1_path,
        build_synthetic_era1_with_logs_and_root(
            vec![log],
            vec![
                contract.as_slice().to_vec(),
                transfer_topic0.as_slice().to_vec(),
            ],
            Some(B256::from([0xAA; 32])),
        ),
    )
    .unwrap();
    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let sink = InMemoryEventSink::default();

    let report = run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert!(
        !report.is_complete(),
        "verification failure must report incomplete"
    );
    assert!(report.covered.is_empty());
    assert_eq!(
        report.incomplete,
        vec![(addr_str.clone(), IncompleteReason::VerifyFailure)]
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_records_coverage_for_valid_eventless_range() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let addr_str = contract.to_checksum(None);

    // A consistent block with no logs at all: no Bloom match for the scope,
    // nothing to verify or decode — valid emptiness, eligible for Coverage.
    let era1_bytes = build_synthetic_era1_with_logs(vec![], vec![]);

    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("mainnet-00012-deadbeef.era1"), &era1_bytes).unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = InMemoryEventSink::default();

    let report = run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert!(
        report.is_complete(),
        "an eventless clean run must report complete"
    );
    assert_eq!(report.total_events, 0);
    assert!(sink.events().await.is_empty());
    assert_eq!(
        sink.covered_ranges().await,
        vec![(addr_str.clone(), 100, 100)],
        "valid emptiness must remain eligible for Coverage"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_reports_coverage_write_failure_as_incomplete() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");
    let addr_str = contract.to_checksum(None);

    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");
    std::fs::write(&era1_path, build_synthetic_era1(contract, transfer_topic0)).unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = FailingCoverageSink::default();

    let report = run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert!(
        !report.is_complete(),
        "a failed Coverage write must not be reported as an unqualified success"
    );
    assert_eq!(
        report.incomplete,
        vec![(addr_str.clone(), IncompleteReason::CoverageWriteFailed)]
    );
    assert_eq!(
        sink.events.lock().await.len(),
        1,
        "events persisted before the Coverage write failure remain stored"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_rerun_recovers_coverage_after_prior_verify_failure() {
    let contract = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");
    let addr_str = contract.to_checksum(None);

    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");

    // First run: corrupt receipts_root → no Coverage.
    let log = PrimitiveLog {
        address: contract,
        data: LogData::new_unchecked(
            vec![
                transfer_topic0,
                B256::from([0x11u8; 32]),
                B256::from([0x22u8; 32]),
            ],
            Bytes::from(vec![0u8; 32]),
        ),
    };
    std::fs::write(
        &era1_path,
        build_synthetic_era1_with_logs_and_root(
            vec![log],
            vec![
                contract.as_slice().to_vec(),
                transfer_topic0.as_slice().to_vec(),
            ],
            Some(B256::from([0xAA; 32])),
        ),
    )
    .unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = InMemoryEventSink::default();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let report = run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();
    assert!(!report.is_complete());
    assert!(sink.covered_ranges().await.is_empty());

    // Second run with repaired archive data: the scope recovers Coverage.
    std::fs::write(&era1_path, build_synthetic_era1(contract, transfer_topic0)).unwrap();
    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();
    let report = run_era1_scope(&source, contract_cfg, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert!(
        report.is_complete(),
        "a successful rerun must recover from a prior incomplete run"
    );
    assert_eq!(
        sink.covered_ranges().await,
        vec![(addr_str.clone(), 100, 100)]
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_indexes_multiple_contracts_in_one_scope_pass() {
    let first = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    let second = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let transfer_topic0 = keccak256(b"Transfer(address,address,uint256)");

    let make_log = |contract: Address, marker: u8| PrimitiveLog {
        address: contract,
        data: LogData::new_unchecked(
            vec![
                transfer_topic0,
                B256::from([marker; 32]),
                B256::from([marker + 1; 32]),
            ],
            Bytes::from(vec![0u8; 32]),
        ),
    };

    let era1_bytes = build_synthetic_era1_with_logs(
        vec![make_log(first, 0x11), make_log(second, 0x33)],
        vec![
            first.as_slice().to_vec(),
            second.as_slice().to_vec(),
            transfer_topic0.as_slice().to_vec(),
        ],
    );

    let dir = tempdir().unwrap();
    let era1_path = dir.path().join("mainnet-00012-deadbeef.era1");
    std::fs::write(&era1_path, &era1_bytes).unwrap();

    let db_path = unique_db_path();
    let db = Db::open(db_path.clone()).await.unwrap();
    db.upsert_contract(&first.to_checksum(None), Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();
    db.upsert_contract(
        &second.to_checksum(None),
        Some("USDC"),
        &transfer_abi_json(),
    )
    .await
    .unwrap();

    let source = Era1Source::scan(dir.path(), None, 100, 100).unwrap();

    let contracts = vec![
        test_config(first).contracts.remove(0),
        test_config(second).contracts.remove(0),
    ];
    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), None);
    let sink = DbEventSink::new(db.clone());

    run_era1_scopes(&source, &contracts, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert_eq!(
        db.count_events_for_contract(&first.to_checksum(None))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        db.count_events_for_contract(&second.to_checksum(None))
            .await
            .unwrap(),
        1
    );
    let outcome = db
        .query_events(&scopenode_storage::EventQuery {
            contract: Some(first.to_checksum(None)),
            from_block: Some(100),
            to_block: Some(100),
            limit: 100,
            ..scopenode_storage::EventQuery::default()
        })
        .await
        .unwrap();
    assert!(
        matches!(outcome, scopenode_storage::EventQueryOutcome::Results(rows) if rows.len() == 1),
        "successful sync should record coverage for the processed range"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn era1_pipeline_scope_preparation_failure_reported_in_incomplete() {
    // Contract A: stale ABI (no "type":"event") + no fetcher → AbiRequired → prep failure.
    // Contract B: canonical ABI → prep succeeds.
    // Expected: report.incomplete contains Contract A with PreparationFailed.
    let addr_a = address!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let addr_b = address!("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");

    let stale_abi = r#"[{"name":"Transfer","inputs":[]}]"#;
    let canonical_abi = r#"[{"type":"event","name":"Transfer","inputs":[]}]"#;

    let mut map = HashMap::new();
    map.insert(addr_a.to_checksum(None), stale_abi.to_string());
    map.insert(addr_b.to_checksum(None), canonical_abi.to_string());

    let store = Arc::new(MapAbiStore(map));
    let abi_resolver = AbiResolver::new(store, None);

    let contracts = vec![
        ContractConfig {
            name: Some("ContractA".to_string()),
            address: addr_a,
            events: vec!["Transfer".to_string()],
            from_block: 1,
            to_block: Some(10),
            abi_override: None,
            impl_address: None,
        },
        ContractConfig {
            name: Some("ContractB".to_string()),
            address: addr_b,
            events: vec!["Transfer".to_string()],
            from_block: 1,
            to_block: Some(10),
            abi_override: None,
            impl_address: None,
        },
    ];

    // The full [1, 10] range is present in the source, so ContractB earns
    // coverage and the only incompleteness is ContractA's preparation failure.
    let blocks = block_stream(1..=10);
    let sink = InMemoryEventSink::default();

    let report = run_scopes_over_blocks(&blocks, &contracts, &abi_resolver, &sink, &NullReporter)
        .await
        .unwrap();

    assert_eq!(
        report.covered,
        vec![addr_b.to_checksum(None)],
        "ContractB should be covered"
    );
    assert_eq!(report.incomplete.len(), 1, "ContractA should appear in incomplete");
    assert_eq!(report.incomplete[0].0, "ContractA");
    assert!(
        matches!(report.incomplete[0].1, IncompleteReason::PreparationFailed),
        "reason should be PreparationFailed"
    );
}
