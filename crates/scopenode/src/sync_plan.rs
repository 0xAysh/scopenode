use alloy_primitives::Address;
use scopenode_core::config::Config;
use std::ops::RangeInclusive;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncPlan {
    pub(crate) era_dir: PathBuf,
    pub(crate) contracts: Vec<ContractScopePlan>,
    pub(crate) block_range: RangeInclusive<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContractScopePlan {
    pub(crate) address: Address,
    pub(crate) name: Option<String>,
    pub(crate) events: Vec<String>,
    pub(crate) block_range: RangeInclusive<u64>,
}

impl SyncPlan {
    pub(crate) fn from_config(config: &Config) -> Self {
        let contracts: Vec<_> = config
            .contracts
            .iter()
            .map(|contract| ContractScopePlan {
                address: contract.address,
                name: contract.name.clone(),
                events: contract.events.clone(),
                block_range: contract.from_block
                    ..=contract
                        .to_block
                        .expect("validated config requires to_block for ERA1 sync"),
            })
            .collect();

        let union_from = contracts
            .iter()
            .map(|contract| *contract.block_range.start())
            .min()
            .unwrap_or(0);
        let union_to = contracts
            .iter()
            .map(|contract| *contract.block_range.end())
            .max()
            .unwrap_or(u64::MAX);

        Self {
            era_dir: crate::runtime::expand_tilde(config.node.era_dir.clone()),
            contracts,
            block_range: union_from..=union_to,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::sync_plan::SyncPlan;
    use alloy_primitives::address;
    use scopenode_core::config::{Config, ContractConfig, NodeConfig};
    use std::path::PathBuf;

    fn contract(
        address: alloy_primitives::Address,
        name: Option<&str>,
        events: &[&str],
        from_block: u64,
        to_block: u64,
    ) -> ContractConfig {
        ContractConfig {
            name: name.map(str::to_owned),
            address,
            events: events.iter().map(|event| (*event).to_owned()).collect(),
            from_block,
            to_block: Some(to_block),
            abi_override: None,
            impl_address: None,
        }
    }

    fn config(era_dir: &str, contracts: Vec<ContractConfig>) -> Config {
        Config {
            node: NodeConfig {
                port: 8545,
                rest_port: 8546,
                data_dir: None,
                era_dir: PathBuf::from(era_dir),
            },
            contracts,
        }
    }

    #[test]
    fn plans_single_contract_from_config() {
        let address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let cfg = config(
            "/tmp/era1",
            vec![contract(address, Some("USDC"), &["Transfer"], 100, 200)],
        );

        let plan = SyncPlan::from_config(&cfg);

        assert_eq!(plan.era_dir, PathBuf::from("/tmp/era1"));
        assert_eq!(plan.block_range, 100..=200);
        assert_eq!(plan.contracts.len(), 1);
        assert_eq!(plan.contracts[0].address, address);
        assert_eq!(plan.contracts[0].name.as_deref(), Some("USDC"));
        assert_eq!(plan.contracts[0].events, vec!["Transfer"]);
        assert_eq!(plan.contracts[0].block_range, 100..=200);
    }

    #[test]
    fn plans_multiple_contracts_from_config() {
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let dai = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
        let cfg = config(
            "/tmp/era1",
            vec![
                contract(usdc, Some("USDC"), &["Transfer", "Approval"], 100, 200),
                contract(dai, Some("DAI"), &["Transfer"], 300, 400),
            ],
        );

        let plan = SyncPlan::from_config(&cfg);

        assert_eq!(plan.contracts.len(), 2);
        assert_eq!(plan.contracts[0].address, usdc);
        assert_eq!(plan.contracts[0].events, vec!["Transfer", "Approval"]);
        assert_eq!(plan.contracts[1].address, dai);
        assert_eq!(plan.contracts[1].name.as_deref(), Some("DAI"));
        assert_eq!(plan.contracts[1].block_range, 300..=400);
    }

    #[test]
    fn computes_union_block_range_for_overlapping_scopes() {
        let cfg = config(
            "/tmp/era1",
            vec![
                contract(
                    address!("1111111111111111111111111111111111111111"),
                    None,
                    &["A"],
                    100,
                    300,
                ),
                contract(
                    address!("2222222222222222222222222222222222222222"),
                    None,
                    &["B"],
                    200,
                    400,
                ),
            ],
        );

        let plan = SyncPlan::from_config(&cfg);

        assert_eq!(plan.block_range, 100..=400);
    }

    #[test]
    fn computes_union_block_range_for_non_overlapping_scopes() {
        let cfg = config(
            "/tmp/era1",
            vec![
                contract(
                    address!("1111111111111111111111111111111111111111"),
                    None,
                    &["A"],
                    10,
                    20,
                ),
                contract(
                    address!("2222222222222222222222222222222222222222"),
                    None,
                    &["B"],
                    90,
                    100,
                ),
            ],
        );

        let plan = SyncPlan::from_config(&cfg);

        assert_eq!(plan.block_range, 10..=100);
    }

    #[test]
    fn preserves_missing_contract_display_name() {
        let address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let cfg = config(
            "/tmp/era1",
            vec![contract(address, None, &["Transfer"], 1, 2)],
        );

        let plan = SyncPlan::from_config(&cfg);

        assert_eq!(plan.contracts[0].name, None);
    }
}
