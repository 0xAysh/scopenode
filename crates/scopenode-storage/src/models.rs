//! SQLite row types — direct representations of database rows.
//!
//! These types mirror the database schema 1:1. Field types use `i64` (not `u64`)
//! because SQLite's INTEGER type is a signed 64-bit integer. All conversions to
//! and from the core types (which use `u64`, `Address`, `B256`, etc.) happen in
//! `db.rs`, keeping this module as a thin data layer.
//!
//! Hashes and addresses are stored as hex strings (with `0x` prefix) rather than
//! raw blobs for readability when inspecting the database with external tools.
//! The `logs_bloom` field is the exception — stored as a raw 256-byte BLOB for
//! efficiency (the bloom is never read as text).

/// A block header row as stored in SQLite.
///
/// All `u64` values from the Ethereum header are stored as `i64` because SQLite's
/// INTEGER type is signed. For Ethereum block numbers (currently ~20M), this is
/// safe — overflow would only occur above 9.2 × 10^18 blocks.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredHeader {
    /// Block number as `i64` (SQLite INTEGER is signed).
    pub number: i64,

    /// Block hash as a hex string with `0x` prefix.
    pub hash: String,

    /// Parent block hash — used for reorg detection in Phase 3a.
    pub parent_hash: String,

    /// Unix timestamp as `i64` (seconds since epoch).
    pub timestamp: i64,

    /// Merkle root of receipts — used for Merkle verification in the pipeline.
    pub receipts_root: String,

    /// Raw 256-byte bloom filter blob.
    ///
    /// Stored as a BLOB (not hex) for efficiency — the bloom is loaded back into
    /// an `alloy_primitives::Bloom` by `bloom_from_bytes` in `db.rs`.
    pub logs_bloom: Vec<u8>,

    /// Total gas used by all transactions in this block.
    pub gas_used: i64,

    /// EIP-1559 base fee in wei, NULL for pre-London blocks (before block 12,965,000).
    pub base_fee: Option<i64>,
}

/// A decoded event row as stored in SQLite.
///
/// Contains both the raw on-chain data (for re-processing if needed) and the
/// human-readable decoded form (for queries). The `(tx_hash, log_index)` pair
/// is the unique key — used by `INSERT OR IGNORE` for idempotent inserts.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredEvent {
    /// Contract address as EIP-55 checksummed hex string (e.g. `"0x8ad599c3..."`).
    pub contract: String,

    /// Human-readable event name (e.g. `"Swap"`, `"Transfer"`).
    pub event_name: String,

    /// `keccak256(event_signature)` as a `0x`-prefixed hex string.
    ///
    /// Same as `topics[0]` in the raw log. Used for filtering by event type.
    pub topic0: String,

    /// Block number where this event was emitted.
    pub block_number: i64,

    /// Hash of the block — stored for reorg tracking.
    ///
    /// In Phase 3a, if a reorg is detected, we set `reorged = 1` on all events
    /// with the orphaned block_hash without touching events in other blocks.
    pub block_hash: String,

    /// Hash of the transaction that emitted this event.
    pub tx_hash: String,

    /// Position of the transaction within the block (0-indexed).
    pub tx_index: i64,

    /// Position of this log within the block (0-indexed, globally unique per block).
    pub log_index: i64,

    /// JSON array of hex strings — all log topics including topic0.
    ///
    /// Example: `["0xddf252ad...","0x0000...sender","0x0000...recipient"]`
    pub raw_topics: String,

    /// Hex-encoded raw ABI data field from the log (non-indexed parameters).
    ///
    /// Example: `"00000000000000000000000000000000000000000000000056bc75e2d631000"`
    pub raw_data: String,

    /// JSON object of named decoded fields.
    ///
    /// Example: `{"from":"0x...","to":"0x...","value":"1000000000000000000"}`
    /// U256 values are decimal strings for JavaScript precision.
    pub decoded: String,

    /// Data source tag indicating which transport served this data.
    ///
    /// - `"devp2p"` — Phase 1 (RPC) and future Phase 2 (real devp2p)
    /// - `"era1"` — ERA1 archive fallback (Phase 2)
    /// - `"rpc"` — last-resort fallback RPC endpoint
    pub source: String,
}

/// Contract registry row — one row per indexed contract.
///
/// The `abi_json` column caches the full ABI so we don't re-fetch Sourcify on
/// every `scopenode sync` run.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContractRow {
    /// EIP-55 checksummed contract address.
    pub address: String,
    /// Human-readable label from the config file.
    pub name: Option<String>,
    /// Cached ABI JSON string from Sourcify or `abi_override`.
    pub abi_json: Option<String>,
}

/// Sync cursor row — tracks progress for each pipeline stage per contract.
///
/// Used to resume interrupted syncs without reprocessing already-completed work.
/// Each `*_done_to` field records the highest block number for which that stage
/// has been completed. NULL means the stage has not yet started.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SyncCursorRow {
    /// Contract address this cursor tracks (EIP-55 checksummed).
    pub contract: String,

    /// Configured start block (from the TOML config).
    pub from_block: i64,

    /// Configured end block (NULL for live sync mode).
    pub to_block: Option<i64>,

    /// Highest block number for which the header has been stored in SQLite.
    ///
    /// Header sync resumes from `headers_done_to + 1` on the next run.
    pub headers_done_to: Option<i64>,

    /// Highest block number for which bloom scanning has been completed.
    ///
    /// Bloom scan can only start after headers are available up to this point.
    pub bloom_done_to: Option<i64>,

    /// Highest block number for which receipt fetch + verify + store is complete.
    ///
    /// Receipt stage can only start after bloom scan is complete.
    pub receipts_done_to: Option<i64>,
}
