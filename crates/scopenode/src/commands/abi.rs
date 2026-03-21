//! `scopenode abi <address>` — fetch and display a contract's ABI from Sourcify.
//!
//! Useful for discovering event names and their topic0 hashes before writing
//! a config, and for verifying that a contract is verified on Sourcify at all.

use alloy_primitives::Address;
use anyhow::{Context, Result};
use scopenode_core::abi::SourcifyClient;

/// Run the `abi` command.
///
/// Fetches all events for the given address from Sourcify and prints each
/// one with its canonical signature and computed topic0 hash.
pub async fn run(address: &str) -> Result<()> {
    let addr: Address = address
        .parse()
        .with_context(|| format!("Invalid Ethereum address: {address}"))?;

    let client = SourcifyClient::new();

    println!("Fetching ABI from Sourcify for {addr}...\n");

    let events = client.fetch_events(addr).await.with_context(|| {
        format!(
            "Failed to fetch ABI for {addr}.\n\
             If this contract is not on Sourcify, set `abi_override` in your config\n\
             to point at a local ABI JSON file (from Etherscan, Hardhat, or Foundry)."
        )
    })?;

    if events.is_empty() {
        println!("No events found in ABI.");
        return Ok(());
    }

    for event in &events {
        println!("Event: {}", event.name);
        println!("  Signature : {}", event.signature());
        println!(
            "  Topic0    : 0x{}",
            alloy_primitives::hex::encode(event.topic0())
        );
        if !event.inputs.is_empty() {
            println!("  Parameters:");
            for input in &event.inputs {
                let indexed = if input.indexed { "  [indexed]" } else { "" };
                println!("    {:<12}  {}{}", input.ty, input.name, indexed);
            }
        }
        println!();
    }

    Ok(())
}
