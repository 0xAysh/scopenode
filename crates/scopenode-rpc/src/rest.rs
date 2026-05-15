//! REST API server at `:8546`.
//!
//! # Endpoints
//!
//! ```text
//! GET /events
//!       ?contract=0x...   — filter by contract address
//!       &event=Swap       — filter by event name
//!       &topic0=0xddf252… — filter by raw topic0 hash
//!       &fromBlock=N      — inclusive lower bound
//!       &toBlock=N        — inclusive upper bound
//!       &limit=100        — max rows (default 100, hard cap 10_000)
//!       &offset=0         — pagination offset
//!
//! GET /status            — block number, contract count, total event count
//! GET /contracts         — indexed contracts with per-contract event counts
//! GET /abi/:address      — raw ABI JSON for a contract
//! ```

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tracing::info;

use scopenode_storage::Db;

#[derive(Clone)]
struct AppState {
    db: Db,
}

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

    if rows.len() >= 10_000 {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "result set exceeds 10,000 rows — narrow your filter (smaller block range or add address/topic filter)".into(),
        ));
    }

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

/// Build the REST API router without binding a port — used in integration tests.
#[doc(hidden)]
pub fn build_rest_router(db: Db) -> Router {
    let state = Arc::new(AppState { db });
    Router::new()
        .route("/events", get(get_events))
        .route("/status", get(get_status))
        .route("/contracts", get(get_contracts))
        .route("/abi/:address", get(get_abi))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the REST API server on `127.0.0.1:<port>`.
pub async fn start_rest_server(port: u16, db: Db) -> anyhow::Result<()> {
    let state = Arc::new(AppState { db });

    let router = Router::new()
        .route("/events", get(get_events))
        .route("/status", get(get_status))
        .route("/contracts", get(get_contracts))
        .route("/abi/:address", get(get_abi))
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
