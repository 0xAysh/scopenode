//! Higher-level types used by the storage layer.
//!
//! These types use idiomatic Rust types (`u64`, `Option<u64>`) rather than the
//! SQLite-compatible `i64` / `Option<i64>` used in `models.rs`. The conversions
//! happen in `db.rs`.

use serde::{Deserialize, Serialize};

/// Records sync progress for a single contract so the pipeline can resume after
/// interruption.
///
/// Each field tracks how far a specific pipeline stage has progressed. Stages run
/// in strict order: headers → bloom → receipts. A stage can only resume from
/// where it left off if all earlier stages are complete up to that point.
///
/// # Example
/// After syncing headers for blocks 12M–13M but being interrupted mid-bloom-scan:
/// ```text
/// headers_done_to  = Some(13_000_000)
/// bloom_done_to    = Some(12_500_000)  // interrupted here
/// receipts_done_to = None              // not started yet
/// ```
/// The next run skips header sync entirely and resumes bloom scan from block 12,500,001.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncCursor {
    /// EIP-55 checksummed contract address this cursor tracks.
    pub contract: String,

    /// Configured start block (from the TOML config's `from_block`).
    pub from_block: u64,

    /// Configured end block (`None` = live sync mode).
    pub to_block: Option<u64>,

    /// Highest block for which the header has been fetched and stored in SQLite.
    ///
    /// Resume header sync from `headers_done_to + 1` on the next run.
    /// `None` means no headers have been synced yet for this contract.
    pub headers_done_to: Option<u64>,

    /// Highest block for which bloom scanning is complete.
    ///
    /// Bloom scan can only proceed up to `headers_done_to` (can't scan headers
    /// that haven't been fetched yet). Resume from `bloom_done_to + 1`.
    pub bloom_done_to: Option<u64>,

    /// Highest block for which receipt fetch + verify + store is complete.
    ///
    /// Receipt stage can only process blocks that passed the bloom scan. Resume
    /// by filtering `bloom_candidates WHERE fetched = 0`.
    pub receipts_done_to: Option<u64>,
}
