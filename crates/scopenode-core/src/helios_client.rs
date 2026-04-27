//! HeliosGuard — lifecycle wrapper around the Helios beacon light client.
//!
//! `HeliosGuard::start()` builds and bootstraps the Helios client, then returns
//! a guard that provides `verify_block()` implementing the R2 5-case per-block
//! verification logic. Returns `None` when `consensus_rpc` is empty (unverified
//! mode).

use alloy::eips::BlockId;
use alloy::rpc::types::BlockNumberOrTag;
use alloy_primitives::B256;
use async_trait::async_trait;
use eyre::Result as EyreResult;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::beacon::{write_beacon_status, BeaconStatus, BeaconStatusTx};
use crate::config::NodeConfig;
use crate::error::CoreError;

// ─── Internal verifier trait (thin seam for testing) ─────────────────────────

/// Minimal async interface to the Helios client used by `HeliosGuard`.
///
/// The real implementation wraps `helios_ethereum::EthereumClient`; tests
/// provide a mock that controls returned block numbers and hashes.
#[async_trait]
pub(crate) trait HeliosHead: Send + Sync {
    async fn head_number(&self) -> EyreResult<u64>;
    async fn block_hash_at(&self, number: u64) -> EyreResult<Option<B256>>;
    async fn shutdown(&self);
}

// ─── Real implementation wrapping helios EthereumClient ──────────────────────

use helios_ethereum::{
    config::networks::Network, database::ConfigDB, EthereumClient, EthereumClientBuilder,
};
use std::sync::Arc;

struct LiveHelios(Arc<EthereumClient>);

#[async_trait]
impl HeliosHead for LiveHelios {
    async fn head_number(&self) -> EyreResult<u64> {
        let n = self.0.get_block_number().await?;
        Ok(n.to::<u64>())
    }

    async fn block_hash_at(&self, number: u64) -> EyreResult<Option<B256>> {
        let block_id = BlockId::Number(BlockNumberOrTag::Number(number));
        let block = self.0.get_block(block_id, false).await?;
        Ok(block.map(|b| b.header.hash))
    }

    async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}

// ─── DEFAULT_CHECKPOINT ───────────────────────────────────────────────────────

/// Mainnet finalized checkpoint used when no `consensus_checkpoint` is configured.
///
/// **Must be updated each release** to a finalized epoch hash no older than 2
/// sync committee periods (~54 hours). Obtain with:
///   `cast block finalized --rpc-url <trusted-mainnet-rpc> | grep hash`
/// Cross-verify against a second independent source before committing.
///
/// Source for this value: helios 0.11.1 built-in mainnet default.
const DEFAULT_CHECKPOINT: &str =
    "9b41a80f58c52068a00e8535b8d6704769c7577a5fd506af5e0c018687991d55";

// ─── VerifyDecision ───────────────────────────────────────────────────────────

/// Decision returned by [`HeliosGuard::verify_block`].
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyDecision {
    /// Process the block normally and advance `our_tip`.
    Accept,
    /// Skip this block; do not advance `our_tip`. Retry on the next poll.
    Discard,
    /// Stop the live sync poll loop and return an error.
    Halt,
}

// ─── HeliosGuard ─────────────────────────────────────────────────────────────

/// Wraps the Helios lifecycle and implements R2 per-block header verification.
pub struct HeliosGuard {
    client: Box<dyn HeliosHead>,
    mismatch_counter: u32,
    error_counter: u32,
    last_mismatched_block: Option<u64>,
}

impl HeliosGuard {
    fn new(client: Box<dyn HeliosHead>) -> Self {
        Self {
            client,
            mismatch_counter: 0,
            error_counter: 0,
            last_mismatched_block: None,
        }
    }

