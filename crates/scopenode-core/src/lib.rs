//! Core pipeline logic for scopenode.
//!
//! This crate orchestrates the full event sync pipeline:
//!
//! 1. **[`abi`]** — fetch event definitions from Sourcify (or a local override)
//! 2. **[`network`]** — abstract transport for fetching headers and receipts
//! 3. **[`headers`]** — bloom filter scanning to identify candidate blocks
//! 4. **[`receipts`]** — Merkle Patricia Trie verification of fetched receipts
//! 5. **[`pipeline`]** — orchestrates all stages end-to-end with progress bars
//!
//! The [`network::EthNetwork`] trait abstracts the data transport layer.
//! [`network::DevP2PNetwork`] implements it using reth-network (devp2p peers).
//! No RPC provider is used at any point — all data comes from Ethereum mainnet
//! peers and is verified cryptographically via Merkle Patricia Trie.

#![deny(warnings)]

pub mod abi;
pub mod config;
pub mod error;
pub mod headers;
pub mod live;
pub mod network;
pub mod pipeline;
pub mod receipts;
pub mod reorg;
pub mod types;
