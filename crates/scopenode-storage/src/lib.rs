//! SQLite storage layer for scopenode.

#![deny(warnings)]

pub mod db;
pub mod error;
pub mod models;
pub mod sink;
pub mod types;

pub use db::Db;
pub use error::DbError;
pub use sink::DbEventSink;
pub use types::{EventFilter, QueryResult};
