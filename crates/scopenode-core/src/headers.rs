//! Bloom filter scanning for candidate blocks.
//!
//! The Ethereum `logsBloom` in each block header is a 2048-bit Bloom filter
//! that encodes every contract address and log topic that emitted an event in
//! that block. We use it to skip blocks that definitely contain no matching
//! events — avoiding ~85–90% of receipt fetches with zero false negatives.
//!
//! # How Ethereum bloom filters work
//! For each log emitted in a block, the EVM inserts:
//! 1. The emitting contract address (20 bytes)
//! 2. Each of the log's topics (up to 4 topics, 32 bytes each)
//!
//! A value is inserted by hashing it with keccak256, then setting 3 bits in the
//! 2048-bit filter using specific bit positions derived from the hash.
//!
//! # False positive rate
//! A 2048-bit filter with many entries has ~15% false positive rate in practice.
//! We accept this: bloom-matched blocks that actually contain no matching logs
//! are fetched, Merkle-verified (finding no relevant logs), and discarded.
//! This is correct behaviour — never a missed event.

use crate::types::BloomTarget;
use alloy_primitives::{Address, Bloom, BloomInput, B256};

/// Stateless bloom filter scanner.
///
/// All methods are static (take no mutable state), so `BloomScanner` is purely
/// a namespace for the bloom scanning functions. The actual bloom data lives in
/// the stored headers.
pub struct BloomScanner;

impl BloomScanner {
    /// Build bloom filter targets for a set of event topic0 hashes and a contract address.
    ///
    /// Each [`BloomTarget`] represents one `(address, topic0)` pair to check.
    /// We build one target per event so a single contract watching multiple events
    /// generates multiple targets — the `matches` call returns true if ANY target
    /// matches (i.e. we don't require ALL events to be present in the bloom).
    pub fn build_targets(topic0s: &[B256], address: Address) -> Vec<BloomTarget> {
        let address_bytes = address.as_slice().to_vec();

        topic0s
            .iter()
            .map(|t0| BloomTarget {
                address_bytes: address_bytes.clone(),
                topic_bytes: vec![t0.as_slice().to_vec()],
            })
            .collect()
    }

    /// Check whether a block's bloom filter might contain logs matching any of the targets.
    ///
    /// Returns `true` if ANY target matches — meaning the contract address is
    /// present in the bloom AND at least one event topic0 is present.
    ///
    /// Returns `false` only when we are **certain** the block has no matching logs.
    /// Bloom filters have zero false negatives: if the contract emitted this event
    /// in this block, both the address AND the topic0 are guaranteed to be in the bloom.
    ///
    /// ~15% of returned `true` values will be false positives — those blocks are
    /// fetched and found empty after Merkle verification. This is acceptable.
    pub fn matches(bloom: &Bloom, targets: &[BloomTarget]) -> bool {
        for target in targets {
            // First check: is the contract address present in the bloom?
            // If not, this target can't match — skip to the next target.
            // Checking address first is a cheap early exit since a block can only
            // contain our contract's logs if the address is in the bloom.
            let address_present =
                bloom.contains_input(BloomInput::Raw(&target.address_bytes));
            if !address_present {
                continue;
            }

            // Second check: is at least one of our event topic0s present?
            // topic0 must be present for our specific event type to appear in the block.
            // Both address AND topic0 must be set — address alone could be a false positive
            // from the contract being involved in a different event we don't care about.
            for topic_bytes in &target.topic_bytes {
                if bloom.contains_input(BloomInput::Raw(topic_bytes)) {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{keccak256, Address, Bloom};

    fn make_bloom_with(inputs: &[&[u8]]) -> Bloom {
        let mut bloom = Bloom::default();
        for input in inputs {
            bloom.accrue(BloomInput::Raw(input));
        }
        bloom
    }

    #[test]
    fn bloom_matches_address_and_topic() {
        let addr = Address::repeat_byte(0xAB);
        let event_sig = b"Swap(address,address,int256,int256,uint160,uint128,int24)";
        let topic0 = keccak256(event_sig);

        let bloom = make_bloom_with(&[addr.as_slice(), topic0.as_slice()]);
        let targets = BloomScanner::build_targets(&[topic0], addr);

        assert!(BloomScanner::matches(&bloom, &targets));
    }

    #[test]
    fn bloom_no_match_missing_address() {
        let addr = Address::repeat_byte(0xAB);
        let other_addr = Address::repeat_byte(0xCD);
        let topic0 = keccak256(b"Swap(address)");

        // Bloom only has other_addr, not addr
        let bloom = make_bloom_with(&[other_addr.as_slice(), topic0.as_slice()]);
        let targets = BloomScanner::build_targets(&[topic0], addr);

        assert!(!BloomScanner::matches(&bloom, &targets));
    }

    #[test]
    fn bloom_no_match_missing_topic() {
        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Swap(address)");
        let other_topic = keccak256(b"Transfer(address,address,uint256)");

        // Bloom has addr + other_topic but not our topic0
        let bloom = make_bloom_with(&[addr.as_slice(), other_topic.as_slice()]);
        let targets = BloomScanner::build_targets(&[topic0], addr);

        assert!(!BloomScanner::matches(&bloom, &targets));
    }
}
