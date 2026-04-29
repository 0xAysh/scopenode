//! REST API server at `:8546`.
//!
//! Provides a JSON HTTP API and a Server-Sent Events (SSE) stream so any app
//! can consume indexed events without speaking Ethereum JSON-RPC.
//!
//! # Endpoints
//!
//! ```text
//! GET /events
//!       ?contract=0x...   — filter by contract address
//!       &event=Swap       — filter by event name
//!       &topic0=0xddf252… — filter by raw topic0 hash (alternative to ?event)
//!       &fromBlock=N      — inclusive lower bound
//!       &toBlock=N        — inclusive upper bound
//!       &limit=100        — max rows (default 100, hard cap 10 000)
//!       &offset=0         — pagination offset
//!
//! GET /status            — block number, contract count, total event count
//! GET /contracts         — list of indexed contracts with per-contract event counts
//! GET /abi/:address      — raw ABI JSON for a contract
//! GET /stream/events     — SSE: live events pushed as they are indexed
//!       ?contract=0x...   — optional contract filter
//!       &event=Swap       — optional event-name filter
//! ```
//!
//! All endpoints support CORS (open by default).
//! SSE subscribes to the live-sync broadcast channel — zero extra overhead.

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt as _};
use tower_http::cors::CorsLayer;
use tracing::info;

use scopenode_core::types::StoredEvent as LiveEvent;
use scopenode_storage::Db;

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    db: Db,
    broadcast: broadcast::Sender<LiveEvent>,
}

// ── Query parameter types ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EventsQuery {
    contract: Option<String>,
    event: Option<String>,
    topic0: Option<String>,
    #[serde(rename = "fromBlock")]
    from_block: Option<u64>,
    #[serde(rename = "toBlock")]
    to_block: Option<u64>,
    limit: Option<usize>,
    offset: Option<u64>,
}

