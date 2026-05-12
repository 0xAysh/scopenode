//! Core pipeline logic for scopenode.
//!
//! This crate provides the ERA1-backed event indexing pipeline:
//!
//! 1. **[`source`]** — scan local ERA1 files, decode headers/receipts/bodies
//! 2. **[`headers`]** — bloom filter scanning to identify candidate blocks
//! 3. **[`receipts`]** — Merkle Patricia Trie verification of receipts
//! 4. **[`abi`]** — fetch event definitions from Sourcify or a local override
//! 5. **[`era_pipeline`]** — orchestrates all stages end-to-end

#![deny(warnings)]

pub mod abi;
pub mod config;
pub mod era_pipeline;
pub mod error;
pub mod headers;
pub mod receipts;
pub mod source;
pub mod types;
