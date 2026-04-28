//! `query` command — query indexed events from SQLite and print them.
//!
//! Reads directly from SQLite without requiring a config file or a running server.
//! Useful for quick inspection during development or debugging.
//!
//! # Filters
//! - `--contract <address>` — filter by contract address
//! - `--event <name>` — filter by event name (e.g. `"Swap"`)
//! - `--topic0 <hex>` — filter by raw keccak256 topic0 hash (alternative to `--event`)
//! - `--limit <n>` — maximum number of rows to return (default: 100)
//!
//! # Output formats
//! - `table` (default) — aligned columns: block, event, contract, tx hash
//! - `json` — JSON array of objects including the decoded fields

use anyhow::Result;
use scopenode_storage::Db;

/// Query events from SQLite and print them.
///
/// Filters: `--contract`, `--event`, `--topic0`, `--from-block`, `--to-block`, `--limit`.
///
/// Output: `"table"` (default) prints aligned columns; `"json"` prints a JSON
/// array including the `decoded` fields — useful for piping to `jq` or scripts.
pub async fn run(
    db: Db,
    contract: Option<String>,
    event: Option<String>,
    topic0: Option<String>,
    from_block: Option<u64>,
    to_block: Option<u64>,
    limit: usize,
    output: String,
) -> Result<()> {
    let events = db
        .query_events_for_filter(
            contract.as_deref(),
            event.as_deref(),
            topic0.as_deref(),
            from_block,
            to_block,
            limit,
            0,
        )
        .await?;

    if events.is_empty() {
        println!("No events found.");
        return Ok(());
    }

    if output == "json" {
        // JSON output: emit a JSON array with full decoded fields.
        // The decoded field is stored as a JSON string in SQLite, so we parse it
        // back to avoid double-escaping (we want the object, not the string).
        let json_events: Vec<serde_json::Value> = events
            .iter()
            .map(|e| {
                serde_json::json!({
                    "contract": e.contract,
                    "event": e.event_name,
                    "block": e.block_number,
                    "tx_hash": e.tx_hash,
                    "log_index": e.log_index,
                    // Re-parse decoded JSON string into a Value so it's embedded
                    // as a nested object rather than an escaped string literal.
                    "decoded": serde_json::from_str::<serde_json::Value>(&e.decoded)
                        .unwrap_or(serde_json::Value::String(e.decoded.clone())),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_events)?);
    } else {
        // Table output: fixed-width columns for quick visual scanning.
        // Truncation is intentional — use --output json for full data.
        println!(
            "{:<12}  {:<20}  {:<44}  {:<70}",
            "Block", "Event", "Contract", "Tx Hash"
        );
        println!("{}", "─".repeat(150));
        for e in &events {
            println!(
                "{:<12}  {:<20}  {:<44}  {:<70}",
                e.block_number, e.event_name, e.contract, e.tx_hash
            );
        }
        println!("\n({} events shown)", events.len());
    }

    Ok(())
}
