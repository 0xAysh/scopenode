//! High-level ERA1 block reader.
//!
//! Wraps the low-level [`crate::source`] primitives and exposes decoded block
//! facts to callers, hiding `Era1BlockTuple`, `decode_era1_header`,
//! `decode_era1_receipts`, and `decode_era1_tx_hashes` behind the
//! [`iter_era1_block_facts`] function.

use crate::e2store::iter_era1_block_tuples;
use crate::era1_codec::decode_ere_slim_receipts;
use crate::source::{decode_era1_header, decode_era1_receipts, decode_era1_tx_hashes, SourceError};
use crate::types::ScopeHeader;
use alloy_consensus::ReceiptEnvelope;
use alloy_primitives::{Log as PrimitiveLog, B256};
use std::path::Path;

/// Decoded facts extracted from a single ERA1 block entry.
pub struct Era1BlockFacts {
    /// Canonical block number.
    pub block_number: u64,

    /// Decoded block header (number, hash, bloom, receipts_root, …).
    pub header: ScopeHeader,

    /// All receipts in this block, decoded from the compressed body.
    pub receipts: Vec<ReceiptEnvelope<PrimitiveLog>>,

    /// Transaction hashes computed from the compressed body.
    ///
    /// Empty when the body cannot be decoded (non-fatal).
    pub tx_hashes: Vec<B256>,
}

