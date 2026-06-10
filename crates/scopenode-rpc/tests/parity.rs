//! Parity test: confirm GET /events and eth_getLogs return equivalent rows
//! for the same filter.

use alloy::rpc::types::Filter;
use alloy_primitives::{address, Address};
use axum::body::Body;
use scopenode_rpc::{
    rest::build_rest_router,
    server::{EthApiImpl, EthApiServer},
};
use scopenode_storage::models::StoredEvent as StoredRow;
use scopenode_storage::Db;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

static COUNTER: AtomicU32 = AtomicU32::new(0);

async fn open_test_db() -> (Db, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("scopenode_parity_{}_{}.db", std::process::id(), n));
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
const TOPIC0_TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const TOPIC0_SWAP: &str = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";

fn make_row(block_number: i64, log_index: i64, topic0: &str) -> StoredRow {
    StoredRow {
        contract: ADDR_STR.to_string(),
        event_name: if topic0 == TOPIC0_TRANSFER {
            "Transfer"
        } else {
            "Swap"
        }
        .to_string(),
        topic0: topic0.to_string(),
        block_number,
        block_hash: format!("0x{:064x}", block_number),
        tx_hash: format!("0x{:064x}", log_index + 100),
        tx_index: 0,
        log_index,
        raw_topics: format!("[\"{topic0}\"]"),
        raw_data: "00".to_string(),
        decoded: "{}".to_string(),
        source: "era1".to_string(),
        timestamp: 0,
    }
}

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

fn norm(s: &str) -> String {
    s.to_lowercase()
}

#[derive(Debug, Eq, PartialEq, PartialOrd, Ord)]
struct Row {
    contract: String,
    block_number: u64,
    tx_hash: String,
    log_index: u64,
    topic0: String,
}

#[tokio::test]
async fn parity_all_events_for_contract() {
    let (db, path) = open_test_db().await;
    seed(&db).await;

    let rpc_rows = get_rpc_rows(&db, None).await;
    let rest_rows = get_rest_rows(&db, None).await;

    assert_eq!(
        rpc_rows, rest_rows,
        "eth_getLogs and GET /events must return the same rows"
    );
    assert_eq!(rpc_rows.len(), 3);

    cleanup(&path);
}

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

#[tokio::test]
async fn rpc_fails_explicitly_on_unprojectable_row() {
    let (db, path) = open_test_db().await;
    db.upsert_contract(ADDR_STR, Some("USDC"), "[]")
        .await
        .unwrap();
    let mut bad = make_row(100, 0, TOPIC0_TRANSFER);
    bad.tx_hash = "not_a_hash".into();
    db.insert_events(&[bad]).await.unwrap();

    let api = EthApiImpl::new(db.clone());
    let err = EthApiServer::get_logs(&api, Filter::new().address(ADDR))
        .await
        .expect_err("an unprojectable stored row must fail the request, not be dropped");

    assert_eq!(err.code(), -32006);
    assert!(
        err.message().contains("block 100") && err.message().contains("log 0"),
        "error must name the offending row, got: {}",
        err.message()
    );

    cleanup(&path);
}

#[tokio::test]
async fn rpc_fails_explicitly_on_lossy_raw_data_row() {
    let (db, path) = open_test_db().await;
    db.upsert_contract(ADDR_STR, Some("USDC"), "[]")
        .await
        .unwrap();
    let mut lossy = make_row(100, 0, TOPIC0_TRANSFER);
    lossy.raw_data = "not_hex!!!".into();
    db.insert_events(&[lossy]).await.unwrap();

    let api = EthApiImpl::new(db.clone());
    let err = EthApiServer::get_logs(&api, Filter::new().address(ADDR))
        .await
        .expect_err("a lossy row must not be served as fully valid log data");

    assert_eq!(err.code(), -32006);

    cleanup(&path);
}

