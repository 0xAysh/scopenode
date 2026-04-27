//! Beacon light client status, file persistence, and consensus pre-flight check.
//!
//! # In-process sharing
//! `BeaconStatus` is shared between `LiveSyncer` and the TUI via a
//! `tokio::sync::watch` channel (`BeaconStatusTx`).
//!
//! # Cross-process sharing
//! `doctor` runs as a separate process invocation and reads `beacon_status.json`
//! written by `LiveSyncer` on every state change.

use crate::error::CoreError;
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tracing::{info, warn};
use url::Url;

/// In-process beacon client state. Shared via `BeaconStatusTx`.
///
/// `Syncing` uses `Instant` for in-process elapsed-time computation.
/// The file-persistence counterpart is [`BeaconStatusFile`].
#[derive(Debug, Clone)]
pub enum BeaconStatus {
    /// No `consensus_rpc` configured; live headers are trusted from peers.
    NotConfigured,
    /// Helios is bootstrapping the sync committee.
    Syncing { started: Instant },
    /// Helios is tracking the canonical chain and verifying block headers.
    Synced { head_block: u64, head_hash: B256 },
    /// Three consecutive hash mismatches at the same block number.
    Stalled { consecutive_mismatches: u32, at_block: u64 },
    /// Five consecutive Helios query errors, or a bootstrap timeout with no fallback.
    Error(String),
    /// Bootstrap timed out and `beacon_fallback_unverified = true` — running
    /// without beacon verification. Live headers are trusted from peers.
    FallbackUnverified,
}

/// File-serializable version of [`BeaconStatus`] written by `LiveSyncer` and
/// read by `doctor`.
///
/// `written_at_unix_secs` is populated at write time so `doctor` can detect
/// a stale file (e.g. sync process died).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BeaconStatusFile {
    NotConfigured {
        written_at_unix_secs: u64,
    },
    Syncing {
        elapsed_secs: f64,
        written_at_unix_secs: u64,
    },
    Synced {
        head_block: u64,
        /// Hex-encoded B256.
        head_hash: String,
        written_at_unix_secs: u64,
    },
    Stalled {
        consecutive_mismatches: u32,
        at_block: u64,
        written_at_unix_secs: u64,
    },
    Error {
        message: String,
        written_at_unix_secs: u64,
    },
    FallbackUnverified {
        written_at_unix_secs: u64,
    },
}

impl BeaconStatusFile {
    pub fn written_at_unix_secs(&self) -> u64 {
        match self {
            Self::NotConfigured { written_at_unix_secs }
            | Self::Syncing { written_at_unix_secs, .. }
            | Self::Synced { written_at_unix_secs, .. }
            | Self::Stalled { written_at_unix_secs, .. }
            | Self::Error { written_at_unix_secs, .. }
            | Self::FallbackUnverified { written_at_unix_secs } => *written_at_unix_secs,
        }
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl From<&BeaconStatus> for BeaconStatusFile {
    fn from(s: &BeaconStatus) -> Self {
        let ts = now_unix_secs();
        match s {
            BeaconStatus::NotConfigured => Self::NotConfigured { written_at_unix_secs: ts },
            BeaconStatus::Syncing { started } => Self::Syncing {
                elapsed_secs: started.elapsed().as_secs_f64(),
                written_at_unix_secs: ts,
            },
            BeaconStatus::Synced { head_block, head_hash } => Self::Synced {
                head_block: *head_block,
                head_hash: format!("{head_hash:#x}"),
                written_at_unix_secs: ts,
            },
            BeaconStatus::Stalled { consecutive_mismatches, at_block } => Self::Stalled {
                consecutive_mismatches: *consecutive_mismatches,
                at_block: *at_block,
                written_at_unix_secs: ts,
            },
            BeaconStatus::Error(msg) => Self::Error {
                message: msg.clone(),
                written_at_unix_secs: ts,
            },
            BeaconStatus::FallbackUnverified => Self::FallbackUnverified { written_at_unix_secs: ts },
        }
    }
}

/// Shared sender for in-process beacon status updates.
pub type BeaconStatusTx = Arc<watch::Sender<BeaconStatus>>;

/// Write the current beacon status to `{data_dir}/beacon_status.json`.
///
/// Silently ignores I/O failures — a missing or unwritable status file must
/// not crash live sync.
pub fn write_beacon_status(data_dir: &Path, status: &BeaconStatus) {
    let file_status = BeaconStatusFile::from(status);
    let json = match serde_json::to_string(&file_status) {
        Ok(j) => j,
        Err(e) => {
            warn!("failed to serialize beacon status: {e}");
            return;
        }
    };
    let path = data_dir.join("beacon_status.json");
    if let Err(e) = std::fs::write(&path, json) {
        warn!("failed to write beacon status to {}: {e}", path.display());
    }
}

/// Read the beacon status file from `{data_dir}/beacon_status.json`.
///
/// Returns `None` if the file is absent or cannot be parsed.
pub fn read_beacon_status(data_dir: &Path) -> Option<BeaconStatusFile> {
    let path = data_dir.join("beacon_status.json");
    let content = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&content) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("failed to parse beacon status from {}: {e}", path.display());
            None
        }
    }
}

