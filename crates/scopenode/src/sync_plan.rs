use alloy_primitives::Address;
use scopenode_core::config::{Config, ContractConfig};
use std::fmt::Write as _;
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
    pub(crate) abi_override: Option<PathBuf>,
    pub(crate) impl_address: Option<Address>,
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
                abi_override: contract.abi_override.clone(),
                impl_address: contract.impl_address,
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

    pub(crate) fn render_dry_run(&self) -> String {
        let mut output = String::new();

        writeln!(output, "ERA1 source: {}", self.era_dir.display())
            .expect("writing to a String cannot fail");
        writeln!(output, "Contracts to sync:").expect("writing to a String cannot fail");

        for contract in &self.contracts {
            writeln!(
                output,
                "  {} blocks {}-{}",
                contract
                    .name
                    .as_deref()
                    .unwrap_or(&contract.address.to_string()),
                contract.block_range.start(),
                contract.block_range.end()
            )
            .expect("writing to a String cannot fail");
        }

        output
    }

    pub(crate) fn pipeline_contracts(&self) -> Vec<ContractConfig> {
        self.contracts
            .iter()
            .map(|contract| ContractConfig {
                name: contract.name.clone(),
                address: contract.address,
                events: contract.events.clone(),
                from_block: *contract.block_range.start(),
                to_block: Some(*contract.block_range.end()),
                abi_override: contract.abi_override.clone(),
                impl_address: contract.impl_address,
            })
            .collect()
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

    #[test]
    fn exposes_pipeline_contracts_without_losing_scope_preparation_fields() {
        let address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let impl_address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let mut cfg = config(
            "/tmp/era1",
            vec![contract(address, Some("USDC"), &["Transfer"], 100, 200)],
        );
        cfg.contracts[0].abi_override = Some(PathBuf::from("./abis/usdc.json"));
        cfg.contracts[0].impl_address = Some(impl_address);
        let plan = SyncPlan::from_config(&cfg);

        let pipeline_contracts = plan.pipeline_contracts();

        assert_eq!(pipeline_contracts.len(), 1);
        assert_eq!(pipeline_contracts[0].address, address);
        assert_eq!(pipeline_contracts[0].name.as_deref(), Some("USDC"));
        assert_eq!(pipeline_contracts[0].events, vec!["Transfer"]);
        assert_eq!(pipeline_contracts[0].from_block, 100);
        assert_eq!(pipeline_contracts[0].to_block, Some(200));
        assert_eq!(
            pipeline_contracts[0].abi_override,
            Some(PathBuf::from("./abis/usdc.json"))
        );
        assert_eq!(pipeline_contracts[0].impl_address, Some(impl_address));
    }

    #[test]
    fn renders_dry_run_from_plan_facts() {
        let address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let plan = SyncPlan {
            era_dir: PathBuf::from("/planned/era1"),
            contracts: vec![crate::sync_plan::ContractScopePlan {
                address,
                name: Some("PlannedUSDC".to_owned()),
                events: vec!["Transfer".to_owned()],
                block_range: 123..=456,
                abi_override: None,
                impl_address: None,
            }],
            block_range: 100..=500,
        };

        assert_eq!(
            plan.render_dry_run(),
            "ERA1 source: /planned/era1\nContracts to sync:\n  PlannedUSDC blocks 123-456\n"
        );
    }

    #[test]
    fn renders_address_when_contract_name_is_missing() {
        let address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let plan = SyncPlan {
            era_dir: PathBuf::from("/planned/era1"),
            contracts: vec![crate::sync_plan::ContractScopePlan {
                address,
                name: None,
                events: vec!["Transfer".to_owned()],
                block_range: 123..=456,
                abi_override: None,
                impl_address: None,
            }],
            block_range: 123..=456,
        };

        assert_eq!(
            plan.render_dry_run(),
            format!("ERA1 source: /planned/era1\nContracts to sync:\n  {address} blocks 123-456\n")
        );
    }
}