/// Open an ERA1 file and return an iterator that yields [`Era1BlockFacts`] for
/// every block in the file, in order.
///
/// # Errors
///
/// Returns `Err` if the file cannot be opened or lacks a valid block index.
/// Individual iterator items are `Err` when header or receipt decoding fails
/// for a specific block. Transaction-hash decode failures are treated as
/// non-fatal and produce an empty `tx_hashes` vec instead.
pub fn iter_era1_block_facts(
    path: impl AsRef<Path>,
) -> Result<impl Iterator<Item = Result<Era1BlockFacts, SourceError>>, SourceError> {
    let is_ere = path.as_ref().extension().and_then(|ext| ext.to_str()) == Some("ere");
    let iter = iter_era1_block_tuples(path)?;
    let mapped = iter.map(move |tuple_result| {
        let tuple = tuple_result?;

        let header = decode_era1_header(&tuple.compressed_header)?;
        let receipts = if is_ere {
            decode_ere_slim_receipts(&tuple.compressed_receipts)?
        } else {
            decode_era1_receipts(&tuple.compressed_receipts)?
        };
        let tx_hashes = decode_era1_tx_hashes(&tuple.compressed_body).unwrap_or_default();

        Ok(Era1BlockFacts {
            block_number: tuple.block_number,
            header,
            receipts,
            tx_hashes,
        })
    });
    Ok(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_rlp::Encodable;
    use std::io::Write;
    use tempfile::tempdir;

    // ── ERA1 binary helpers ──────────────────────────────────────────────────

    /// Snappy-frame-compress `data`.
    fn snappy_compress(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = snap::write::FrameEncoder::new(&mut out);
            enc.write_all(data).unwrap();
            enc.flush().unwrap();
        }
        out
    }

    /// Build a single e2store entry (8-byte header + data).
    fn e2store_entry(entry_type: [u8; 2], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + data.len());
        bytes.extend_from_slice(&entry_type);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    /// Encode `header` as `snappy(RLP(header))`.
    fn encode_header(header: &Header) -> Vec<u8> {
        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        snappy_compress(&rlp)
    }

    /// Encode an empty body (no transactions, no uncles) as
    /// `snappy(RLP([[],[]])))`.
    fn encode_empty_body() -> Vec<u8> {
        use alloy_rlp::Header as RlpHeader;

        let empty = RlpHeader {
            list: true,
            payload_length: 0,
        };
        let mut empty_list = Vec::new();
        empty.encode(&mut empty_list);

        let payload_len = empty_list.len() * 2;
        let outer = RlpHeader {
            list: true,
            payload_length: payload_len,
        };
        let mut body = Vec::new();
        outer.encode(&mut body);
        body.extend_from_slice(&empty_list);
        body.extend_from_slice(&empty_list);

        snappy_compress(&body)
    }

    /// Encode an empty receipt list as `snappy(RLP([]))`.
    fn encode_empty_receipts() -> Vec<u8> {
        // RLP empty list = 0xC0
        snappy_compress(&[0xC0])
    }

    /// Build a complete single-block ERA1 binary with the given block number,
    /// a valid (but empty) header, empty body and empty receipts.
    fn build_single_block_era1(block_number: u64) -> Vec<u8> {
        let header = Header {
            number: block_number,
            ..Default::default()
        };

        let compressed_header = encode_header(&header);
        let compressed_body = encode_empty_body();
        let compressed_receipts = encode_empty_receipts();
        // Total difficulty: 8 dummy bytes (u64 LE).
        let td = 0u64.to_le_bytes();

        // Block index: starting_number + one i64 offset (0) + count (1).
        let mut index_data = Vec::new();
        index_data.extend_from_slice(&block_number.to_le_bytes());
        index_data.extend_from_slice(&0i64.to_le_bytes()); // offset for this block
        index_data.extend_from_slice(&1i64.to_le_bytes()); // count

        let mut file = Vec::new();
        file.extend(e2store_entry([0x65, 0x32], &[])); // version
        file.extend(e2store_entry([0x03, 0x00], &compressed_header)); // header
        file.extend(e2store_entry([0x04, 0x00], &compressed_body)); // body
        file.extend(e2store_entry([0x05, 0x00], &compressed_receipts)); // receipts
        file.extend(e2store_entry([0x06, 0x00], &td)); // total difficulty
        file.extend(e2store_entry([0x66, 0x32], &index_data)); // block index
        file
    }

    /// Build an ERA1 binary where the header entry is replaced with raw
    /// `garbage_header` bytes (not valid snappy/RLP).
    fn build_era1_with_bad_header(block_number: u64, garbage_header: &[u8]) -> Vec<u8> {
        let td = 0u64.to_le_bytes();

        let mut index_data = Vec::new();
        index_data.extend_from_slice(&block_number.to_le_bytes());
        index_data.extend_from_slice(&0i64.to_le_bytes());
        index_data.extend_from_slice(&1i64.to_le_bytes());

        let mut file = Vec::new();
        file.extend(e2store_entry([0x65, 0x32], &[]));
        file.extend(e2store_entry([0x03, 0x00], garbage_header));
        file.extend(e2store_entry([0x04, 0x00], &encode_empty_body()));
        file.extend(e2store_entry([0x05, 0x00], &encode_empty_receipts()));
        file.extend(e2store_entry([0x06, 0x00], &td));
        file.extend(e2store_entry([0x66, 0x32], &index_data));
        file
    }

    /// Build an ERA1 binary with a valid header but a garbage receipts entry.
    fn build_era1_with_bad_receipts(block_number: u64, garbage_receipts: &[u8]) -> Vec<u8> {
        let header = Header {
            number: block_number,
            ..Default::default()
        };
        let compressed_header = encode_header(&header);
        let td = 0u64.to_le_bytes();

        let mut index_data = Vec::new();
        index_data.extend_from_slice(&block_number.to_le_bytes());
        index_data.extend_from_slice(&0i64.to_le_bytes());
        index_data.extend_from_slice(&1i64.to_le_bytes());

        let mut file = Vec::new();
        file.extend(e2store_entry([0x65, 0x32], &[]));
        file.extend(e2store_entry([0x03, 0x00], &compressed_header));
        file.extend(e2store_entry([0x04, 0x00], &encode_empty_body()));
        file.extend(e2store_entry([0x05, 0x00], garbage_receipts));
        file.extend(e2store_entry([0x06, 0x00], &td));
        file.extend(e2store_entry([0x66, 0x32], &index_data));
        file
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    /// A valid single-block ERA1 file should yield exactly one `Era1BlockFacts`
    /// whose `block_number` and `header.number` both match what we encoded.
    #[test]
    fn iter_block_facts_decodes_valid_block() {
        let block_number = 42_u64;

        let dir = tempdir().unwrap();
        let path = dir.path().join("mainnet-00000-deadbeef.era1");
        std::fs::write(&path, build_single_block_era1(block_number)).unwrap();

        let facts: Vec<_> = iter_era1_block_facts(&path)
            .expect("iter construction failed")
            .collect::<Result<Vec<_>, _>>()
            .expect("iterator yielded Err");

        assert_eq!(facts.len(), 1);
        let f = &facts[0];
        assert_eq!(f.block_number, block_number);
        assert_eq!(f.header.number, block_number);
        // Empty body → no tx hashes, empty receipts list.
        assert!(f.tx_hashes.is_empty());
        assert!(f.receipts.is_empty());
    }

    /// A garbage `compressed_header` must cause the iterator item to be `Err`
    /// rather than panicking.
    #[test]
    fn iter_block_facts_malformed_header_yields_err() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mainnet-00000-deadbeef.era1");
        std::fs::write(
            &path,
            build_era1_with_bad_header(10, &[0xDE, 0xAD, 0xBE, 0xEF]),
        )
        .unwrap();

        let mut iter = iter_era1_block_facts(&path).expect("iter construction failed");
        let result = iter.next().expect("iterator should yield one item");
        assert!(result.is_err(), "expected Err for malformed header, got Ok");
    }

    /// A valid header but garbage `compressed_receipts` must cause the iterator
    /// item to be `Err`.
    #[test]
    fn iter_block_facts_malformed_receipts_yields_err() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mainnet-00000-deadbeef.era1");
        std::fs::write(&path, build_era1_with_bad_receipts(10, &[0xFF, 0xFE, 0xFD])).unwrap();

        let mut iter = iter_era1_block_facts(&path).expect("iter construction failed");
        let result = iter.next().expect("iterator should yield one item");
        assert!(
            result.is_err(),
            "expected Err for malformed receipts, got Ok"
        );
    }

    /// Calling `iter_era1_block_facts` on a non-existent path must return `Err`
    /// immediately (not panic).
    #[test]
    fn iter_block_facts_missing_file_returns_err() {
        let result = iter_era1_block_facts("/nonexistent/path/that/does/not/exist.era1");
        assert!(result.is_err(), "expected Err for missing file, got Ok");
    }

    /// Build a multi-block ERA1 binary with the given block numbers (must be
    /// sequential), each with a valid but empty header/body/receipts.
    ///
    /// The block index format requires:
    ///   starting_block_number: u64 LE
    ///   offset_0 … offset_{N-1}: i64 LE each (offsets are not used by the
    ///     sequential iterator — only `from_block` and `count` matter there)
    ///   count: i64 LE
    fn build_multi_block_era1(block_numbers: &[u64]) -> Vec<u8> {
        assert!(!block_numbers.is_empty(), "need at least one block");
        let starting = block_numbers[0];

        let mut file = Vec::new();
        file.extend(e2store_entry([0x65, 0x32], &[])); // version

        for &n in block_numbers {
            let header = Header {
                number: n,
                ..Default::default()
            };
            file.extend(e2store_entry([0x03, 0x00], &encode_header(&header)));
            file.extend(e2store_entry([0x04, 0x00], &encode_empty_body()));
            file.extend(e2store_entry([0x05, 0x00], &encode_empty_receipts()));
            file.extend(e2store_entry([0x06, 0x00], &0u64.to_le_bytes()));
        }

        // Block index: starting_number + one i64 offset per block (all 0 —
        // the sequential iterator ignores them) + count.
        let mut index_data = Vec::new();
        index_data.extend_from_slice(&starting.to_le_bytes());
        for _ in block_numbers {
            index_data.extend_from_slice(&0i64.to_le_bytes());
        }
        index_data.extend_from_slice(&(block_numbers.len() as i64).to_le_bytes());
        file.extend(e2store_entry([0x66, 0x32], &index_data));

        file
    }

    /// A 3-block ERA1 file must yield exactly 3 `Era1BlockFacts` items, all
    /// `Ok`, with sequential `block_number` values matching what was encoded.
    ///
    /// This verifies that `iter_era1_block_facts` advances past the first block
    /// and correctly tracks `from_block + block_index` across the full file.
    #[test]
    fn iter_block_facts_multi_block_file_yields_all_blocks() {
        let block_numbers = [100_u64, 101, 102];

        let dir = tempdir().unwrap();
        let path = dir.path().join("mainnet-00000-deadbeef.era1");
        std::fs::write(&path, build_multi_block_era1(&block_numbers)).unwrap();

        let facts: Vec<_> = iter_era1_block_facts(&path)
            .expect("iter construction failed")
            .collect::<Result<Vec<_>, _>>()
            .expect("iterator yielded Err");

        assert_eq!(facts.len(), 3, "expected 3 facts, got {}", facts.len());
        for (i, f) in facts.iter().enumerate() {
            assert_eq!(
                f.block_number, block_numbers[i],
                "block_number mismatch at index {i}"
            );
            assert_eq!(
                f.header.number, block_numbers[i],
                "header.number mismatch at index {i}"
            );
            assert!(
                f.tx_hashes.is_empty(),
                "expected empty tx_hashes at index {i}"
            );
            assert!(
                f.receipts.is_empty(),
                "expected empty receipts at index {i}"
            );
        }
    }

    /// An ERA1 entry with a valid header and valid empty receipts (`0xC0`) but
    /// a garbage body (4 bytes of `0xDE 0xAD 0xBE 0xEF` that are not valid
    /// snappy data) must yield `Ok(Era1BlockFacts { tx_hashes: vec![], … })`
    /// rather than `Err`.
    ///
    /// This locks down the `unwrap_or_default()` call in `iter_era1_block_facts`
    /// that makes tx-hash decode failures non-fatal.
    #[test]
    fn iter_block_facts_bad_tx_body_yields_ok_with_empty_tx_hashes() {
        let block_number = 77_u64;
        let header = Header {
            number: block_number,
            ..Default::default()
        };
        let compressed_header = encode_header(&header);
        let compressed_receipts = encode_empty_receipts();
        let garbage_body: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
        let td = 0u64.to_le_bytes();

        let mut index_data = Vec::new();
        index_data.extend_from_slice(&block_number.to_le_bytes());
        index_data.extend_from_slice(&0i64.to_le_bytes());
        index_data.extend_from_slice(&1i64.to_le_bytes());

        let mut file_bytes = Vec::new();
        file_bytes.extend(e2store_entry([0x65, 0x32], &[]));
        file_bytes.extend(e2store_entry([0x03, 0x00], &compressed_header));
        file_bytes.extend(e2store_entry([0x04, 0x00], garbage_body)); // bad body
        file_bytes.extend(e2store_entry([0x05, 0x00], &compressed_receipts));
        file_bytes.extend(e2store_entry([0x06, 0x00], &td));
        file_bytes.extend(e2store_entry([0x66, 0x32], &index_data));

        let dir = tempdir().unwrap();
        let path = dir.path().join("mainnet-00000-deadbeef.era1");
        std::fs::write(&path, file_bytes).unwrap();

        let mut iter = iter_era1_block_facts(&path).expect("iter construction failed");
        let result = iter.next().expect("iterator should yield one item");

        let facts = result.expect("expected Ok, got Err — bad body should be non-fatal");
        assert_eq!(facts.block_number, block_number);
        assert!(
            facts.tx_hashes.is_empty(),
            "expected empty tx_hashes when body is garbage, got {:?}",
            facts.tx_hashes
        );
        // Receipts should still be decoded correctly from the valid receipts entry.
        assert!(facts.receipts.is_empty());
    }
}
