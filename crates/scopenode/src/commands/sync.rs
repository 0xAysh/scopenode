//! `sync` command — indexes contract events from local ERA1 files.

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use scopenode_core::{
    abi_resolution::AbiResolver,
    config::{Config, ContractConfig},
    era_pipeline::{run_era1_scopes, ProgressReporter},
    source::Era1Source,
};
use scopenode_storage::{Db, DbAbiStore};
use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::runtime::RuntimeContext;
use crate::sourcify::SourcifyClient;

struct IndicatifReporter(ProgressBar);

impl ProgressReporter for IndicatifReporter {
    fn set_total(&self, n: u64) {
        self.0.set_length(n);
    }
    fn inc(&self) {
        self.0.inc(1);
    }
    fn finish(&self, msg: &str) {
        self.0.finish_with_message(msg.to_owned());
    }
}

fn union_block_range(contracts: &[ContractConfig]) -> RangeInclusive<u64> {
    let from = contracts.iter().map(|c| c.from_block).min().unwrap_or(0);
    let to = contracts
        .iter()
        .filter_map(|c| c.to_block)
        .max()
        .unwrap_or(u64::MAX);
    from..=to
}

fn render_dry_run(era_dir: &Path, contracts: &[ContractConfig]) -> String {
    let mut output = String::new();
    writeln!(output, "ERA1 source: {}", era_dir.display()).expect("writing to a String cannot fail");
    writeln!(output, "Contracts to sync:").expect("writing to a String cannot fail");
    for contract in contracts {
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
            to_block,
        )
        .expect("writing to a String cannot fail");
    }
    output
}

/// Run the `sync` command.
pub async fn run(config: Config, db: Db, dry_run: bool, quiet: bool) -> Result<()> {
    let era_dir = crate::runtime::expand_tilde(config.node.era_dir.clone());
    let block_range = union_block_range(&config.contracts);

    if dry_run {
        print!("{}", render_dry_run(&era_dir, &config.contracts));
        return Ok(());
    }

    let source = Era1Source::scan(&era_dir, None, *block_range.start(), *block_range.end())
        .with_context(|| format!("Failed to scan ERA1 source: {}", era_dir.display()))?;

    let mp = MultiProgress::new();
    if quiet {
        mp.set_draw_target(ProgressDrawTarget::hidden());
    }
    let pb = mp.add(ProgressBar::new(0));
    pb.set_style(
        ProgressStyle::default_bar()
            .template("ERA1 index     {bar:20.cyan/blue}  {pos:>7} / {len:<7}  {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("█░"),
    );
    let reporter = IndicatifReporter(pb);

    let http_client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("Failed to build HTTP client")?;
    let sourcify = Arc::new(SourcifyClient::new(http_client));

    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), Some(sourcify));
    let sink = scopenode_storage::DbEventSink::new(db);

    let report = run_era1_scopes(&source, &config.contracts, &abi_resolver, &sink, &reporter)
        .await
        .context("ERA1 sync failed")?;

    if !report.is_complete() {
        for (contract, reason) in &report.incomplete {
            eprintln!("  {contract}: {}", reason.describe());
        }
        anyhow::bail!(
            "sync incomplete — {} contract scope(s) did not earn coverage; fix the cause and rerun `scopenode sync`",
            report.incomplete.len()
        );
    }

    println!("sync complete — run `scopenode serve` to start JSON-RPC server");
    Ok(())
}

/// Entry point called from main.rs — loads config, opens DB, calls run().
pub async fn execute(config_path: PathBuf, dry_run: bool, quiet: bool) -> Result<()> {
    let ctx = RuntimeContext::load(config_path).await?;
    run(ctx.config, ctx.db, dry_run, quiet).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use scopenode_core::config::ContractConfig;
    use std::path::PathBuf;

    fn contract(
        address: alloy_primitives::Address,
        name: Option<&str>,
        from_block: u64,
        to_block: Option<u64>,
    ) -> ContractConfig {
        ContractConfig {
            name: name.map(str::to_owned),
            address,
            events: vec![],
            from_block,
            to_block,
            abi_override: None,
            impl_address: None,
        }
    }

    #[test]
    fn union_range_overlapping_scopes() {
        let contracts = vec![
            contract(
                address!("1111111111111111111111111111111111111111"),
                None,
                100,
                Some(300),
            ),
            contract(
                address!("2222222222222222222222222222222222222222"),
                None,
                200,
                Some(400),
            ),
        ];
        assert_eq!(union_block_range(&contracts), 100..=400);
    }

    #[test]
    fn union_range_non_overlapping_scopes() {
        let contracts = vec![
            contract(
                address!("1111111111111111111111111111111111111111"),
                None,
                10,
                Some(20),
            ),
            contract(
                address!("2222222222222222222222222222222222222222"),
                None,
                90,
                Some(100),
            ),
        ];
        assert_eq!(union_block_range(&contracts), 10..=100);
    }

    #[test]
    fn union_range_all_open_ended_to_block() {
        let contracts = vec![
            contract(
                address!("1111111111111111111111111111111111111111"),
                None,
                100,
                None,
            ),
            contract(
                address!("2222222222222222222222222222222222222222"),
                None,
                50,
                None,
            ),
        ];
        let range = union_block_range(&contracts);
        assert_eq!(*range.start(), 50);
        assert_eq!(*range.end(), u64::MAX, "all open-ended scopes produce u64::MAX upper bound");
    }

    #[test]
    fn renders_dry_run_format() {
        let addr = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let contracts = vec![contract(addr, Some("PlannedUSDC"), 123, Some(456))];
        let output = render_dry_run(&PathBuf::from("/planned/era1"), &contracts);
        assert_eq!(
            output,
            "ERA1 source: /planned/era1\nContracts to sync:\n  PlannedUSDC blocks 123-456\n"
        );
    }

    #[test]
    fn renders_address_when_name_is_missing() {
        let addr = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let contracts = vec![contract(addr, None, 123, Some(456))];
        let output = render_dry_run(&PathBuf::from("/planned/era1"), &contracts);
        assert_eq!(
            output,
            format!(
                "ERA1 source: /planned/era1\nContracts to sync:\n  {addr} blocks 123-456\n"
            )
        );
    }
}
