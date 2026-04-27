//! `doctor` command — check node health.
//!
//! Reports: DB size, pending retry queue length, event count, contract sync
//! state, and beacon light-client status.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use scopenode_core::beacon::{read_beacon_status, BeaconStatusFile};
use scopenode_storage::Db;

/// Run the `doctor` command.
pub async fn run(db: Db, data_dir: &Path) -> Result<()> {
    let pending = db
        .count_pending_retry()
        .await
        .context("Failed to query retry queue")?;

    let size_bytes = db
        .db_size_bytes()
        .await
        .context("Failed to read DB size")?;

    let contracts = db
        .get_all_contracts()
        .await
        .context("Failed to query contracts")?;

    println!("DB size:       {:.1} MB", size_bytes as f64 / 1_048_576.0);
    println!("Contracts:     {}", contracts.len());
    println!("Retry queue:   {pending} block(s) pending");

    if pending > 0 {
        println!("  → Run `scopenode retry <config>` to re-fetch failed blocks.");
    }

    println!("{}", format_beacon_status(data_dir));

    Ok(())
}

fn format_beacon_status(data_dir: &Path) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let Some(file) = read_beacon_status(data_dir) else {
        return "beacon:        unknown  (sync not running or status file not found)".to_string();
    };

    let age_secs = now.saturating_sub(file.written_at_unix_secs());
    if age_secs > 30 {
        return format!(
            "beacon:        unknown  (status stale — last updated {}s ago, sync may not be running)",
            age_secs
        );
    }

    match &file {
        BeaconStatusFile::Synced { head_block, head_hash, .. } => {
            let hash_display = if head_hash.len() >= 10 {
                format!("{}...", &head_hash[..10])
            } else {
                head_hash.clone()
            };
            let rotation_h = next_rotation_hours(*head_block);
            format!(
                "beacon:        verified  head #{head_block}  ({hash_display})  next rotation ~{rotation_h:.1}h"
            )
        }
        BeaconStatusFile::Syncing { elapsed_secs, .. } => {
            format!(
                "beacon:        syncing  ({:.0}s elapsed)",
                elapsed_secs
            )
        }
        BeaconStatusFile::Error { message, .. } => {
            let msg = if message.len() > 80 { &message[..80] } else { message };
            format!("beacon:        error  {msg}")
        }
        BeaconStatusFile::Stalled { consecutive_mismatches, at_block, .. } => {
            format!(
                "beacon:        stalled  {consecutive_mismatches} consecutive mismatches at block {at_block}"
            )
        }
        BeaconStatusFile::NotConfigured { .. } => {
            "beacon:        not configured  (unverified mode)".to_string()
        }
        BeaconStatusFile::FallbackUnverified { .. } => {
            "beacon:        unverified  (fallback active — beacon_fallback_unverified=true)".to_string()
        }
    }
}

/// Estimate hours until the next sync committee rotation.
///
/// Each period is 8192 slots × 12 s = 98304 s ≈ 27.3 h.
fn next_rotation_hours(head_block: u64) -> f64 {
    const PERIOD_SLOTS: u64 = 8192;
    const SLOT_SECS: f64 = 12.0;
    let slots_into_period = head_block % PERIOD_SLOTS;
    let remaining_slots = PERIOD_SLOTS.saturating_sub(slots_into_period);
    (remaining_slots as f64 * SLOT_SECS) / 3600.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use scopenode_core::beacon::BeaconStatusFile;
    use tempfile::TempDir;

    fn write_status(dir: &TempDir, file: &BeaconStatusFile) {
        let json = serde_json::to_string(file).unwrap();
        std::fs::write(dir.path().join("beacon_status.json"), json).unwrap();
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    #[test]
    fn synced_output_contains_verified_and_block() {
        let dir = TempDir::new().unwrap();
        write_status(
            &dir,
            &BeaconStatusFile::Synced {
                head_block: 20_000_000,
                head_hash: "0xaabbccdd1122334455667788".to_string(),
                written_at_unix_secs: now_secs(),
            },
        );
        let out = format_beacon_status(dir.path());
        assert!(out.contains("verified"), "expected 'verified' in: {out}");
        assert!(out.contains("20000000"), "expected block in: {out}");
    }

    #[test]
    fn file_absent_gives_unknown() {
        let dir = TempDir::new().unwrap();
        let out = format_beacon_status(dir.path());
        assert!(out.contains("status file not found"), "expected not-found msg in: {out}");
    }

    #[test]
    fn stale_file_gives_stale_message() {
        let dir = TempDir::new().unwrap();
        write_status(
            &dir,
            &BeaconStatusFile::NotConfigured { written_at_unix_secs: now_secs() - 60 },
        );
        let out = format_beacon_status(dir.path());
        assert!(out.contains("stale"), "expected 'stale' in: {out}");
    }

    #[test]
    fn not_configured_output() {
        let dir = TempDir::new().unwrap();
        write_status(&dir, &BeaconStatusFile::NotConfigured { written_at_unix_secs: now_secs() });
        let out = format_beacon_status(dir.path());
        assert!(out.contains("not configured"), "expected 'not configured' in: {out}");
    }

    #[test]
    fn next_rotation_at_period_start() {
        let h = next_rotation_hours(0);
        let expected = (8192.0 * 12.0) / 3600.0;
        assert!((h - expected).abs() < 0.01);
    }

    #[test]
    fn next_rotation_at_period_end() {
        let h = next_rotation_hours(8191);
        let expected = (1.0 * 12.0) / 3600.0;
        assert!((h - expected).abs() < 0.01);
    }
}