// ─── Pre-flight response structs ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BeaconBlockResponse {
    data: BeaconBlockData,
}

#[derive(Debug, Deserialize)]
struct BeaconBlockData {
    message: BeaconBlockMessage,
}

#[derive(Debug, Deserialize)]
struct BeaconBlockMessage {
    slot: serde_json::Value,
    body: BeaconBlockBody,
}

#[derive(Debug, Deserialize)]
struct BeaconBlockBody {
    execution_payload: ExecutionPayload,
}

#[derive(Debug, Deserialize)]
struct ExecutionPayload {
    block_hash: String,
}

fn sanitize_url(url: &Url) -> String {
    format!("{}://{}{}", url.scheme(), url.host_str().unwrap_or(""), url.path())
}

fn parse_slot(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// R3 pre-flight: check that all configured consensus RPC endpoints agree on
/// the latest execution block hash before starting Helios.
///
/// - Single-endpoint configs: logs an info notice and returns `Ok(())`.
/// - Multi-endpoint configs: queries all URLs concurrently, partitions by slot,
///   and errors if any two endpoints at the same slot report different hashes.
/// - Propagation tolerance: endpoints within 1 slot of the max are "current";
///   beyond that they are considered lagging (warn, not error).
pub async fn run_consensus_preflight(
    urls: &[Url],
    client: &reqwest::Client,
) -> Result<(), CoreError> {
    if urls.len() <= 1 {
        info!(
            "single consensus_rpc endpoint — pre-flight agreement check skipped \
             (no Byzantine fault tolerance)"
        );
        return Ok(());
    }

    // Query all endpoints concurrently with a 5s per-request timeout.
    let futs = urls.iter().map(|url| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let endpoint = format!("{}/eth/v2/beacon/blocks/head", url.as_str().trim_end_matches('/'));
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.get(&endpoint).send(),
            )
            .await;
            (url, result)
        }
    });

    let results = futures::future::join_all(futs).await;

    struct EndpointResult {
        url: Url,
        slot: u64,
        block_hash: String,
    }

    let mut responses: Vec<EndpointResult> = Vec::new();

    for (url, result) in results {
        let safe_url = sanitize_url(&url);
        let resp = match result {
            Err(_timeout) => {
                warn!("consensus pre-flight: {safe_url} timed out, skipping");
                continue;
            }
            Ok(Err(e)) => {
                warn!("consensus pre-flight: {safe_url} request failed ({e}), skipping");
                continue;
            }
            Ok(Ok(r)) => r,
        };

        if !resp.status().is_success() {
            warn!(
                "consensus pre-flight: {safe_url} returned HTTP {}, skipping",
                resp.status()
            );
            continue;
        }

        let body: BeaconBlockResponse = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                warn!("consensus pre-flight: {safe_url} JSON parse failed ({e}), skipping");
                continue;
            }
        };

        let slot = match parse_slot(&body.data.message.slot) {
            Some(s) => s,
            None => {
                warn!("consensus pre-flight: {safe_url} returned unparseable slot, skipping");
                continue;
            }
        };

        responses.push(EndpointResult {
            url,
            slot,
            block_hash: body.data.message.body.execution_payload.block_hash.clone(),
        });
    }

    if responses.len() < 2 {
        // Not enough live endpoints to compare; proceed with whatever is available.
        return Ok(());
    }

    let max_slot = responses.iter().map(|r| r.slot).max().unwrap_or(0);

    for r in &responses {
        if r.slot < max_slot.saturating_sub(1) {
            warn!(
                "consensus pre-flight: {} is lagging (slot {} vs max {})",
                sanitize_url(&r.url),
                r.slot,
                max_slot,
            );
        }
    }

    // Compare hashes only among endpoints within 1 slot of max_slot.
    let current: Vec<&EndpointResult> = responses
        .iter()
        .filter(|r| r.slot >= max_slot.saturating_sub(1))
        .collect();

    // Group by slot and compare within each slot group.
    use std::collections::HashMap;
    let mut by_slot: HashMap<u64, Vec<&EndpointResult>> = HashMap::new();
    for r in &current {
        by_slot.entry(r.slot).or_default().push(r);
    }

    for (slot, group) in &by_slot {
        if group.len() < 2 {
            continue;
        }
        let reference_hash = &group[0].block_hash;
        for other in group.iter().skip(1) {
            if &other.block_hash != reference_hash {
                return Err(CoreError::Internal(format!(
                    "consensus endpoints disagree on block hash at slot {slot}: \
                     {}={} vs {}={}",
                    sanitize_url(&group[0].url),
                    reference_hash,
                    sanitize_url(&other.url),
                    other.block_hash,
                )));
            }
        }
    }

    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn synced_status() -> BeaconStatus {
        BeaconStatus::Synced {
            head_block: 42,
            head_hash: B256::from([0xab; 32]),
        }
    }

    // ── BeaconStatusFile round-trip ───────────────────────────────────────────

    #[test]
    fn not_configured_round_trip() {
        let s = BeaconStatus::NotConfigured;
        let file: BeaconStatusFile = (&s).into();
        let json = serde_json::to_string(&file).unwrap();
        let back: BeaconStatusFile = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, BeaconStatusFile::NotConfigured { .. }));
    }

    #[test]
    fn syncing_round_trip() {
        let s = BeaconStatus::Syncing { started: Instant::now() };
        let file: BeaconStatusFile = (&s).into();
        let json = serde_json::to_string(&file).unwrap();
        let back: BeaconStatusFile = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, BeaconStatusFile::Syncing { elapsed_secs, .. } if elapsed_secs >= 0.0));
    }

    #[test]
    fn synced_round_trip() {
        let s = synced_status();
        let file: BeaconStatusFile = (&s).into();
        let json = serde_json::to_string(&file).unwrap();
        let back: BeaconStatusFile = serde_json::from_str(&json).unwrap();
        match back {
            BeaconStatusFile::Synced { head_block, head_hash, .. } => {
                assert_eq!(head_block, 42);
                assert!(head_hash.contains("abab"), "hash should contain ab bytes: {head_hash}");
            }
            other => panic!("expected Synced, got {other:?}"),
        }
    }

    #[test]
    fn stalled_round_trip() {
        let s = BeaconStatus::Stalled { consecutive_mismatches: 3, at_block: 100 };
        let file: BeaconStatusFile = (&s).into();
        let json = serde_json::to_string(&file).unwrap();
        let back: BeaconStatusFile = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, BeaconStatusFile::Stalled { consecutive_mismatches: 3, at_block: 100, .. })
        );
    }

    #[test]
    fn error_round_trip() {
        let s = BeaconStatus::Error("helios error".to_string());
        let file: BeaconStatusFile = (&s).into();
        let json = serde_json::to_string(&file).unwrap();
        let back: BeaconStatusFile = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, BeaconStatusFile::Error { message, .. } if message == "helios error"));
    }

    #[test]
    fn fallback_unverified_round_trip() {
        let s = BeaconStatus::FallbackUnverified;
        let file: BeaconStatusFile = (&s).into();
        let json = serde_json::to_string(&file).unwrap();
        let back: BeaconStatusFile = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, BeaconStatusFile::FallbackUnverified { .. }));
    }

    // ── File persistence ──────────────────────────────────────────────────────

    #[test]
    fn write_then_read_round_trip() {
        let dir = TempDir::new().unwrap();
        let status = synced_status();
        write_beacon_status(dir.path(), &status);
        let back = read_beacon_status(dir.path()).expect("file should exist");
        assert!(matches!(back, BeaconStatusFile::Synced { head_block: 42, .. }));
    }

    #[test]
    fn read_absent_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_beacon_status(dir.path()).is_none());
    }

    // ── Pre-flight: single endpoint ───────────────────────────────────────────

    #[tokio::test]
    async fn single_endpoint_skips_comparison() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/eth/v2/beacon/blocks/head"))
            .respond_with(ResponseTemplate::new(200).set_body_json(beacon_response("1000", "0xaaa")))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let urls = vec![Url::parse(&server.uri()).unwrap()];
        run_consensus_preflight(&urls, &client).await.unwrap();
    }

    // ── Pre-flight: two endpoints agreeing ───────────────────────────────────

    #[tokio::test]
    async fn two_endpoints_same_slot_same_hash_ok() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;
        mount_beacon_response(&server1, "1000", "0xabc123").await;
        mount_beacon_response(&server2, "1000", "0xabc123").await;
        let client = reqwest::Client::new();
        let urls = vec![
            Url::parse(&server1.uri()).unwrap(),
            Url::parse(&server2.uri()).unwrap(),
        ];
        run_consensus_preflight(&urls, &client).await.unwrap();
    }

    // ── Pre-flight: propagation lag (slots differ by 1) ───────────────────────

    #[tokio::test]
    async fn two_endpoints_adjacent_slots_ok() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;
        mount_beacon_response(&server1, "1001", "0xnewer").await;
        mount_beacon_response(&server2, "1000", "0xolder").await;
        let client = reqwest::Client::new();
        let urls = vec![
            Url::parse(&server1.uri()).unwrap(),
            Url::parse(&server2.uri()).unwrap(),
        ];
        // Adjacent slots — no same-slot hash comparison possible → ok
        run_consensus_preflight(&urls, &client).await.unwrap();
    }

    // ── Pre-flight: two endpoints disagreeing at same slot ───────────────────

    #[tokio::test]
    async fn two_endpoints_same_slot_different_hash_errors() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;
        mount_beacon_response(&server1, "1000", "0xhash_a").await;
        mount_beacon_response(&server2, "1000", "0xhash_b").await;
        let client = reqwest::Client::new();
        let urls = vec![
            Url::parse(&server1.uri()).unwrap(),
            Url::parse(&server2.uri()).unwrap(),
        ];
        let err = run_consensus_preflight(&urls, &client).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("disagree on block hash"), "unexpected error: {msg}");
    }

    // ── Pre-flight: one endpoint HTTP 500 ────────────────────────────────────

    #[tokio::test]
    async fn one_endpoint_http_500_skipped() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;
        mount_beacon_response(&server1, "1000", "0xabc").await;
        Mock::given(method("GET"))
            .and(path("/eth/v2/beacon/blocks/head"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server2)
            .await;
        let client = reqwest::Client::new();
        let urls = vec![
            Url::parse(&server1.uri()).unwrap(),
            Url::parse(&server2.uri()).unwrap(),
        ];
        // Only 1 live response → no comparison → ok
        run_consensus_preflight(&urls, &client).await.unwrap();
    }

    // ── Pre-flight: all endpoints unreachable ─────────────────────────────────

    #[tokio::test]
    async fn all_endpoints_fail_ok() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/eth/v2/beacon/blocks/head"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server1)
            .await;
        Mock::given(method("GET"))
            .and(path("/eth/v2/beacon/blocks/head"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server2)
            .await;
        let client = reqwest::Client::new();
        let urls = vec![
            Url::parse(&server1.uri()).unwrap(),
            Url::parse(&server2.uri()).unwrap(),
        ];
        run_consensus_preflight(&urls, &client).await.unwrap();
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn beacon_response(slot: &str, block_hash: &str) -> serde_json::Value {
        serde_json::json!({
            "data": {
                "message": {
                    "slot": slot,
                    "body": {
                        "execution_payload": {
                            "block_hash": block_hash
                        }
                    }
                }
            }
        })
    }

    async fn mount_beacon_response(server: &MockServer, slot: &str, block_hash: &str) {
        Mock::given(method("GET"))
            .and(path("/eth/v2/beacon/blocks/head"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(beacon_response(slot, block_hash)),
            )
            .mount(server)
            .await;
    }
}
