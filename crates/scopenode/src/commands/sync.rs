//! `sync` command — indexes contract events from local ERA1 files.

use anyhow::{Context, Result};
use async_trait::async_trait;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use scopenode_core::{
    abi::{AbiCache, AbiStore},
    config::Config,
    era_pipeline::{run_era1_scopes, ProgressReporter},
    error::AbiError,
    source::scan_era1_source,
};
use scopenode_storage::Db;
use std::path::PathBuf;
use std::sync::Arc;

use crate::runtime::RuntimeContext;
use crate::sourcify::SourcifyClient;

struct DbAbiStore(Db);

#[async_trait]
impl AbiStore for DbAbiStore {
    async fn load(&self, address: &str) -> Result<Option<String>, AbiError> {
        self.0
            .get_contract_abi(address)
            .await
            .map_err(|e| AbiError::Cache(e.to_string()))
    }

    async fn save(&self, address: &str, name: Option<&str>, abi_json: &str) -> Result<(), AbiError> {
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
    let era_dir = crate::runtime::expand_tilde(config.node.era_dir.clone());

    if dry_run {
        println!("ERA1 source: {}", era_dir.display());
        println!("Contracts to sync:");
        for c in &config.contracts {
            println!(
                "  {} blocks {}-{}",
                c.name.as_deref().unwrap_or(&c.address.to_string()),
                c.from_block,
                c.to_block.unwrap_or(0),
            );
        }
        return Ok(());
    }

    // Compute union range across all contracts so scan only SHA256-hashes in-range files.
    let union_from = config
        .contracts
        .iter()
        .map(|c| c.from_block)
        .min()
        .unwrap_or(0);
    let union_to = config
        .contracts
        .iter()
        .filter_map(|c| c.to_block)
        .max()
        .unwrap_or(u64::MAX);

    let scan = scan_era1_source(&era_dir, None, union_from, union_to)
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
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to build HTTP client")?;
    let sourcify = Arc::new(SourcifyClient::new(http_client));

    let mut abi_cache = AbiCache::new(
        Arc::new(DbAbiStore(db.clone())),
        Some(sourcify),
    );
    let sink = scopenode_storage::DbEventSink::new(db);

    run_era1_scopes(
        &scan.files,
        &config.contracts,
        &mut abi_cache,
        &sink,
        &reporter,
    )
    .await
    .context("ERA1 sync failed")?;

    println!("sync complete — run `scopenode serve` to start JSON-RPC server");
    Ok(())
}

/// Entry point called from main.rs — loads config, opens DB, calls run().
pub async fn execute(config_path: PathBuf, dry_run: bool, quiet: bool) -> Result<()> {
    let ctx = RuntimeContext::load(config_path).await?;
    run(ctx.config, ctx.db, dry_run, quiet).await
}
