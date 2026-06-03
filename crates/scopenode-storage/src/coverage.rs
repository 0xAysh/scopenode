//! Local scope coverage recorded by the indexing pipeline.
//!
//! Coverage is the storage seam for scopenode's "loud failure beats silent
//! partial data" rule. Query callers should not infer completeness from rows.

use crate::db::Db;
use crate::error::DbError;

/// A missing covered range for a contract query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCoverage {
    pub contract: String,
    pub from_block: u64,
    pub to_block: u64,
}

impl Db {
    /// Record that a contract scope was successfully processed over a range.
    pub async fn record_covered_range(
        &self,
        contract: &str,
        from_block: u64,
        to_block: u64,
    ) -> Result<(), DbError> {
        sqlx::query(
            r#"INSERT OR IGNORE INTO covered_ranges
               (contract, from_block, to_block, source)
               VALUES (?, ?, ?, 'era1')"#,
        )
        .bind(contract)
        .bind(from_block as i64)
        .bind(to_block as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// Return true when one recorded range fully covers the requested range.
    pub(crate) async fn is_range_covered(
        &self,
        contract: &str,
        from_block: u64,
        to_block: u64,
    ) -> Result<bool, DbError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            count: i64,
        }

        let row = sqlx::query_as::<_, Row>(
            r#"SELECT COUNT(*) as count
               FROM covered_ranges
               WHERE contract = ?
                 AND from_block <= ?
                 AND to_block >= ?"#,
        )
        .bind(contract)
        .bind(from_block as i64)
        .bind(to_block as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        Ok(row.count > 0)
    }
}
