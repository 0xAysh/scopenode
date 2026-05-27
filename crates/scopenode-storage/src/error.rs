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
    #[error("Failed to open database: {0}")]
    Open(String),

    /// Schema migration failed.
    #[error("Migration failed: {0}")]
    Migration(String),

    /// A SQL query execution failed.
    #[error("Query failed: {0}")]
    Query(String),
}
