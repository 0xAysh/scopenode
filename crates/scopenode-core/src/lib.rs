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
//! The [`network::EthNetwork`] trait abstracts the data transport layer so that
//! Phase 2 can swap in real devp2p peer connections without touching the pipeline.
//!
//! Phase 1 uses [`network::RpcNetwork`] — an alloy HTTP provider — as a
//! temporary stand-in. All events are tagged `source = "devp2p"` in the DB so
//! the schema stays consistent when the real transport is introduced.

#![deny(warnings)]

pub mod abi;
pub mod config;
pub mod error;
pub mod headers;
pub mod network;
pub mod pipeline;
pub mod receipts;
pub mod types;
