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
//!
//! # Malformed stored rows
//!
//! `GET /events` returns every stored row. A row whose Projection degraded
//! carries a `decode_quality` object (`{"quality": "lossy"|"invalid",
//! "reason": …}`); in particular, a `decoded: null` produced by a JSON parse
//! fallback is flagged, while a stored JSON `null` is not.

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

use crate::filter_plan::FilterPlan;
use crate::projection::{project_row, EventResponse};
use crate::query_front_door::{
    execute_event_query, EventQueryFrontDoorError, EventQueryResponse, MISSING_COVERAGE_MESSAGE,
    RESULT_CAP_MESSAGE,
};
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
    let limit = q.limit.unwrap_or(100).min(10_000) as u64;
    let plan = FilterPlan::from_rest_params(
        q.contract,
        q.event,
        q.topic0,
        q.from_block,
        q.to_block,
        limit,
        q.offset.unwrap_or(0),
    );

    let response = execute_event_query(&state.db, plan)
        .await
        .map_err(|err| match err {
            EventQueryFrontDoorError::MissingAddress => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "missing REST query plan".to_string(),
            ),
            EventQueryFrontDoorError::Unsupported { reason } => {
                (axum::http::StatusCode::BAD_REQUEST, reason)
            }
            EventQueryFrontDoorError::Storage(e) => {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            }
        })?;

    match response {
        EventQueryResponse::TooManyResults { .. } => Err((
            axum::http::StatusCode::BAD_REQUEST,
            RESULT_CAP_MESSAGE.into(),
        )),
        EventQueryResponse::MissingCoverage => Err((
            axum::http::StatusCode::BAD_REQUEST,
            MISSING_COVERAGE_MESSAGE.into(),
        )),
        EventQueryResponse::NotIndexed | EventQueryResponse::Empty => Ok(Json(EventsResponse {
            events: vec![],
            count: 0,
        })),
        EventQueryResponse::Results(rows) => {
            // Every row is returned; degraded rows carry an explicit
            // `decode_quality` field instead of a silent fallback.
            let events: Vec<EventResponse> =
                rows.iter().map(|row| project_row(row).event).collect();
            let count = events.len();
            Ok(Json(EventsResponse { events, count }))
        }
    }
}

async fn get_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<StatusResponse>, (axum::http::StatusCode, String)> {
    // The storage status read module is the single source of indexed status
    // facts; this adapter only projects them into the REST response shape.
    let summary = state
        .db
        .status_summary()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(StatusResponse {
        block_number: summary.latest_block.unwrap_or(0),
        contract_count: summary.contract_count as usize,
        event_count: summary.event_count,
    }))
}

async fn get_contracts(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ContractsResponse>, (axum::http::StatusCode, String)> {
    let summary = state
        .db
        .status_summary()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let contracts = summary
        .contracts
        .into_iter()
        .map(|contract| ContractResponse {
            address: contract.address,
            name: contract.name,
            event_count: contract.total_events,
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

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;

    info!(port, "REST server started at http://127.0.0.1:{port}");

    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("REST server exited unexpectedly");
    });

    Ok(())
}
