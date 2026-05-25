//! Shared data types used across the core pipeline.
//!
//! These types are the common language between the network layer, ABI decoder,
//! bloom scanner, and storage layer. They represent Ethereum concepts — headers,
//! logs, bloom filters — in a form that is convenient for the pipeline stages.

use alloy_primitives::{Bloom, B256};

/// A minimal Ethereum block header with the fields scopenode needs.
///
/// The full Ethereum header has more fields (e.g. `mixHash`, `nonce`, `extraData`)
/// but only these are needed for bloom scanning, Merkle verification, and storage.
#[derive(Debug, Clone)]
pub struct ScopeHeader {
    /// Block number (height in the canonical chain).
    pub number: u64,

    /// Block hash — keccak256 of the RLP-encoded header.
    pub hash: B256,

    /// Hash of the parent block. Used for reorg detection in live sync (Phase 3a).
    pub parent_hash: B256,

    /// Unix timestamp of the block (seconds since epoch).
    pub timestamp: u64,

    /// Merkle Patricia Trie root of all receipts in this block.
    ///
    /// Every receipt we fetch is verified against this before storing. The trie
    /// is rebuilt locally from the fetched receipts; if the computed root does not
    /// match this field, the peer sent tampered data and we discard the batch.
    pub receipts_root: B256,

    /// 2048-bit bloom filter encoding every address and topic that emitted a log in this block.
    ///
    /// Used to skip ~85–90% of blocks without fetching receipts. A block is
    /// skipped if NEITHER the contract address NOR the event topic0 appears in
    /// the bloom — bloom filters have zero false negatives, so this is safe.
    /// The ~15% false positive rate is acceptable cost: those blocks are fetched
    /// and found empty after Merkle verification.
    pub logs_bloom: Bloom,

    /// Total gas consumed by all transactions in this block.
    pub gas_used: u64,

    /// EIP-1559 base fee per gas (None for pre-London blocks before block 12,965,000).
    ///
    /// London hard fork introduced the base fee mechanism. Pre-London blocks
    /// do not have this field, so it is represented as `None` here.
    pub base_fee_per_gas: Option<u128>,
}

/// Bloom filter inputs for a single (contract, event) pair.
///
/// Used to check whether a block's `logsBloom` might contain logs from our
/// contract for a specific event. Both the address AND the topic0 must be
/// present in the bloom — if either is missing the block definitely has no
/// matching logs (zero false negatives guarantee).
#[derive(Debug, Clone)]
pub struct BloomTarget {
    /// Raw bytes of the contract address, used as the bloom filter input.
    ///
    /// The Ethereum bloom filter operates on raw byte slices; the 20-byte
    /// address is passed directly without any hashing by the caller.
    pub address_bytes: Vec<u8>,

    /// Raw bytes of each event's topic0 (keccak256 of the event signature).
    ///
    /// A `Vec` so one `BloomTarget` can cover multiple events for the same contract.
    /// Each entry is 32 bytes (a keccak256 digest). The bloom check passes if
    /// ANY of these topic0 bytes are present together with the address bytes.
    pub topic_bytes: Vec<Vec<u8>>,
}

/// Summary row for `scopenode status` output.
#[derive(Debug, Clone)]
pub struct ContractStatus {
    /// EIP-55 checksummed contract address.
    pub address: String,

    /// Human-readable label from the config file.
    pub name: Option<String>,

    /// Cached ABI JSON string from Sourcify. Avoids re-fetching on every sync.
    pub abi_json: Option<String>,
}