    /// Build and start the Helios beacon client.
    ///
    /// Returns `None` when `consensus_rpc` is empty (unverified mode). The
    /// pre-flight check (R3) is run for multi-endpoint configs before building.
    pub async fn start(
        config: &NodeConfig,
        status_tx: &BeaconStatusTx,
        data_dir: Option<&Path>,
        reqwest_client: &reqwest::Client,
    ) -> Result<Option<HeliosGuard>, CoreError> {
        if config.consensus_rpc.is_empty() {
            return Ok(None);
        }

        // R3 pre-flight agreement check for multi-endpoint configs.
        if config.consensus_rpc.len() > 1 {
            crate::beacon::run_consensus_preflight(&config.consensus_rpc, reqwest_client).await?;
        } else {
            info!("single consensus_rpc endpoint — pre-flight agreement check skipped (no Byzantine fault tolerance)");
        }

        let checkpoint = parse_checkpoint(config)?;

        let execution_rpc = config
            .execution_rpc
            .as_ref()
            .expect("execution_rpc validated present when consensus_rpc non-empty");

        let client: EthereumClient =
            EthereumClientBuilder::<ConfigDB>::new()
                .network(Network::Mainnet)
                .consensus_rpc(config.consensus_rpc[0].as_str())
                .map_err(|e| CoreError::Internal(format!("invalid consensus_rpc: {e}")))?
                .execution_rpc(execution_rpc.as_str())
                .map_err(|e| CoreError::Internal(format!("invalid execution_rpc: {e}")))?
                .checkpoint(checkpoint)
                .build()
                .map_err(|e| CoreError::Internal(format!("failed to build Helios client: {e}")))?;

        let client = Arc::new(client);

        update_status(
            status_tx,
            data_dir,
            BeaconStatus::Syncing { started: Instant::now() },
        );

        let sync_timeout = Duration::from_secs(config.beacon_sync_timeout_secs);

        match timeout(sync_timeout, client.wait_synced()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(CoreError::Internal(format!("Helios sync failed: {e}")));
            }
            Err(_elapsed) => {
                let msg = format!(
                    "helios did not sync within {}s — cannot start live sync in verified mode. \
                     Check your consensus_rpc and execution_rpc endpoints. \
                     To proceed without verification (not recommended), set \
                     beacon_fallback_unverified = true in config.toml.",
                    config.beacon_sync_timeout_secs
                );
                if config.beacon_fallback_unverified {
                    warn!("{msg}");
                    update_status(status_tx, data_dir, BeaconStatus::FallbackUnverified);
                    return Ok(None);
                }
                error!("{msg}");
                return Err(CoreError::Internal(msg));
            }
        }

        // Retry loop for first head fetch — Helios may return OutOfSync immediately
        // after wait_synced() if no fresh block has arrived yet (block age > 60s).
        let head_number = fetch_head_with_retry(client.as_ref()).await?;

        // We don't have a hash at this point without an extra query — seed with
        // B256::ZERO; the first verify_block() call will update it properly.
        update_status(
            status_tx,
            data_dir,
            BeaconStatus::Synced {
                head_block: head_number,
                head_hash: B256::ZERO,
            },
        );

