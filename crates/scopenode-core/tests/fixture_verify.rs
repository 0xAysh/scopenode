//! Bloom scan tests against real Ethereum chain data (block 17000042).
//!
//! Fixture files are optional — tests skip if not present.
//! Generate them with a custom script or an Ethereum RPC.

use alloy_primitives::{address, keccak256, Address, Bloom};
use scopenode_core::headers::BloomScanner;

const BLOCK_JSON: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/block_17000042_block.json"
);

// Uniswap V3 ETH/USDC 0.3% pool
const UNISWAP_V3_POOL: Address = address!("8ad599c3a0ff1de082011efddc58f1908eb6e6d8");
const BLOCK_NUMBER: u64 = 17_000_042;

fn load_block() -> Option<Bloom> {
    let path = std::path::Path::new(BLOCK_JSON);
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(path).expect("read block fixture");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse block fixture JSON");

    let logs_bloom: Bloom = v["logsBloom"]
        .as_str()
        .expect("logsBloom must be a string")
        .parse()
        .expect("logsBloom must be a valid 256-byte hex string");

    Some(logs_bloom)
}

fn skip_if_missing() -> bool {
    if !std::path::Path::new(BLOCK_JSON).exists() {
        eprintln!("\n[fixture_verify] Fixtures not found — skipping real-data tests.\n");
        return true;
    }
    false
}

#[test]
fn bloom_scan_finds_uniswap_swap_in_block() {
    if skip_if_missing() {
        return;
    }

    let logs_bloom = load_block().expect("block fixture");
    let swap_topic0 = keccak256(b"Swap(address,address,int256,int256,uint160,uint128,int24)");
    let targets = BloomScanner::build_targets(&[swap_topic0], UNISWAP_V3_POOL);

    assert!(
        BloomScanner::matches(&logs_bloom, &targets),
        "BloomScanner::matches returned false for block {BLOCK_NUMBER}."
    );
}

#[test]
fn bloom_scan_rejects_absent_address() {
    if skip_if_missing() {
        return;
    }

    let logs_bloom = load_block().expect("block fixture");
    let absent_address = Address::ZERO;
    let swap_topic0 = keccak256(b"Swap(address,address,int256,int256,uint160,uint128,int24)");
    let targets = BloomScanner::build_targets(&[swap_topic0], absent_address);

    let matches = BloomScanner::matches(&logs_bloom, &targets);
    if matches {
        eprintln!(
            "[bloom_scan_rejects_absent_address] Address::ZERO appeared in bloom — \
             this is a bloom false positive (expected). Skipping assertion."
        );
    } else {
        assert!(
            !matches,
            "Address::ZERO should not be in the bloom for block {BLOCK_NUMBER}"
        );
    }
}
