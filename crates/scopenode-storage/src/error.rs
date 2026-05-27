//! Error types for the storage layer.
//!
//! [`DbError`] is the single error type returned by all `Db` methods. It is
//! re-exported at the crate root (`scopenode_storage::DbError`).

use thiserror::Error;

/// Error from any database operation.
///
/// Covers the three failure modes of the SQLite storage layer: opening the file,
/// running schema migrations, and executing queries.
#[derive(Debug, Error)]
pub enum DbError {
    /// SQLite file could not be opened or created.
    ///
    /// Common causes: the data directory does not exist, insufficient file system
    /// permissions, or a corrupt SQLite file that cannot be opened.
    #[error("Failed to open database: {0}")]
    Open(String),

    /// Schema migration failed.
    ///
    /// This can happen if the database was created by a newer version of scopenode
    /// (schema version mismatch), or if the file is corrupt. Deleting the database
    /// file and re-running `scopenode sync` will resolve this, though all indexed
    /// data will be lost.
    #[error("Migration failed: {0}")]
    Migration(String),

    /// A SQL query execution failed.
    ///
    /// The string contains the underlying `sqlx` error. Common causes: constraint
    /// violation (should not happen with `INSERT OR IGNORE`), disk full, or a
    /// programming error in the query string.
    #[error("Query failed: {0}")]
    Query(String),

    /// The query returned more rows than the requested limit.
    ///
    /// Callers should map this to their wire error format (HTTP 400 or JSON-RPC
    /// error code -32005) with a message that advises narrowing the filter.
    #[error("result set exceeds {limit} rows")]
    TooManyResults { limit: u64 },
}