        Ok(Some(HeliosGuard::new(Box::new(LiveHelios(client)))))
    }

    /// R2 per-block header verification.
    ///
    /// Returns the action LiveSyncer should take for this block.
    pub async fn verify_block(
        &mut self,
        peer_num: u64,
        peer_hash: B256,
        current_status: &BeaconStatus,
        status_tx: &BeaconStatusTx,
        data_dir: Option<&Path>,
    ) -> Result<VerifyDecision, CoreError> {
        let helios_num = match self.client.head_number().await {
            Ok(n) => n,
            Err(e) => {
                warn!("Helios head_number error: {e}");
                self.error_counter += 1;
                if self.error_counter >= 5 {
                    let msg = format!("Helios query failed 5 consecutive times: {e}");
                    error!("{msg}");
                    update_status(status_tx, data_dir, BeaconStatus::Error(msg));
                    return Ok(VerifyDecision::Halt);
                }
                return Ok(VerifyDecision::Discard);
            }
        };

        if peer_num > helios_num {
            // Case 4: Helios is behind the tip (normal — 1-2 block lag expected).
            if peer_num.saturating_sub(helios_num) > 4 {
                warn!(
                    "Helios is lagging: peer at block {peer_num}, Helios at {helios_num} \
                     ({} blocks behind)",
                    peer_num - helios_num
                );
            }
            // When Stalled, keep our_tip frozen — do not accept even provisional blocks.
            if matches!(current_status, BeaconStatus::Stalled { .. }) {
                return Ok(VerifyDecision::Discard);
            }
            return Ok(VerifyDecision::Accept);
        }

        // Case 3/5: peer_num <= helios_num — query Helios for the canonical hash.
        let helios_hash = match self.client.block_hash_at(peer_num).await {
            Ok(Some(h)) => h,
            Ok(None) => {
                warn!("Helios returned no block at {peer_num}, treating as error");
                self.error_counter += 1;
                if self.error_counter >= 5 {
                    let msg = format!("Helios returned no block at {peer_num} (5 consecutive errors)");
                    error!("{msg}");
                    update_status(status_tx, data_dir, BeaconStatus::Error(msg));
                    return Ok(VerifyDecision::Halt);
                }
                return Ok(VerifyDecision::Discard);
            }
            Err(e) => {
                warn!("Helios block_hash_at({peer_num}) error: {e}");
                self.error_counter += 1;
                if self.error_counter >= 5 {
                    let msg = format!("Helios query failed 5 consecutive times: {e}");
                    error!("{msg}");
                    update_status(status_tx, data_dir, BeaconStatus::Error(msg));
                    return Ok(VerifyDecision::Halt);
                }
                return Ok(VerifyDecision::Discard);
            }
        };

        if helios_hash == peer_hash {
            // Match — decay counters and possibly recover from Stalled/Error.
            self.mismatch_counter = self.mismatch_counter.saturating_sub(1);
            self.error_counter = self.error_counter.saturating_sub(1);
            if self.mismatch_counter == 0 {
                self.last_mismatched_block = None;
            }
            if matches!(current_status, BeaconStatus::Stalled { .. } | BeaconStatus::Error(_)) {
                update_status(
                    status_tx,
                    data_dir,
                    BeaconStatus::Synced {
                        head_block: helios_num,
                        head_hash: helios_hash,
                    },
                );
            } else {
                // Keep Synced status current.
                update_status(
                    status_tx,
                    data_dir,
                    BeaconStatus::Synced {
                        head_block: helios_num,
                        head_hash: helios_hash,
                    },
                );
            }
            Ok(VerifyDecision::Accept)
        } else {
            // Mismatch — discard, track consecutive mismatches.
            warn!(
                "Beacon hash mismatch at block {peer_num}: \
                 peer={peer_hash:#x}, helios={helios_hash:#x}"
            );
            self.mismatch_counter += 1;
            self.last_mismatched_block = Some(peer_num);

            if self.mismatch_counter >= 3
                && self.last_mismatched_block == Some(peer_num)
            {
                error!(
                    "3 consecutive hash mismatches at block {peer_num} — stalling live sync"
                );
                update_status(
                    status_tx,
                    data_dir,
                    BeaconStatus::Stalled {
                        consecutive_mismatches: self.mismatch_counter,
                        at_block: peer_num,
                    },
                );
            }

            Ok(VerifyDecision::Discard)
        }
    }

    /// Shut down the Helios background consensus task.
    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn update_status(
    status_tx: &BeaconStatusTx,
    data_dir: Option<&Path>,
    status: BeaconStatus,
) {
    // send_replace always stores the new value regardless of receiver count,
    // which is correct here — we want the status file and TUI to reflect the
    // latest state even when no TUI is attached (e.g. non-interactive mode).
    status_tx.send_replace(status.clone());
    if let Some(dir) = data_dir {
        write_beacon_status(dir, &status);
    }
}

fn parse_checkpoint(config: &NodeConfig) -> Result<B256, CoreError> {
    debug_assert_ne!(
        DEFAULT_CHECKPOINT,
        "0x...",
        "DEFAULT_CHECKPOINT placeholder not replaced — update before release"
    );

    let hex = config
        .consensus_checkpoint
        .as_deref()
        .unwrap_or(DEFAULT_CHECKPOINT);

    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    let bytes = hex::decode(hex).map_err(|e| {
        CoreError::Internal(format!("invalid consensus_checkpoint hex: {e}"))
    })?;
    if bytes.len() != 32 {
        return Err(CoreError::Internal(format!(
            "consensus_checkpoint must be 32 bytes, got {}",
            bytes.len()
        )));
    }
    Ok(B256::from_slice(&bytes))
}

