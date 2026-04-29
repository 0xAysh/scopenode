//! Parity test: confirm GET /events and eth_getLogs return equivalent rows
//! for the same filter.
//!
//! Both endpoints delegate to `query_events_for_filter`. This test seeds 3
//! events into a temp DB (one with a distinct topic0), calls both endpoints
//! independently, and asserts that the same rows come back.

use alloy::rpc::types::Filter;
use alloy_primitives::{address, Address};
use axum::body::Body;
use scopenode_rpc::{rest::build_rest_router, server::{EthApiImpl, EthApiServer}};
use scopenode_storage::models::StoredEvent as StoredRow;
use scopenode_storage::Db;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::broadcast;
use tower::ServiceExt;

static COUNTER: AtomicU32 = AtomicU32::new(0);

async fn open_test_db() -> (Db, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir()
        .join(format!("scopenode_parity_{}_{}.db", std::process::id(), n));
    let db = Db::open(path.clone()).await.unwrap();
    (db, path)
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

const ADDR: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const ADDR_STR: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
const TOPIC0_TRANSFER: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const TOPIC0_SWAP: &str =
    "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";

fn make_row(block_number: i64, log_index: i64, topic0: &str) -> StoredRow {
    StoredRow {
        contract:     ADDR_STR.to_string(),
        event_name:   if topic0 == TOPIC0_TRANSFER { "Transfer" } else { "Swap" }.to_string(),
        topic0:       topic0.to_string(),
        block_number,
        block_hash:   format!("0x{:064x}", block_number),
        tx_hash:      format!("0x{:064x}", log_index + 100),
        tx_index:     0,
        log_index,
        raw_topics:   format!("[\"{topic0}\"]"),
        raw_data:     "00".to_string(),
        decoded:      "{}".to_string(),
        source:       "devp2p".to_string(),
    }
}

/// Seed the DB with a contract row and 3 events (2 Transfer, 1 Swap).
async fn seed(db: &Db) {
    db.upsert_contract(ADDR_STR, Some("USDC"), "[]")
        .await
        .unwrap();
    let rows = vec![
        make_row(100, 0, TOPIC0_TRANSFER),
        make_row(101, 1, TOPIC0_TRANSFER),
        make_row(102, 2, TOPIC0_SWAP),
    ];
    db.insert_events(&rows).await.unwrap();
}

/// Normalize hex to lowercase for stable comparison.
fn norm(s: &str) -> String {
    s.to_lowercase()
}

// ── Row comparison helper ─────────────────────────────────────────────────────

#[derive(Debug, Eq, PartialEq, PartialOrd, Ord)]
struct Row {
    contract:     String,
    block_number: u64,
    tx_hash:      String,
    log_index:    u64,
    topic0:       String,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Both endpoints return the same 3 rows for a plain contract filter.
#[tokio::test]
async fn parity_all_events_for_contract() {
    let (db, path) = open_test_db().await;
    seed(&db).await;

    let rpc_rows = get_rpc_rows(&db, None).await;
    let rest_rows = get_rest_rows(&db, None).await;

    assert_eq!(rpc_rows, rest_rows, "eth_getLogs and GET /events must return the same rows");
    assert_eq!(rpc_rows.len(), 3);

    cleanup(&path);
}

/// Both endpoints return only the 2 Transfer rows when filtered by topic0.
#[tokio::test]
async fn parity_topic0_filter() {
    let (db, path) = open_test_db().await;
    seed(&db).await;

    let rpc_rows = get_rpc_rows(&db, Some(TOPIC0_TRANSFER)).await;
    let rest_rows = get_rest_rows(&db, Some(TOPIC0_TRANSFER)).await;

    assert_eq!(rpc_rows, rest_rows);
    assert_eq!(rpc_rows.len(), 2, "only Transfer events should match");

    cleanup(&path);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Call EthApiImpl::get_logs directly with an address filter (and optional topic0).
async fn get_rpc_rows(db: &Db, topic0: Option<&str>) -> Vec<Row> {
    let (broadcast_tx, _) = broadcast::channel(8);
    let (headers_tx, _) = broadcast::channel(8);
    let api = EthApiImpl::new(db.clone(), broadcast_tx, headers_tx);

    let mut filter = Filter::new().address(ADDR);
    if let Some(t0) = topic0 {
        let hash: alloy_primitives::B256 = t0.parse().unwrap();
        filter = filter.event_signature(hash);
    }

    let logs = EthApiServer::get_logs(&api, filter).await.unwrap();

    let mut rows: Vec<Row> = logs
        .iter()
        .map(|log| Row {
            contract:     norm(&format!("{:?}", log.inner.address)),
            block_number: log.block_number.unwrap_or(0),
            tx_hash:      norm(&format!("{:?}", log.transaction_hash.unwrap_or_default())),
            log_index:    log.log_index.unwrap_or(0),
            topic0:       log.inner.data.topics()
                             .first()
                             .map(|t| norm(&format!("{:?}", t)))
                             .unwrap_or_default(),
        })
        .collect();
    rows.sort();
    rows
}

/// Call GET /events via axum tower::ServiceExt::oneshot (no port binding).
async fn get_rest_rows(db: &Db, topic0: Option<&str>) -> Vec<Row> {
    let (broadcast_tx, _) = broadcast::channel(8);
    let router = build_rest_router(db.clone(), broadcast_tx);

    let uri = if let Some(t0) = topic0 {
        format!("/events?contract={ADDR_STR}&topic0={t0}")
    } else {
        format!("/events?contract={ADDR_STR}")
    };

    let request = axum::http::Request::builder()
        .uri(&uri)
        .body(Body::empty())
        .unwrap();

    let response = ServiceExt::<axum::http::Request<Body>>::oneshot(router, request)
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let mut rows: Vec<Row> = json["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ev| Row {
            contract:     norm(ev["contract"].as_str().unwrap_or("")),
            block_number: ev["block_number"].as_i64().unwrap_or(0) as u64,
            tx_hash:      norm(ev["tx_hash"].as_str().unwrap_or("")),
            log_index:    ev["log_index"].as_i64().unwrap_or(0) as u64,
            topic0:       norm(ev["topic0"].as_str().unwrap_or("")),
        })
        .collect();
    rows.sort();
    rows
}
