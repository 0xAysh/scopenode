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
use alloy::rpc::types::TransactionReceipt;
use alloy_primitives::B256;
use alloy_trie::HashBuilder;
use alloy_trie::Nibbles;

/// Verify that a set of receipts matches the `receipts_root` in the block header.
///
/// # Algorithm
/// 1. RLP-encode each transaction index as a trie key
/// 2. EIP-2718 encode each receipt as a trie value
/// 3. Sort key-value pairs by key (required for `HashBuilder` correctness)
/// 4. Feed sorted pairs into `HashBuilder` to compute the trie root
/// 5. Assert: `computed_root == expected_root`
///
/// If they match: the peer's receipts are authentic.
/// If not: the peer sent tampered data — call site should `mark_retry` and skip.
pub fn verify_receipts(
    receipts: &[TransactionReceipt],
    expected_root: B256,
    block_num: u64,
) -> Result<(), VerifyError> {
    // Empty block: no receipts means the receipt trie is empty.
    // The empty trie has a well-known root (keccak256 of the empty string RLP: 0x56e81f171...).
    if receipts.is_empty() {
        let empty_root = alloy_trie::EMPTY_ROOT_HASH;
        if expected_root == empty_root {
            return Ok(());
        } else {
            return Err(VerifyError::RootMismatch {
                block_num,
                expected: expected_root,
                computed: empty_root,
            });
        }
    }

    // Build a sorted list of (key, encoded_receipt) pairs.
    // Keys are RLP-encoded transaction indices (0, 1, 2, ...).
    let mut items: Vec<(Vec<u8>, Vec<u8>)> = receipts
        .iter()
        .enumerate()
        .map(|(i, receipt)| {
            let key = rlp_encode_index(i);
            let value = encode_receipt_for_trie(receipt);
            (key, value)
        })
        .collect();

    // Trie keys must be inserted in lexicographic order for HashBuilder to produce
    // the correct root. Since keys are RLP-encoded integers, lexicographic order
    // matches numeric order for indices up to ~16 million (well above any block size).
    items.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hb = HashBuilder::default();
    for (key, value) in &items {
        // Unpack the key bytes into nibbles (4-bit units) for the trie path.
        // Each byte becomes two nibbles; the trie operates at the nibble level.
        let nibbles = Nibbles::unpack(key);
        hb.add_leaf(nibbles, value);
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

/// EIP-2718 encode a transaction receipt for inclusion in the Merkle Patricia Trie.
///
/// The RPC `TransactionReceipt` type uses `rpc::Log` internally, but the consensus
/// `ReceiptEnvelope<primitives::Log>` type is what implements `Encodable2718`.
/// We convert between the two representations before encoding.
///
/// EIP-2718 format: for legacy receipts, the encoding is plain RLP. For typed
/// receipts (EIP-1559, EIP-2930, etc.), it's `transaction_type || RLP(receipt)`.
fn encode_receipt_for_trie(receipt: &TransactionReceipt) -> Vec<u8> {
    use alloy::eips::eip2718::Encodable2718;

    // Convert from RPC TransactionReceipt (which uses rpc::Log) to consensus type
    // (which uses primitives::Log). The consensus ReceiptEnvelope<primitives::Log>
    // implements Encodable2718, producing the canonical trie encoding.
    let consensus_receipt: alloy::rpc::types::TransactionReceipt<
        alloy::consensus::ReceiptEnvelope<alloy_primitives::Log>,
    > = receipt.clone().into();

    let mut buf = Vec::new();
    consensus_receipt.inner.encode_2718(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::VerifyError;
    use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope};
    use alloy::rpc::types::{Log as RpcLog, TransactionReceipt};
    use alloy_primitives::{Address, B256};
    use alloy_trie::EMPTY_ROOT_HASH;

    /// Build a minimal legacy receipt with no logs.
    /// Only `cumulative_gas_used` varies between calls — enough to change the trie root.
    fn make_receipt(cumulative_gas_used: u64) -> TransactionReceipt {
        let receipt: Receipt<RpcLog> = Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used,
            logs: vec![],
        };
        let inner: ReceiptEnvelope<RpcLog> = ReceiptEnvelope::Legacy(receipt.into());
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            gas_used: cumulative_gas_used,
            effective_gas_price: 0,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        }
    }

    #[test]
    fn empty_receipts_match_empty_root() {
        assert!(verify_receipts(&[], EMPTY_ROOT_HASH, 0).is_ok());
    }

    #[test]
    fn empty_receipts_wrong_root_fails() {
        let wrong_root = B256::from([1u8; 32]);
        let result = verify_receipts(&[], wrong_root, 0);
        assert!(
            matches!(result, Err(VerifyError::RootMismatch { expected, computed, .. })
                if expected == wrong_root && computed == EMPTY_ROOT_HASH),
            "expected RootMismatch with computed=EMPTY_ROOT_HASH, got {:?}",
            result
        );
    }

    #[test]
    fn tampered_receipt_fails() {
        let receipt_a = make_receipt(21_000);

        // Derive root_a: we don't know it in advance, so we use the error.
        let root_a = match verify_receipts(&[receipt_a.clone()], B256::ZERO, 1) {
            Err(VerifyError::RootMismatch { computed, .. }) => computed,
            other => panic!("expected RootMismatch when probing root, got {:?}", other),
        };

        // Sanity: receipt_a passes against its own root.
        assert!(
            verify_receipts(&[receipt_a], root_a, 1).is_ok(),
            "receipt_a should pass against root_a"
        );

        // Tampered: different cumulative_gas_used → different trie encoding → different root.
        let receipt_b = make_receipt(42_000);
        assert!(
            verify_receipts(&[receipt_b], root_a, 1).is_err(),
            "tampered receipt should fail against root_a"
        );
    }
}