#[derive(Deserialize)]
struct StreamQuery {
    contract: Option<String>,
    event: Option<String>,
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct EventResponse {
    contract: String,
    event_name: String,
    topic0: String,
    block_number: i64,
    block_hash: String,
    tx_hash: String,
    tx_index: i64,
    log_index: i64,
    decoded: serde_json::Value,
    source: String,
}

impl From<&scopenode_storage::models::StoredEvent> for EventResponse {
    fn from(e: &scopenode_storage::models::StoredEvent) -> Self {
        Self {
            contract: e.contract.clone(),
            event_name: e.event_name.clone(),
            topic0: e.topic0.clone(),
            block_number: e.block_number,
            block_hash: e.block_hash.clone(),
            tx_hash: e.tx_hash.clone(),
            tx_index: e.tx_index,
            log_index: e.log_index,
            decoded: serde_json::from_str(&e.decoded).unwrap_or(serde_json::Value::Null),
            source: e.source.clone(),
        }
    }
}

#[derive(Serialize)]
struct EventsResponse {
    events: Vec<EventResponse>,
    count: usize,
}

#[derive(Serialize)]
struct StatusResponse {
    block_number: u64,
    contract_count: usize,
    event_count: i64,
}

#[derive(Serialize)]
struct ContractResponse {
    address: String,
    name: Option<String>,
    event_count: i64,
}

#[derive(Serialize)]
struct ContractsResponse {
    contracts: Vec<ContractResponse>,
}

// ── Route handlers ────────────────────────────────────────────────────────────

async fn get_events(
    State(state): State<Arc<AppState>>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, (axum::http::StatusCode, String)> {
    let limit = q.limit.unwrap_or(100).min(10_000);
    let offset = q.offset.unwrap_or(0);

    let rows = state
        .db
        .query_events_for_filter(
            q.contract.as_deref(),
            q.event.as_deref(),
            q.topic0.as_deref(),
            q.from_block,
            q.to_block,
            limit,
            offset,
        )
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let events: Vec<EventResponse> = rows.iter().map(EventResponse::from).collect();
    let count = events.len();
    Ok(Json(EventsResponse { events, count }))
}

async fn get_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<StatusResponse>, (axum::http::StatusCode, String)> {
    let err = |e: scopenode_storage::error::DbError| {
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    };
    let (block_number, contract_count, event_count) = tokio::try_join!(
        async { state.db.latest_block_number().await.map(|n| n.unwrap_or(0)).map_err(err) },
        async { state.db.count_contracts().await.map_err(err) },
        async { state.db.count_all_events().await.map_err(err) },
    )?;

    Ok(Json(StatusResponse {
        block_number,
        contract_count: contract_count as usize,
        event_count,
    }))
}

async fn get_contracts(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ContractsResponse>, (axum::http::StatusCode, String)> {
    let rows = state
        .db
        .contracts_with_event_counts()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let contracts = rows
        .into_iter()
        .map(|(row, event_count)| ContractResponse {
            address: row.address,
            name: row.name,
            event_count,
        })
        .collect();

    Ok(Json(ContractsResponse { contracts }))
}

async fn get_abi(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
) -> impl IntoResponse {
    match state.db.get_contract_abi(&address).await {
        Ok(Some(abi)) => {
            let json: serde_json::Value =
                serde_json::from_str(&abi).unwrap_or(serde_json::Value::String(abi));
            (axum::http::StatusCode::OK, Json(json)).into_response()
        }
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "ABI not found for this address"})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn stream_events(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.broadcast.subscribe();
    let contract_filter = q.contract;
    let event_filter = q.event;

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let ev = result.ok()?; // swallow lagged errors — slow clients miss some events

        if !sse_matches(&ev, contract_filter.as_deref(), event_filter.as_deref()) {
            return None;
        }

        let addr = ev.contract.to_checksum(None);
        let data = serde_json::json!({
            "contract": addr,
            "event_name": ev.event_name,
            "block_number": ev.block_number,
            "tx_hash": format!("{:?}", ev.tx_hash),
            "log_index": ev.log_index,
            "decoded": ev.decoded,
        });

        Some(Ok(Event::default().data(data.to_string())))
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── SSE filter helper ─────────────────────────────────────────────────────────

/// Returns `true` when `ev` passes the optional contract and event-name filters.
///
/// Both comparisons are case-insensitive. An absent filter matches everything.
fn sse_matches(ev: &LiveEvent, contract_filter: Option<&str>, event_filter: Option<&str>) -> bool {
    let addr = ev.contract.to_checksum(None);
    if let Some(c) = contract_filter {
        if !addr.eq_ignore_ascii_case(c) {
            return false;
        }
    }
    if let Some(e) = event_filter {
        if !ev.event_name.eq_ignore_ascii_case(e) {
            return false;
        }
    }
    true
}

// ── Server startup ────────────────────────────────────────────────────────────

/// Build the REST API router without binding a port — used in integration tests.
#[doc(hidden)]
pub fn build_rest_router(db: Db, broadcast: broadcast::Sender<LiveEvent>) -> Router {
    let state = Arc::new(AppState { db, broadcast });
    Router::new()
        .route("/events", get(get_events))
        .route("/status", get(get_status))
        .route("/contracts", get(get_contracts))
        .route("/abi/:address", get(get_abi))
        .route("/stream/events", get(stream_events))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the REST API server on `127.0.0.1:<port>`.
///
/// The server runs as a background tokio task for the process lifetime.
/// `broadcast` is the live-sync event sender — the SSE endpoint subscribes
/// to it so live events appear within one block (~12 s) of being indexed.
pub async fn start_rest_server(
    port: u16,
    db: Db,
    broadcast: broadcast::Sender<LiveEvent>,
) -> anyhow::Result<()> {
    let state = Arc::new(AppState { db, broadcast });

    let router = Router::new()
        .route("/events", get(get_events))
        .route("/status", get(get_status))
        .route("/contracts", get(get_contracts))
        .route("/abi/:address", get(get_abi))
        .route("/stream/events", get(stream_events))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await?;

    info!(port, "REST server started at http://127.0.0.1:{port}");

    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("REST server exited unexpectedly");
    });

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, B256};
    use tokio::sync::broadcast;

    fn make_event(contract: Address, event_name: &str) -> LiveEvent {
        LiveEvent {
            contract,
            event_name: event_name.to_string(),
            topic0: B256::ZERO,
            block_number: 1,
            block_hash: B256::ZERO,
            tx_hash: B256::ZERO,
            tx_index: 0,
            log_index: 0,
            raw_topics: vec![],
            raw_data: Bytes::default(),
            decoded: serde_json::json!({}),
            source: "devp2p".to_string(),
            timestamp: 0,
        }
    }

    #[test]
    fn sse_no_filter_passes_all() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let ev = make_event(addr, "Transfer");
        assert!(sse_matches(&ev, None, None));
    }

    #[test]
    fn sse_contract_filter_passes_matching() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let ev = make_event(addr, "Transfer");
        assert!(sse_matches(&ev, Some(&addr.to_checksum(None)), None));
    }

    #[test]
    fn sse_contract_filter_blocks_other_contract() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let other = "0x0000000000000000000000000000000000000001";
        let ev = make_event(addr, "Transfer");
        assert!(!sse_matches(&ev, Some(other), None));
    }

    #[test]
    fn sse_event_filter_passes_matching_case_insensitive() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let ev = make_event(addr, "Transfer");
        assert!(sse_matches(&ev, None, Some("transfer")));
        assert!(sse_matches(&ev, None, Some("TRANSFER")));
    }

    #[test]
    fn sse_event_filter_blocks_other_event() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let ev = make_event(addr, "Transfer");
        assert!(!sse_matches(&ev, None, Some("Swap")));
    }

    /// Events sent on the broadcast channel are received by all active subscribers.
    #[tokio::test]
    async fn sse_broadcast_fan_out_reaches_all_receivers() {
        let addr: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let (tx, mut rx1) = broadcast::channel::<LiveEvent>(8);
        let mut rx2 = tx.subscribe();

        tx.send(make_event(addr, "Transfer")).unwrap();

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert_eq!(e1.event_name, "Transfer");
        assert_eq!(e2.event_name, "Transfer");
    }
}
