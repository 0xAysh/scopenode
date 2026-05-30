//! Core pipeline logic for scopenode.
//!
//! Reads contract events from local ERA1 files, verifies via Merkle receipt root,
//! stores in SQLite, serves locally.

#![deny(warnings)]

pub mod abi;
pub use abi::AbiFetcher;
pub mod config;
pub mod era1_reader;
pub mod era_pipeline;
pub mod error;
pub mod headers;
pub mod receipts;
pub mod source;
pub mod types;