/// Retry `head_number()` up to 6 times with 5s backoff to handle `OutOfSync`
/// immediately after `wait_synced()`.
async fn fetch_head_with_retry(client: &EthereumClient) -> Result<u64, CoreError> {
    let mut last_err = String::new();
    for attempt in 0..6u32 {
        match client.get_block_number().await {
            Ok(n) => return Ok(n.to::<u64>()),
            Err(e) => {
                last_err = e.to_string();
                if attempt < 5 {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
    Err(CoreError::Internal(format!(
        "Helios not ready after 30s post-sync: {last_err}"
    )))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacon::BeaconStatus;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::sync::watch;

    // ── Mock HeliosHead ────────────────────────────────────────────────────────

    struct MockHelios {
        head: u64,
        blocks: HashMap<u64, Option<B256>>,
        /// If Some, `head_number()` returns this error.
        head_err: Option<String>,
        /// If Some, `block_hash_at()` returns this error.
        block_err: Option<String>,
    }

    impl MockHelios {
        fn new(head: u64) -> Self {
            Self {
                head,
                blocks: HashMap::new(),
                head_err: None,
                block_err: None,
            }
        }

        fn with_block(mut self, number: u64, hash: B256) -> Self {
            self.blocks.insert(number, Some(hash));
            self
        }

    }

    #[async_trait]
    impl HeliosHead for Mutex<MockHelios> {
        async fn head_number(&self) -> EyreResult<u64> {
            let inner = self.lock().unwrap();
            if let Some(err) = &inner.head_err {
                return Err(eyre::eyre!("{}", err));
            }
            Ok(inner.head)
        }

        async fn block_hash_at(&self, number: u64) -> EyreResult<Option<B256>> {
            let inner = self.lock().unwrap();
            if let Some(err) = &inner.block_err {
                return Err(eyre::eyre!("{}", err));
            }
            Ok(inner.blocks.get(&number).copied().flatten())
        }

        async fn shutdown(&self) {}
    }

    fn mock_guard(head: u64, blocks: Vec<(u64, B256)>) -> HeliosGuard {
        let mut mock = MockHelios::new(head);
        for (n, h) in blocks {
            mock = mock.with_block(n, h);
        }
        HeliosGuard::new(Box::new(Mutex::new(mock)))
    }

    fn mock_guard_no_blocks(head: u64) -> HeliosGuard {
        HeliosGuard::new(Box::new(Mutex::new(MockHelios::new(head))))
    }

    fn dummy_status_tx() -> BeaconStatusTx {
        let (tx, _rx) = watch::channel(BeaconStatus::NotConfigured);
        Arc::new(tx)
    }

    // ── verify_block tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn accept_when_hashes_match() {
        let hash = B256::from([0xaa; 32]);
        let mut guard = mock_guard(100, vec![(100, hash)]);
        let tx = dummy_status_tx();
        let status = BeaconStatus::Synced { head_block: 100, head_hash: hash };
        let decision = guard
            .verify_block(100, hash, &status, &tx, None)
            .await
            .unwrap();
        assert_eq!(decision, VerifyDecision::Accept);
        assert_eq!(guard.mismatch_counter, 0);
    }

    #[tokio::test]
    async fn discard_on_first_hash_mismatch() {
        let helios_hash = B256::from([0xaa; 32]);
        let peer_hash = B256::from([0xbb; 32]);
        let mut guard = mock_guard(100, vec![(100, helios_hash)]);
        let tx = dummy_status_tx();
        let status = BeaconStatus::Synced { head_block: 100, head_hash: helios_hash };
        let decision = guard
            .verify_block(100, peer_hash, &status, &tx, None)
            .await
            .unwrap();
        assert_eq!(decision, VerifyDecision::Discard);
        assert_eq!(guard.mismatch_counter, 1);
    }

    #[tokio::test]
    async fn stalled_after_three_consecutive_mismatches() {
        let helios_hash = B256::from([0xaa; 32]);
        let peer_hash = B256::from([0xbb; 32]);
        let mut guard = mock_guard(100, vec![(100, helios_hash)]);
        let tx = dummy_status_tx();
        let mut status = BeaconStatus::Synced { head_block: 100, head_hash: helios_hash };

        for _ in 0..3 {
            let d = guard
                .verify_block(100, peer_hash, &status, &tx, None)
                .await
                .unwrap();
            assert_eq!(d, VerifyDecision::Discard);
            status = tx.borrow().clone();
        }

        assert_eq!(guard.mismatch_counter, 3);
        assert!(matches!(*tx.borrow(), BeaconStatus::Stalled { .. }));
    }

    #[tokio::test]
    async fn recovers_from_stalled_on_match() {
        let helios_hash = B256::from([0xaa; 32]);
        let peer_hash = B256::from([0xbb; 32]);
        let mut guard = mock_guard(100, vec![(100, helios_hash)]);
        let tx = dummy_status_tx();
        let mut status = BeaconStatus::Synced { head_block: 100, head_hash: helios_hash };

        // Drive to Stalled.
        for _ in 0..3 {
            guard
                .verify_block(100, peer_hash, &status, &tx, None)
                .await
                .unwrap();
            status = tx.borrow().clone();
        }
        assert!(matches!(status, BeaconStatus::Stalled { .. }));

        // Now matching hash — should recover (counter decays toward 0).
        // After 3 mismatches, need 3 matches to reach counter = 0.
        for _ in 0..3 {
            let d = guard
                .verify_block(100, helios_hash, &status, &tx, None)
                .await
                .unwrap();
            assert_eq!(d, VerifyDecision::Accept);
            status = tx.borrow().clone();
        }
        assert!(matches!(status, BeaconStatus::Synced { .. }));
        assert_eq!(guard.mismatch_counter, 0);
    }

    #[tokio::test]
    async fn halt_after_five_consecutive_errors() {
        let mut mock = MockHelios::new(100);
        mock.head_err = Some("OutOfSync".to_string());
        let mut guard = HeliosGuard::new(Box::new(Mutex::new(mock)));
        let tx = dummy_status_tx();
        let status = BeaconStatus::Synced { head_block: 100, head_hash: B256::ZERO };

        for i in 0..5 {
            let d = guard
                .verify_block(100, B256::ZERO, &status, &tx, None)
                .await
                .unwrap();
            if i < 4 {
                assert_eq!(d, VerifyDecision::Discard, "iteration {i}");
            } else {
                assert_eq!(d, VerifyDecision::Halt);
                assert!(matches!(*tx.borrow(), BeaconStatus::Error(_)));
            }
        }
    }

    #[tokio::test]
    async fn provisional_accept_when_peer_ahead() {
        let mut guard = mock_guard_no_blocks(99);
        let tx = dummy_status_tx();
        let status = BeaconStatus::Synced { head_block: 99, head_hash: B256::ZERO };
        // peer_num (100) > helios_num (99) → provisional accept
        let d = guard
            .verify_block(100, B256::from([0xcc; 32]), &status, &tx, None)
            .await
            .unwrap();
        assert_eq!(d, VerifyDecision::Accept);
        assert_eq!(guard.mismatch_counter, 0);
    }

    #[tokio::test]
    async fn provisional_discarded_when_stalled() {
        let mut guard = mock_guard_no_blocks(99);
        let tx = dummy_status_tx();
        let status = BeaconStatus::Stalled { consecutive_mismatches: 3, at_block: 98 };
        // Even though peer is ahead, Stalled freezes our_tip.
        let d = guard
            .verify_block(100, B256::from([0xcc; 32]), &status, &tx, None)
            .await
            .unwrap();
        assert_eq!(d, VerifyDecision::Discard);
    }

    // ── parse_checkpoint ─────────────────────────────────────────────────────

    #[test]
    fn parse_checkpoint_from_config() {
        let mut node = test_node_config();
        // 32 bytes = 64 hex chars, prefixed with 0x
        node.consensus_checkpoint = Some(format!("0x{}", "aabbccdd".repeat(8)));
        let r = parse_checkpoint(&node);
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn parse_checkpoint_uses_default_when_absent() {
        let node = test_node_config();
        let r = parse_checkpoint(&node);
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn parse_checkpoint_rejects_wrong_length() {
        let mut node = test_node_config();
        node.consensus_checkpoint = Some("0xdeadbeef".to_string()); // 4 bytes
        let r = parse_checkpoint(&node);
        assert!(r.is_err());
    }

    fn test_node_config() -> NodeConfig {
        NodeConfig {
            port: 18545,
            data_dir: None,
            consensus_rpc: vec![],
            reorg_buffer: 64,
            execution_rpc: None,
            beacon_unverified_ack: false,
            beacon_fallback_unverified: false,
            allow_http_consensus_rpc: false,
            consensus_checkpoint: None,
            beacon_sync_timeout_secs: 300,
        }
    }
}
