//! `sync` command — indexes contract events from local ERA1 files.

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use scopenode_core::{
    abi::AbiCache,
    config::Config,
    era_pipeline::{run_era1_scopes, ProgressReporter},
    source::scan_era1_source,
};
use scopenode_storage::Db;
use std::path::PathBuf;

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
    let era_dir = expand_tilde(config.node.era_dir.clone());

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

    let mut abi_cache = AbiCache::new(db.clone());

    run_era1_scopes(
        &scan.files,
        &config.contracts,
        &mut abi_cache,
        &db,
        &reporter,
    )
    .await
    .context("ERA1 sync failed")?;

    println!("sync complete — run `scopenode serve` to start JSON-RPC server");
    Ok(())
}

/// Entry point called from main.rs — loads config, opens DB, calls run().
pub async fn execute(config_path: PathBuf, dry_run: bool, quiet: bool) -> Result<()> {
    let config = Config::from_file(&config_path).context("Failed to load config")?;

    let data_dir = expand_tilde(
        config
            .node
            .data_dir
            .clone()
            .unwrap_or_else(default_data_dir),
    );
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("Failed to create data dir: {}", data_dir.display()))?;

    let db = Db::open(data_dir.join("scopenode.db"))
        .await
        .context("Failed to open database")?;

    run(config, db, dry_run, quiet).await
}

fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".scopenode")
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(stripped) = s.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(stripped);
            }
        }
    }
    path
}
