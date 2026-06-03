//! ABI resolution module.
//!
//! This module is the source-facing name for cache/local-file/remote ABI
//! resolution. Event decoding stays in `abi`.

use crate::abi::{AbiCache, EventAbi};
use crate::config::ContractConfig;
use crate::error::AbiError;

pub use crate::abi::{AbiFetcher, AbiStore};

pub struct AbiResolver {
    cache: AbiCache,
}

impl AbiResolver {
    pub fn new(
        store: std::sync::Arc<dyn AbiStore>,
        fetcher: Option<std::sync::Arc<dyn AbiFetcher>>,
    ) -> Self {
        Self {
            cache: AbiCache::new(store, fetcher),
        }
    }

    pub async fn resolve_events(
        &self,
        contract: &ContractConfig,
    ) -> Result<Vec<EventAbi>, AbiError> {
        self.cache.get_or_fetch(contract).await
    }
}