#[tokio::test]
async fn rest_flags_malformed_decoded_json_distinguishably_from_stored_null() {
    let (db, path) = open_test_db().await;
    db.upsert_contract(ADDR_STR, Some("USDC"), "[]")
        .await
        .unwrap();
    let mut malformed = make_row(100, 0, TOPIC0_TRANSFER);
    malformed.decoded = "not valid json".into();
    let mut stored_null = make_row(101, 1, TOPIC0_TRANSFER);
    stored_null.decoded = "null".into();
    db.insert_events(&[malformed, stored_null]).await.unwrap();

    let router = build_rest_router(db.clone());
    let request = axum::http::Request::builder()
        .uri(format!("/events?contract={ADDR_STR}"))
        .body(Body::empty())
        .unwrap();
    let response = ServiceExt::<axum::http::Request<Body>>::oneshot(router, request)
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let events = json["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "REST returns every row");

    let by_block = |n: i64| {
        events
            .iter()
            .find(|ev| ev["block_number"].as_i64() == Some(n))
            .unwrap()
    };

    let flagged = by_block(100);
    assert!(flagged["decoded"].is_null());
    assert_eq!(flagged["decode_quality"]["quality"].as_str(), Some("lossy"));
    assert!(flagged["decode_quality"]["reason"]
        .as_str()
        .unwrap()
        .contains("decoded JSON"));

    let unflagged = by_block(101);
    assert!(unflagged["decoded"].is_null());
    assert!(
        unflagged.get("decode_quality").is_none(),
        "a stored JSON null is valid and must not be flagged"
    );

    cleanup(&path);
}

async fn get_json(db: &Db, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
    let router = build_rest_router(db.clone());
    let request = axum::http::Request::builder()
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let response = ServiceExt::<axum::http::Request<Body>>::oneshot(router, request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn rest_status_reports_storage_status_facts() {
    let (db, path) = open_test_db().await;
    seed(&db).await;

    let (status, json) = get_json(&db, "/status").await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["block_number"].as_u64(), Some(102));
    assert_eq!(json["contract_count"].as_u64(), Some(1));
    assert_eq!(json["event_count"].as_i64(), Some(3));

    cleanup(&path);
}

#[tokio::test]
async fn rest_status_on_empty_database_reports_zeros() {
    let (db, path) = open_test_db().await;

    let (status, json) = get_json(&db, "/status").await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["block_number"].as_u64(), Some(0));
    assert_eq!(json["contract_count"].as_u64(), Some(0));
    assert_eq!(json["event_count"].as_i64(), Some(0));

    cleanup(&path);
}

#[tokio::test]
async fn rest_contracts_reports_per_contract_event_counts() {
    let (db, path) = open_test_db().await;
    seed(&db).await;

    let (status, json) = get_json(&db, "/contracts").await;

    assert_eq!(status, axum::http::StatusCode::OK);
    let contracts = json["contracts"].as_array().unwrap();
    assert_eq!(contracts.len(), 1);
    assert_eq!(contracts[0]["address"].as_str(), Some(ADDR_STR));
    assert_eq!(contracts[0]["name"].as_str(), Some("USDC"));
    assert_eq!(contracts[0]["event_count"].as_i64(), Some(3));

    cleanup(&path);
}

async fn get_rpc_rows(db: &Db, topic0: Option<&str>) -> Vec<Row> {
    let api = EthApiImpl::new(db.clone());

    let mut filter = Filter::new().address(ADDR);
    if let Some(t0) = topic0 {
        let hash: alloy_primitives::B256 = t0.parse().unwrap();
        filter = filter.event_signature(hash);
    }

    let logs = EthApiServer::get_logs(&api, filter).await.unwrap();

    let mut rows: Vec<Row> = logs
        .iter()
        .map(|log| Row {
            contract: norm(&format!("{:?}", log.inner.address)),
            block_number: log.block_number.unwrap_or(0),
            tx_hash: norm(&format!("{:?}", log.transaction_hash.unwrap_or_default())),
            log_index: log.log_index.unwrap_or(0),
            topic0: log
                .inner
                .data
                .topics()
                .first()
                .map(|t| norm(&format!("{:?}", t)))
                .unwrap_or_default(),
        })
        .collect();
    rows.sort();
    rows
}

async fn get_rest_rows(db: &Db, topic0: Option<&str>) -> Vec<Row> {
    let router = build_rest_router(db.clone());

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
            contract: norm(ev["contract"].as_str().unwrap_or("")),
            block_number: ev["block_number"].as_i64().unwrap_or(0) as u64,
            tx_hash: norm(ev["tx_hash"].as_str().unwrap_or("")),
            log_index: ev["log_index"].as_i64().unwrap_or(0) as u64,
            topic0: norm(ev["topic0"].as_str().unwrap_or("")),
        })
        .collect();
    rows.sort();
    rows
}

async fn seed_many(db: &Db, count: i64) {
    db.upsert_contract(ADDR_STR, Some("USDC"), "[]")
        .await
        .unwrap();
    let rows: Vec<_> = (0..count)
        .map(|i| make_row(1_000 + i, 0, TOPIC0_TRANSFER))
        .collect();
    db.insert_events(&rows).await.unwrap();
}

#[tokio::test]
async fn rest_allows_exactly_ten_thousand_events() {
    let (db, path) = open_test_db().await;
    seed_many(&db, 10_000).await;

    let router = build_rest_router(db.clone());
    let request = axum::http::Request::builder()
        .uri(format!("/events?contract={ADDR_STR}&limit=10000"))
        .body(Body::empty())
        .unwrap();

    let response = ServiceExt::<axum::http::Request<Body>>::oneshot(router, request)
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["events"].as_array().unwrap().len(), 10_000);

    cleanup(&path);
}

#[tokio::test]
async fn rest_rejects_more_than_ten_thousand_events() {
    let (db, path) = open_test_db().await;
    seed_many(&db, 10_001).await;

    let router = build_rest_router(db.clone());
    let request = axum::http::Request::builder()
        .uri(format!("/events?contract={ADDR_STR}&limit=10000"))
        .body(Body::empty())
        .unwrap();

    let response = ServiceExt::<axum::http::Request<Body>>::oneshot(router, request)
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);

    cleanup(&path);
}

#[tokio::test]
async fn rpc_allows_exactly_ten_thousand_logs() {
    let (db, path) = open_test_db().await;
    seed_many(&db, 10_000).await;

    let rows = get_rpc_rows(&db, Some(TOPIC0_TRANSFER)).await;
    assert_eq!(rows.len(), 10_000);

    cleanup(&path);
}

#[tokio::test]
async fn rpc_rejects_more_than_ten_thousand_logs() {
    let (db, path) = open_test_db().await;
    seed_many(&db, 10_001).await;

    let api = EthApiImpl::new(db.clone());
    let hash: alloy_primitives::B256 = TOPIC0_TRANSFER.parse().unwrap();
    let filter = Filter::new().address(ADDR).event_signature(hash);
    let err = EthApiServer::get_logs(&api, filter).await.unwrap_err();
    assert_eq!(
        err.code(),
        jsonrpsee::types::error::ErrorCode::ServerError(-32005).code()
    );

    cleanup(&path);
}
