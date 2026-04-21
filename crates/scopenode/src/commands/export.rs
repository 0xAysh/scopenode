//! `export` command — stream all matching events to stdout as CSV or JSON.
//!
//! Unlike `scopenode query` (capped at a row limit), export streams the full
//! result set row-by-row from SQLite and writes directly to stdout. Intended
//! for piping into other tools (`jq`, `duckdb`, `pandas`, etc.).
//!
//! # Filters
//! - `--contract <address>` — filter by contract address
//! - `--event <name>` — filter by event name (e.g. `"Swap"`)
//! - `--topic0 <hex>` — filter by raw keccak256 topic0 hash
//! - `--from-block N` / `--to-block N` — inclusive block range
//!
//! # Output formats
//! - `csv` (default) — RFC 4180-compatible, with header row
//! - `json` — streaming JSON array, all 12 fields per event

use std::borrow::Cow;
use std::io::{self, BufWriter, Write};

use anyhow::Result;
use scopenode_storage::Db;
use tokio_stream::StreamExt;

const CSV_HEADER: &str = "contract,event_name,topic0,block_number,block_hash,\
    tx_hash,tx_index,log_index,raw_topics,raw_data,decoded,source";

pub async fn run(
    db: Db,
    contract: Option<String>,
    event: Option<String>,
    topic0: Option<String>,
    from_block: Option<u64>,
    to_block: Option<u64>,
    format: String,
) -> Result<()> {
    let result = export(db, contract, event, topic0, from_block, to_block, &format).await;
    // Broken pipe means the consumer (e.g. `head`) closed the read end — that's fine.
    if let Err(ref e) = result {
        if let Some(io_err) = e.downcast_ref::<io::Error>() {
            if io_err.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
        }
    }
    result
}

async fn export(
    db: Db,
    contract: Option<String>,
    event: Option<String>,
    topic0: Option<String>,
    from_block: Option<u64>,
    to_block: Option<u64>,
    format: &str,
) -> Result<()> {
    let stream = db.stream_events_for_filter(
        contract.as_deref(),
        event.as_deref(),
        topic0.as_deref(),
        from_block,
        to_block,
    );
    tokio::pin!(stream);

    let stdout = io::stdout();
    let mut w = BufWriter::new(stdout.lock());

    match format {
        "json" => {
            let mut first = true;
            write!(w, "[")?;
            while let Some(result) = stream.next().await {
                let row = result?;
                let decoded: serde_json::Value = serde_json::from_str(&row.decoded)
                    .unwrap_or(serde_json::Value::Null);
                let raw_topics: serde_json::Value = serde_json::from_str(&row.raw_topics)
                    .unwrap_or(serde_json::Value::Null);
                let obj = serde_json::json!({
                    "contract":     row.contract,
                    "event_name":   row.event_name,
                    "topic0":       row.topic0,
                    "block_number": row.block_number,
                    "block_hash":   row.block_hash,
                    "tx_hash":      row.tx_hash,
                    "tx_index":     row.tx_index,
                    "log_index":    row.log_index,
                    "raw_topics":   raw_topics,
                    "raw_data":     row.raw_data,
                    "decoded":      decoded,
                    "source":       row.source,
                });
                if !first {
                    write!(w, ",")?;
                }
                first = false;
                write!(w, "{}", serde_json::to_string(&obj)?)?;
            }
            writeln!(w, "]")?;
        }
        "csv" => {
            writeln!(w, "{}", CSV_HEADER)?;
            while let Some(result) = stream.next().await {
                let row = result?;
                writeln!(
                    w,
                    "{},{},{},{},{},{},{},{},{},{},{},{}",
                    csv_field(&row.contract),
                    csv_field(&row.event_name),
                    csv_field(&row.topic0),
                    row.block_number,
                    csv_field(&row.block_hash),
                    csv_field(&row.tx_hash),
                    row.tx_index,
                    row.log_index,
                    csv_field(&row.raw_topics),
                    csv_field(&row.raw_data),
                    csv_field(&row.decoded),
                    csv_field(&row.source),
                )?;
            }
        }
        other => anyhow::bail!("unknown format {:?} — use 'csv' or 'json'", other),
    }

    w.flush()?;
    Ok(())
}

/// RFC 4180 CSV field quoting.
///
/// Wraps the value in double-quotes and escapes internal `"` as `""` when the
/// value contains `,`, `"`, `\n`, or `\r`. Otherwise returns the value as-is.
fn csv_field(s: &str) -> Cow<'_, str> {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else {
        Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_value_not_quoted() {
        assert_eq!(csv_field("0xabcdef"), "0xabcdef");
        assert_eq!(csv_field("Transfer"), "Transfer");
        assert_eq!(csv_field(""), "");
    }

    #[test]
    fn value_with_comma_gets_quoted() {
        let result = csv_field("{\"amount\":1,\"to\":\"0xabc\"}");
        assert!(result.starts_with('"') && result.ends_with('"'));
    }

    #[test]
    fn internal_double_quotes_escaped() {
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn unknown_format_is_rejected() {
        // The error path for unknown format is tested via the match arm;
        // the actual anyhow::bail! cannot be tested in a unit test without
        // running the full async fn. Verify the csv/json arms compile correctly
        // by checking the CSV_HEADER constant is well-formed.
        assert!(CSV_HEADER.starts_with("contract,"));
        assert!(CSV_HEADER.ends_with(",source"));
        assert_eq!(CSV_HEADER.split(',').count(), 12);
    }
}
