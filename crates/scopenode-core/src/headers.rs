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

use alloy_primitives::{Address, Bloom, BloomInput, B256};

struct BloomTarget {
    address_bytes: Vec<u8>,
    topic_bytes: Vec<Vec<u8>>,
}

/// Bloom filter scanner for a specific (contract, events) combination.
///
/// Constructed once per contract scope with the precomputed targets, then used
/// to quickly test each block header's `logsBloom`. Owning the targets means
/// the pipeline never needs to carry `BloomTarget` as a separate concern.
pub struct BloomScanner {
    targets: Vec<BloomTarget>,
}

impl BloomScanner {
    /// Build a scanner for a set of event topic0 hashes and a contract address.
    ///
    /// One [`BloomTarget`] is created per event so a single contract watching
    /// multiple events generates multiple targets — `matches` returns `true`
    /// if ANY target matches (we don't require ALL events to be present).
    pub fn new(topic0s: &[B256], address: Address) -> Self {
        let address_bytes = address.as_slice().to_vec();
        let targets = topic0s
            .iter()
            .map(|t0| BloomTarget {
                address_bytes: address_bytes.clone(),
                topic_bytes: vec![t0.as_slice().to_vec()],
            })
            .collect();
        Self { targets }
    }

    /// Check whether a block's bloom filter might contain logs matching any target.
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
    pub fn matches(&self, bloom: &Bloom) -> bool {
        for target in &self.targets {
            // Checking address first is a cheap early exit: a block can only
            // contain our contract's logs if the address is in the bloom.
            if !bloom.contains_input(BloomInput::Raw(&target.address_bytes)) {
                continue;
            }
            // Both address AND topic0 must be set — address alone could be a
            // false positive from the contract appearing in an unrelated event.
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
        let scanner = BloomScanner::new(&[topic0], addr);

        assert!(scanner.matches(&bloom));
    }

    #[test]
    fn bloom_no_match_missing_address() {
        let addr = Address::repeat_byte(0xAB);
        let other_addr = Address::repeat_byte(0xCD);
        let topic0 = keccak256(b"Swap(address)");

        let bloom = make_bloom_with(&[other_addr.as_slice(), topic0.as_slice()]);
        let scanner = BloomScanner::new(&[topic0], addr);

        assert!(!scanner.matches(&bloom));
    }

    #[test]
    fn bloom_no_match_missing_topic() {
        let addr = Address::repeat_byte(0xAB);
        let topic0 = keccak256(b"Swap(address)");
        let other_topic = keccak256(b"Transfer(address,address,uint256)");

        let bloom = make_bloom_with(&[addr.as_slice(), other_topic.as_slice()]);
        let scanner = BloomScanner::new(&[topic0], addr);

        assert!(!scanner.matches(&bloom));
    }
}
