use alloy::rpc::types::Filter;
use scopenode_storage::EventQuery;

/// Normalized event query plan shared by JSON-RPC and REST adapters.
#[derive(Debug, Clone)]
pub enum FilterPlan {
    Query(EventQuery),
    MissingAddress,
    Unsupported { reason: String },
}

impl FilterPlan {
    pub fn from_rpc_filter(filter: &Filter) -> Self {
        let addresses: Vec<_> = filter.address.iter().collect();
        let [addr] = addresses.as_slice() else {
            return if addresses.is_empty() {
                Self::MissingAddress
            } else {
                Self::Unsupported {
                    reason: "multiple address filters are not supported".to_string(),
                }
            };
        };

        let topic0_values: Vec<_> = filter
            .topics
            .first()
            .map(|topic| topic.iter().collect())
            .unwrap_or_default();
        if topic0_values.len() > 1 {
            return Self::Unsupported {
                reason: "topic0 OR filters are not supported".to_string(),
            };
        }
        if filter
            .topics
            .iter()
            .skip(1)
            .any(|topic| topic.iter().next().is_some())
        {
            return Self::Unsupported {
                reason: "topic filters beyond topic0 are not supported".to_string(),
            };
        }

        Self::Query(EventQuery {
            contract: Some(addr.to_checksum(None)),
            topic0: topic0_values.first().map(|topic| format!("{topic:?}")),
            from_block: filter.get_from_block(),
            to_block: filter.get_to_block(),
            limit: 10_000,
            ..EventQuery::default()
        })
    }

    pub fn from_rest_params(
        contract: Option<String>,
        event_name: Option<String>,
        topic0: Option<String>,
        from_block: Option<u64>,
        to_block: Option<u64>,
        limit: u64,
        offset: u64,
    ) -> Self {
        Self::Query(EventQuery {
            contract,
            event_name,
            topic0,
            from_block,
            to_block,
            limit,
            offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::FilterPlan;
    use alloy::rpc::types::Filter;
    use alloy_primitives::{address, B256};

    const TOPIC0: B256 = B256::new([0x11; 32]);

    #[test]
    fn rpc_filter_with_multiple_addresses_is_unsupported() {
        let first = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let second = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let filter = Filter::new()
            .address(vec![first, second])
            .event_signature(TOPIC0);

        let plan = FilterPlan::from_rpc_filter(&filter);

        assert!(matches!(plan, FilterPlan::Unsupported { .. }));
    }
}
