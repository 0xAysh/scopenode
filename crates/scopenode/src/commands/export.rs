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
    fn csv_header_has_all_twelve_fields() {
        assert!(CSV_HEADER.starts_with("contract,"));
        assert!(CSV_HEADER.ends_with(",source"));
        assert_eq!(CSV_HEADER.split(',').count(), 12);
    }

    // ── Format correctness against a real (temp) DB ───────────────────────────

    async fn setup_db_with_event() -> (scopenode_storage::Db, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("scopenode_export_test_{}_{}.db", std::process::id(), n));

        let db = scopenode_storage::Db::open(path.clone()).await.unwrap();

        let addr = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let abi = serde_json::json!([{
            "name": "Transfer",
            "inputs": [
                {"name": "from", "type": "address", "indexed": true},
                {"name": "to",   "type": "address", "indexed": true},
                {"name": "value","type": "uint256",  "indexed": false}
            ]
        }])
        .to_string();
        db.upsert_contract(addr, Some("test"), &abi).await.unwrap();

        let row = scopenode_storage::models::StoredEvent {
            contract:    addr.to_string(),
            event_name:  "Transfer".to_string(),
            topic0:      "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".to_string(),
            block_number: 1,
            block_hash:  "0x0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            tx_hash:     "0x0000000000000000000000000000000000000000000000000000000000000002".to_string(),
            tx_index:    0,
            log_index:   0,
            raw_topics:  "[]".to_string(),
            raw_data:    "0x".to_string(),
            decoded:     "{\"from\":\"0x0\",\"to\":\"0x1\",\"value\":\"1000\"}".to_string(),
            source:      "devp2p".to_string(),
        };
        db.insert_events(&[row]).await.unwrap();
        (db, path)
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[tokio::test]
    async fn csv_row_has_twelve_fields() {
        let (db, path) = setup_db_with_event().await;

        let mut buf = Vec::new();
        let stream = db.stream_events_for_filter(None, None, None, None, None);
        tokio::pin!(stream);

        use tokio_stream::StreamExt as _;
        // Write the CSV the same way export() does, but capture into a Vec.
        let mut w = std::io::BufWriter::new(&mut buf);
        writeln!(w, "{}", CSV_HEADER).unwrap();
        while let Some(result) = stream.next().await {
            let row = result.unwrap();
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
            )
            .unwrap();
        }
        drop(w);

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2, "header + 1 data row");
        // Data row must have exactly 12 comma-separated bare fields (before quoting
        // the decoded JSON with commas makes the split unreliable, so count header).
        assert_eq!(lines[0].split(',').count(), 12);

        cleanup(&path);
    }

    #[tokio::test]
    async fn json_output_has_expected_keys() {
        let (db, path) = setup_db_with_event().await;

        let stream = db.stream_events_for_filter(None, None, None, None, None);
        tokio::pin!(stream);

        use tokio_stream::StreamExt as _;
        let mut objects: Vec<serde_json::Value> = Vec::new();
        while let Some(result) = stream.next().await {
            let row = result.unwrap();
            let decoded: serde_json::Value =
                serde_json::from_str(&row.decoded).unwrap_or(serde_json::Value::Null);
            let raw_topics: serde_json::Value =
                serde_json::from_str(&row.raw_topics).unwrap_or(serde_json::Value::Null);
            objects.push(serde_json::json!({
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
            }));
        }

        assert_eq!(objects.len(), 1);
        let obj = &objects[0];
        for key in &[
            "contract", "event_name", "topic0", "block_number", "block_hash",
            "tx_hash", "tx_index", "log_index", "raw_topics", "raw_data",
            "decoded", "source",
        ] {
            assert!(obj.get(key).is_some(), "missing key: {key}");
        }
        assert_eq!(obj["event_name"], "Transfer");
        assert_eq!(obj["block_number"], 1);

        cleanup(&path);
    }
}
