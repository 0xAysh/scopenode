//! `scopenode validate <config>` — pre-flight check before syncing.
//!
//! Verifies each contract in the config without making any network connections
//! to devp2p peers:
//!   - Address is a valid Ethereum address (caught by config parse)
//!   - ABI is available on Sourcify (or `abi_override` is set and readable)
//!   - All listed event names exist in the ABI
//!   - `impl_address`, if set, has an ABI on Sourcify
//!
//! Exits with code 1 if any check fails so it can be used in CI.

use std::path::PathBuf;

use anyhow::Result;
use scopenode_core::abi::{load_abi_override, SourcifyClient};
use scopenode_core::config::Config;
use scopenode_core::error::AbiError;

/// Run the `validate` command.
pub async fn run(config_path: PathBuf) -> Result<()> {
    let config = Config::from_file(&config_path)?;
    let sourcify = SourcifyClient::new();
    let mut all_ok = true;

    for contract in &config.contracts {
        let label = contract.name.as_deref().unwrap_or("(unnamed)");
        println!("Contract: {label} ({})", contract.address);

        // Show proxy info when impl_address is set.
        if let Some(impl_addr) = contract.impl_address {
            println!("  proxy → impl  {impl_addr}");
        }

        // Determine which address to use for ABI lookup (same logic as AbiCache).
        let abi_address = contract.impl_address.unwrap_or(contract.address);

        // Fetch or load ABI.
        let events_result = if let Some(override_path) = &contract.abi_override {
            match load_abi_override(override_path, contract.address) {
                Ok(events) => {
                    println!("  ✓  abi_override loaded ({} events)", events.len());
                    Ok(events)
                }
                Err(e) => {
                    println!("  ✗  abi_override failed: {e}");
                    all_ok = false;
                    Err(())
                }
            }
        } else {
            match sourcify.fetch_events(abi_address).await {
                Ok(events) => {
                    if contract.impl_address.is_some() {
                        println!("  ✓  ABI from Sourcify ({abi_address}) — {} events", events.len());
                    } else {
                        println!("  ✓  ABI from Sourcify — {} events", events.len());
                    }
                    Ok(events)
                }
                Err(AbiError::NotOnSourcify(_)) => {
                    println!(
                        "  ✗  Not on Sourcify. Add to config:\n\
                         \n\
                         \t     abi_override = \"./abis/{}.json\"\n\
                         \n\
                         \t  Or, if this is a proxy, set:\n\
                         \n\
                         \t     impl_address = \"0x<implementation>\"",
                        contract.address
                    );
                    all_ok = false;
                    Err(())
                }
                Err(e) => {
                    println!("  ✗  ABI fetch failed: {e}");
                    all_ok = false;
                    Err(())
                }
            }
        };

        // Check that each configured event name exists in the ABI.
        if let Ok(all_events) = events_result {
            let names: std::collections::HashSet<&str> =
                all_events.iter().map(|e| e.name.as_str()).collect();

            for event_name in &contract.events {
                if let Some(event) = all_events.iter().find(|e| &e.name == event_name) {
                    println!(
                        "  ✓  event '{event_name}'  topic0 0x{}",
                        alloy_primitives::hex::encode(event.topic0())
                    );
                } else {
                    println!(
                        "  ✗  event '{event_name}' not found in ABI\n\
                         \t  Available events: {}",
                        names
                            .iter()
                            .copied()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    all_ok = false;
                }
            }
        }

        println!();
    }

    if all_ok {
        println!(
            "All checks passed — run `scopenode sync {}` to start.",
            config_path.display()
        );
        Ok(())
    } else {
        anyhow::bail!("Validation failed — fix the errors above and re-run.")
    }
}
