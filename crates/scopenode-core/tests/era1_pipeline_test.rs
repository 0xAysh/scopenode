use alloy_consensus::{Eip658Value, Header, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{
    address, keccak256, Address, Bloom, BloomInput, Bytes, Log as PrimitiveLog, LogData, B256,
};
use alloy_rlp::{Encodable, Header as RlpHeader};
use alloy_trie::{HashBuilder, Nibbles};
use alloy::eips::eip2718::Encodable2718;
use scopenode_core::{
    abi::AbiCache,
    config::{Config, ContractConfig, NodeConfig, SourceConfig, SourceKind},
    era_pipeline::run_era1_scope,
    source::{ChecksumStatus, RangeCompleteness, SourceFileManifest, SourceRangeManifest},
};
use scopenode_storage::Db;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use tempfile::tempdir;

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
        let h = RlpHeader { list: true, payload_length: 0 };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        buf
    };
    let body_payload_len = empty_list.len() + empty_list.len();
    let body_outer = RlpHeader { list: true, payload_length: body_payload_len };
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
    index_data.extend_from_slice(&0i64.to_le_bytes());   // offset for block 100
    index_data.extend_from_slice(&1i64.to_le_bytes());   // count

    // Assemble ERA1 file
    let mut file = Vec::new();
    file.extend(e2store_entry([0x65, 0x32], &[]));                   // version
    file.extend(e2store_entry([0x03, 0x00], &compressed_header));    // header
    file.extend(e2store_entry([0x04, 0x00], &compressed_body));      // body
    file.extend(e2store_entry([0x05, 0x00], &compressed_receipts));  // receipts
    file.extend(e2store_entry([0x06, 0x00], &td));                   // total difficulty
    file.extend(e2store_entry([0x66, 0x32], &index_data));           // block index
    file
}

/// ABI JSON in the cache format used by AbiCache::get_or_fetch.
/// This is NOT the raw Ethereum ABI format — it uses {name, inputs} objects
/// matching parse_cached_events expectations.
fn transfer_abi_json() -> String {
    r#"[{"name":"Transfer","inputs":[
        {"name":"from","type":"address","indexed":true,"components":[]},
        {"name":"to","type":"address","indexed":true,"components":[]},
        {"name":"value","type":"uint256","indexed":false,"components":[]}
    ]}]"#.to_string()
}

fn test_config(contract_address: Address) -> Config {
    Config {
        node: NodeConfig {
            port: 18545,
            data_dir: None,
        },
        source: Some(SourceConfig {
            kind: SourceKind::Era1,
            path: PathBuf::from("fixtures/era1/mainnet"),
            network: Some("mainnet".into()),
        }),
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

    // Pre-cache ABI (skips Sourcify fetch)
    let addr_str = contract.to_checksum(None);
    db.upsert_contract(&addr_str, Some("USDT"), &transfer_abi_json())
        .await
        .unwrap();

    // Build manifest pointing at the synthetic file
    let files = vec![SourceFileManifest {
        format: "era1".into(),
        path: era1_path.clone(),
        filename: "mainnet-00012-deadbeef.era1".into(),
        network: "mainnet".into(),
        epoch: 12,
        file_hash: "deadbeef".into(),
        size_bytes: era1_bytes.len() as u64,
        modified_at: None,
        sha256: "".into(),
        checksum_status: ChecksumStatus::Unavailable,
        ranges: vec![SourceRangeManifest {
            from_block: 100,
            to_block: 100,
            completeness: RangeCompleteness::FileIndex,
        }],
    }];

    let config = test_config(contract);
    let contract_cfg = &config.contracts[0];
    let progress = indicatif::MultiProgress::new();
    let mut abi_cache = AbiCache::new(db.clone());

    run_era1_scope(&files, contract_cfg, &mut abi_cache, &db, &progress)
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
