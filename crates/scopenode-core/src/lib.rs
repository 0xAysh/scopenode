//! Core pipeline logic for scopenode.
//!
//! Reads contract events from local ERA1 files, verifies via Merkle receipt root,
//! stores in SQLite, serves locally.

#![deny(warnings)]

pub mod abi;
pub mod abi_resolution;
pub use abi_resolution::AbiFetcher;
pub mod config;
pub mod decode_quality;
pub mod era1_codec;
pub mod era1_reader;
pub mod era_pipeline;
pub mod error;
pub mod headers;
pub mod receipts;
pub mod source;
pub mod types;
