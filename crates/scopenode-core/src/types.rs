//! Shared data types used across the core pipeline.
//!
//! These types are the common language between the ABI decoder, pipeline stages,
//! and storage layer. They represent Ethereum concepts — headers, logs — in a
//! form that is convenient for the pipeline stages.

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
