//! SQLite storage layer for scopenode.

#![deny(warnings)]

pub mod db;
pub mod error;
pub mod models;
pub mod query;
pub mod sink;
pub mod types;

pub use db::Db;
pub use error::DbError;
pub use query::{ContractSummary, EventQuery, EventQueryOutcome, StatusSummary};
pub use sink::DbEventSink;
