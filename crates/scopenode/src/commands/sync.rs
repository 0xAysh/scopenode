//! `sync` command — indexes contract events from local ERA1 files.

use anyhow::{Context, Result};
use async_trait::async_trait;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use scopenode_core::{
    abi_resolution::{AbiResolver, AbiStore},
    config::Config,
    era_pipeline::{run_era1_scopes, ProgressReporter},
    error::AbiError,
    source::Era1Source,
};
use scopenode_storage::Db;
use std::path::PathBuf;
use std::sync::Arc;

use crate::runtime::RuntimeContext;
use crate::sourcify::SourcifyClient;
use crate::sync_plan::SyncPlan;

struct DbAbiStore(Db);

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

/// Run the `sync` command.
pub async fn run(config: Config, db: Db, dry_run: bool, quiet: bool) -> Result<()> {
    let plan = SyncPlan::from_config(&config);

    if dry_run {
        print!("{}", plan.render_dry_run());
        return Ok(());
    }

    let source = Era1Source::scan(
        &plan.era_dir,
        None,
        *plan.block_range.start(),
        *plan.block_range.end(),
    )
    .with_context(|| format!("Failed to scan ERA1 source: {}", plan.era_dir.display()))?;

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
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to build HTTP client")?;
    let sourcify = Arc::new(SourcifyClient::new(http_client));

    let abi_resolver = AbiResolver::new(Arc::new(DbAbiStore(db.clone())), Some(sourcify));
    let sink = scopenode_storage::DbEventSink::new(db);
    let pipeline_contracts = plan.pipeline_contracts();

    let report = run_era1_scopes(
        &source,
        &pipeline_contracts,
        &abi_resolver,
        &sink,
        &reporter,
    )
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
