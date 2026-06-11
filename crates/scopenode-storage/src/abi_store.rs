//! `DbAbiStore` — adapts `Db` contract ABI persistence to the `AbiStore` trait.

use crate::Db;
use async_trait::async_trait;
use scopenode_core::abi_resolution::AbiStore;
use scopenode_core::error::AbiError;

/// Loads and saves cached contract ABIs through SQLite.
pub struct DbAbiStore(pub Db);

#[async_trait]
impl AbiStore for DbAbiStore {
    async fn load(&self, address: &str) -> Result<Option<String>, AbiError> {
        self.0
            .get_contract_abi(address)
            .await
            .map_err(|e| AbiError::Cache(e.to_string()))
    }

    async fn save(
        &self,
        address: &str,
        name: Option<&str>,
        abi_json: &str,
    ) -> Result<(), AbiError> {
        self.0
            .upsert_contract(address, name, abi_json)
            .await
            .map_err(|e| AbiError::Cache(e.to_string()))
    }
}
