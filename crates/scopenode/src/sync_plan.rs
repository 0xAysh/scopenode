use scopenode_core::config::{Config, ContractConfig};
use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::path::PathBuf;

/// Planning facts for one sync run: the normalized Archive source path, the
/// immutable Contract scopes exactly as configured, and the Union range.
///
/// The configured `ContractConfig` values are passed forward untouched — ABI
/// resolution and the pipeline consume the same Contract scopes the operator
/// wrote, with no intermediate copy to drift out of sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncPlan {
    pub(crate) era_dir: PathBuf,
    pub(crate) contracts: Vec<ContractConfig>,
    pub(crate) block_range: RangeInclusive<u64>,
}

impl SyncPlan {
    pub(crate) fn from_config(config: &Config) -> Self {
        let contracts = config.contracts.clone();

        let union_from = contracts.iter().map(|c| c.from_block).min().unwrap_or(0);
        let union_to = contracts
            .iter()
            .filter_map(|c| c.to_block)
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
            let to_block = contract
                .to_block
                .map(|b| b.to_string())
                .unwrap_or_else(|| "?".to_string());
            writeln!(
                output,
                "  {} blocks {}-{}",
                contract
                    .name
                    .as_deref()
                    .unwrap_or(&contract.address.to_string()),
                contract.from_block,
                to_block
            )
            .expect("writing to a String cannot fail");
        }

        output
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
    fn plan_passes_contract_scopes_through_unchanged() {
        let address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let impl_address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let mut cfg = config(
            "/tmp/era1",
            vec![contract(address, Some("USDC"), &["Transfer"], 100, 200)],
        );
        cfg.contracts[0].abi_override = Some(PathBuf::from("./abis/usdc.json"));
        cfg.contracts[0].impl_address = Some(impl_address);

        let plan = SyncPlan::from_config(&cfg);

        assert_eq!(
            plan.contracts, cfg.contracts,
            "planned Contract scopes are the configured scopes — no copy, no rebuild"
        );
        assert_eq!(plan.era_dir, PathBuf::from("/tmp/era1"));
        assert_eq!(plan.block_range, 100..=200);
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
    fn renders_dry_run_from_plan_facts() {
        let address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let plan = SyncPlan {
            era_dir: PathBuf::from("/planned/era1"),
            contracts: vec![contract(
                address,
                Some("PlannedUSDC"),
                &["Transfer"],
                123,
                456,
            )],
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
            contracts: vec![contract(address, None, &["Transfer"], 123, 456)],
            block_range: 123..=456,
        };

        assert_eq!(
            plan.render_dry_run(),
            format!("ERA1 source: /planned/era1\nContracts to sync:\n  {address} blocks 123-456\n")
        );
    }
}
