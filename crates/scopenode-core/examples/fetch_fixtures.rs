//! Fixture generator for integration tests.
//!
//! Fetches block 17000042 (Uniswap V3 ETH/USDC pool) from an Ethereum JSON-RPC
//! node and saves the raw responses as JSON files under `tests/fixtures/`.
//!
//! The fixture files are used by `tests/fixture_verify.rs` to run
//! `verify_receipts` and bloom scan against real on-chain data — without
//! a live network connection at test time.
//!
//! # Usage
//!
//! ```bash
//! ETH_RPC_URL=https://mainnet.infura.io/v3/<key> \
//!   cargo run --example fetch_fixtures
//! ```
//!
//! This writes two files:
//!   - `tests/fixtures/block_17000042_block.json`
//!   - `tests/fixtures/block_17000042_receipts.json`
//!
//! Run this once, commit the fixtures, and `cargo test` works offline forever.

use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

const BLOCK_NUMBER: u64 = 17_000_042;
const BLOCK_HEX: &str = "0x103668A";
const FIXTURES_DIR: &str = "tests/fixtures";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc_url = env::var("ETH_RPC_URL").unwrap_or_else(|_| {
        eprintln!("Error: ETH_RPC_URL environment variable is required.");
        eprintln!("Example: ETH_RPC_URL=https://mainnet.infura.io/v3/<key> cargo run --example fetch_fixtures");
        std::process::exit(1);
    });

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    println!("Fetching block {} from {rpc_url}...", BLOCK_NUMBER);

    // ── eth_getBlockByNumber ──────────────────────────────────────────────────
    let block_resp = rpc_call(
        &client,
        &rpc_url,
        "eth_getBlockByNumber",
        json!([BLOCK_HEX, false]),
    )
    .await?;

    let block = block_resp
        .get("result")
        .ok_or("eth_getBlockByNumber: missing 'result'")?
        .clone();

    if block.is_null() {
        return Err(format!("block {BLOCK_NUMBER} not found — is your node synced to mainnet?").into());
    }

    // Validate we got a receiptsRoot and logsBloom
    let receipts_root = block["receiptsRoot"]
        .as_str()
        .ok_or("block missing receiptsRoot")?;
    let logs_bloom = block["logsBloom"]
        .as_str()
        .ok_or("block missing logsBloom")?;

    println!(
        "  receiptsRoot: {}",
        &receipts_root[..18]
    );
    println!("  logsBloom:    {}...", &logs_bloom[..18]);

    // ── eth_getBlockReceipts ──────────────────────────────────────────────────
    println!("Fetching receipts for block {}...", BLOCK_NUMBER);

    let receipts_resp = rpc_call(
        &client,
        &rpc_url,
        "eth_getBlockReceipts",
        json!([BLOCK_HEX]),
    )
    .await?;

    let receipts = receipts_resp
        .get("result")
        .ok_or("eth_getBlockReceipts: missing 'result'")?
        .clone();

    if receipts.is_null() {
        return Err(
            "eth_getBlockReceipts returned null — your node may not support this method.\n\
             Try Infura, Alchemy, or any archive node."
                .into(),
        );
    }

    let receipt_count = receipts
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    println!("  {} receipts returned", receipt_count);

    // ── Write fixtures ────────────────────────────────────────────────────────
    fs::create_dir_all(FIXTURES_DIR)?;

    let block_path = Path::new(FIXTURES_DIR).join("block_17000042_block.json");
    let receipts_path = Path::new(FIXTURES_DIR).join("block_17000042_receipts.json");

    fs::write(&block_path, serde_json::to_string_pretty(&block)?)?;
    fs::write(&receipts_path, serde_json::to_string_pretty(&receipts)?)?;

    println!();
    println!("Fixtures written:");
    println!("  {}", block_path.display());
    println!("  {}", receipts_path.display());
    println!();
    println!("Run `cargo test` to verify the fixtures work correctly.");

    Ok(())
}

async fn rpc_call(
    client: &Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let body = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1,
    });

    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!("{method}: HTTP {}", resp.status()).into());
    }

    let json: Value = resp.json().await?;

    if let Some(err) = json.get("error") {
        return Err(format!("{method} RPC error: {err}").into());
    }

    Ok(json)
}
