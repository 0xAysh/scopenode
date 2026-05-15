//! Merkle Patricia Trie verification of Ethereum block receipts.
//!
//! This is the core trust mechanism of scopenode. Before storing any events,
//! we rebuild the receipt trie from the data received from a peer and verify
//! its root matches `receipts_root` in the block header.
//!
//! A peer cannot fabricate events without breaking this check — the
//! `receipts_root` is part of the header that we independently fetched and
//! agreed on. Forging the header itself would require PoW/PoS work, which is
//! computationally infeasible.
//!
//! # Ethereum receipt trie structure
//! Each block has a Merkle Patricia Trie of receipts:
//! - Keys: RLP-encoded transaction indices (0, 1, 2, ...)
//! - Values: EIP-2718 encoded transaction receipts
//! - Root: stored in the block header as `receipts_root`
//!
//! We use `alloy-trie`'s `HashBuilder` to reconstruct this trie and compare
//! the computed root to the expected one.

use crate::error::VerifyError;
use alloy::eips::eip2718::Encodable2718;
use alloy_consensus::ReceiptEnvelope;
use alloy_primitives::{Log as PrimitiveLog, B256};
use alloy_trie::HashBuilder;
use alloy_trie::Nibbles;

/// Verify that a set of ERA1-sourced consensus receipts matches the `receipts_root`
/// in the block header.
///
/// Unlike [`verify_receipts`], this function takes [`ReceiptEnvelope<PrimitiveLog>`]
/// directly — the consensus type already implements `Encodable2718`, so no RPC
/// type conversion is required.
///
/// # Algorithm
/// Same as [`verify_receipts`]: RLP-encode indices as keys, EIP-2718 encode receipts
/// as values, sort, feed into `HashBuilder`, compare root.
pub fn verify_era1_receipts(
    receipts: &[ReceiptEnvelope<PrimitiveLog>],
    expected_root: B256,
    block_num: u64,
) -> Result<(), VerifyError> {
    if receipts.is_empty() {
        let empty_root = alloy_trie::EMPTY_ROOT_HASH;
        return if expected_root == empty_root {
            Ok(())
        } else {
            Err(VerifyError::RootMismatch {
                block_num,
                expected: expected_root,
                computed: empty_root,
            })
        };
    }

    let mut items: Vec<(Vec<u8>, Vec<u8>)> = receipts
        .iter()
        .enumerate()
        .map(|(i, receipt)| {
            let key = rlp_encode_index(i);
            let mut value = Vec::new();
            receipt.encode_2718(&mut value);
            (key, value)
        })
        .collect();

    items.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hb = HashBuilder::default();
    for (key, value) in &items {
        hb.add_leaf(Nibbles::unpack(key), value);
    }

    let computed = hb.root();
    if computed == expected_root {
        Ok(())
    } else {
        Err(VerifyError::RootMismatch {
            block_num,
            expected: expected_root,
            computed,
        })
    }
}

/// RLP-encode a transaction index for use as a trie key.
///
/// This follows the standard Ethereum RLP integer encoding:
/// - `0` → `[0x80]` (RLP encoding of the empty byte string, which represents 0)
/// - `1..=127` → single byte equal to the value
/// - `128+` → `[0x80 + len, byte1, byte2, ...]` (length-prefixed big-endian)
fn rlp_encode_index(index: usize) -> Vec<u8> {
    if index == 0 {
        vec![0x80] // RLP encoding of integer 0
    } else if index < 0x80 {
        // Single-byte integers 1–127 are their own RLP encoding.
        vec![index as u8]
    } else {
        // Multi-byte RLP integer encoding: prefix byte + big-endian value.
        let bytes = index.to_be_bytes();
        let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(7);
        let trimmed = &bytes[first_nonzero..];
        let mut result = vec![0x80 + trimmed.len() as u8];
        result.extend_from_slice(trimmed);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_era1_empty_block_passes() {
        use alloy_trie::EMPTY_ROOT_HASH;
        assert!(verify_era1_receipts(&[], EMPTY_ROOT_HASH, 0).is_ok());
    }

    #[test]
    fn verify_era1_single_legacy_receipt_correct_root() {
        use alloy::eips::eip2718::Encodable2718;
        use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
        use alloy_primitives::{Bloom, Log as PrimitiveLog};
        use alloy_trie::{HashBuilder, Nibbles};

        let with_bloom = ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::default(),
        };
        let envelope = ReceiptEnvelope::Legacy(with_bloom);

        // Compute expected root the same way the function does
        let key = vec![0x80u8]; // rlp_encode_index(0) for index 0
        let mut value = Vec::new();
        envelope.encode_2718(&mut value);
        let mut hb = HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(&key), &value);
        let expected_root = hb.root();

        assert!(verify_era1_receipts(&[envelope], expected_root, 1).is_ok());
    }

    #[test]
    fn verify_era1_wrong_root_fails() {
        use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
        use alloy_primitives::{Bloom, Log as PrimitiveLog, B256};

        let with_bloom = ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::default(),
        };
        let envelope = ReceiptEnvelope::Legacy(with_bloom);
        let wrong_root = B256::from([0xABu8; 32]);

        assert!(verify_era1_receipts(&[envelope], wrong_root, 1).is_err());
    }
}
