//! `retry` command — not applicable in ERA1 mode.

use anyhow::Result;

pub async fn run() -> Result<()> {
    println!("retry is not supported in ERA1 mode — re-run `scopenode sync` to reprocess.");
    Ok(())
}
