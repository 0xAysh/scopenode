//! SQLite storage layer for scopenode.
//!
//! All persistent state lives here: block headers, bloom filter candidates,
//! decoded events, ABI cache, and sync progress cursors.
//!
//! The database is a single SQLite file opened in WAL mode for safe concurrent
//! reads. Migrations run automatically on [`Db::open`].
//!
//! All inserts use `INSERT OR IGNORE` (or `ON CONFLICT DO UPDATE`) so the
//! pipeline can be interrupted and resumed without duplicating data.

#![deny(warnings)]

pub mod db;
pub mod error;
pub mod models;
pub mod types;

pub use db::{Db, HeaderRow};
pub use error::DbError;
pub use types::SyncCursor;
