//! Scan local historical source files and persist coverage metadata.

use anyhow::{bail, Context, Result};
use scopenode_core::config::{Config, SourceKind};
use scopenode_core::source::{scan_era1_source, ChecksumStatus, SourceScan};
use scopenode_storage::models::NewSourceFile;
use scopenode_storage::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockRange {
    from: u64,
    to: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeReport {
    label: String,
    requested_from: u64,
    requested_to: Option<u64>,
    missing: Vec<BlockRange>,
}

pub async fn run(cfg: Config, db: Db) -> Result<()> {
    let source = cfg
        .source
        .as_ref()
        .context("index requires a [source] config section")?;

    let scan = match source.kind {
        SourceKind::Era1 => scan_era1_source(&source.path, source.network.as_deref())
            .with_context(|| format!("Failed to scan ERA1 source: {}", source.path.display()))?,
    };

    persist_scan(&db, &scan).await?;
    let scopes = build_scope_reports(&cfg, &scan);
    print_report(&scan, &scopes);

    let mismatch_count = scan
        .files
        .iter()
        .filter(|file| file.checksum_status.is_mismatched())
        .count();
    let missing_scope_count = scopes
        .iter()
        .filter(|scope| !scope.missing.is_empty())
        .count();

    if mismatch_count > 0 || missing_scope_count > 0 {
        bail!(
            "index blocked: {mismatch_count} checksum mismatch(es), {missing_scope_count} scope(s) missing coverage"
        );
    }

    Ok(())
}

async fn persist_scan(db: &Db, scan: &SourceScan) -> Result<()> {
    let source_id = db
        .upsert_source(
            &scan.kind,
            scan.network.as_deref(),
            &scan.path.display().to_string(),
        )
        .await
        .context("Failed to persist source manifest")?;

    for file in &scan.files {
        let input = NewSourceFile {
            format: file.format.clone(),
            path: file.path.display().to_string(),
            filename: file.filename.clone(),
            network: file.network.clone(),
            epoch: Some(file.epoch as i64),
            file_hash: Some(file.file_hash.clone()),
            size_bytes: file.size_bytes as i64,
            modified_at: file.modified_at,
            sha256: file.sha256.clone(),
            checksum_status: file.checksum_status.as_str().to_string(),
            expected_sha256: file
                .checksum_status
                .expected_sha256()
                .map(ToOwned::to_owned),
        };
        let file_id = db
            .upsert_source_file(source_id, &input)
            .await
            .with_context(|| format!("Failed to persist source file {}", file.filename))?;
        let ranges = file
            .ranges
            .iter()
            .map(|range| {
                (
                    range.from_block,
                    range.to_block,
                    range.completeness.as_str(),
                )
            })
            .collect::<Vec<_>>();
        db.replace_source_file_ranges(file_id, &ranges)
            .await
            .with_context(|| format!("Failed to persist source ranges for {}", file.filename))?;
    }

    Ok(())
}

fn build_scope_reports(cfg: &Config, scan: &SourceScan) -> Vec<ScopeReport> {
    let mut available = scan
        .files
        .iter()
        .flat_map(|file| {
            file.ranges.iter().map(|range| BlockRange {
                from: range.from_block,
                to: range.to_block,
            })
        })
        .collect::<Vec<_>>();
    available.sort_by_key(|range| (range.from, range.to));

    cfg.contracts
        .iter()
        .map(|contract| {
            let label = contract
                .name
                .clone()
                .unwrap_or_else(|| contract.address.to_string());
            let missing = match contract.to_block {
                Some(to) => missing_ranges(contract.from_block, to, &available),
                None => vec![BlockRange {
                    from: contract.from_block,
                    to: u64::MAX,
                }],
            };
            ScopeReport {
                label,
                requested_from: contract.from_block,
                requested_to: contract.to_block,
                missing,
            }
        })
        .collect()
}

fn missing_ranges(from: u64, to: u64, available: &[BlockRange]) -> Vec<BlockRange> {
    if to < from {
        return vec![BlockRange { from, to }];
    }

    let mut cursor = from;
    let mut missing = Vec::new();
    for range in available {
        if range.to < cursor {
            continue;
        }
        if range.from > to {
            break;
        }
        if range.from > cursor {
            missing.push(BlockRange {
                from: cursor,
                to: range.from.saturating_sub(1).min(to),
            });
        }
        if range.to >= to {
            return missing;
        }
        cursor = range.to.saturating_add(1);
    }

    if cursor <= to {
        missing.push(BlockRange { from: cursor, to });
    }
    missing
}

fn print_report(scan: &SourceScan, scopes: &[ScopeReport]) {
    println!("Source manifest");
    println!("  kind: {}", scan.kind);
    println!("  path: {}", scan.path.display());
    println!(
        "  network: {}",
        scan.network.as_deref().unwrap_or("unknown")
    );
    println!("  files: {}", scan.files.len());

    let verified = scan
        .files
        .iter()
        .filter(|file| matches!(file.checksum_status, ChecksumStatus::Verified))
        .count();
    let mismatched = scan
        .files
        .iter()
        .filter(|file| matches!(file.checksum_status, ChecksumStatus::Mismatched { .. }))
        .count();
    let missing = scan
        .files
        .iter()
        .filter(|file| matches!(file.checksum_status, ChecksumStatus::Missing))
        .count();
    let unavailable = scan
        .files
        .iter()
        .filter(|file| matches!(file.checksum_status, ChecksumStatus::Unavailable))
        .count();
    println!(
        "  checksums: {verified} verified, {mismatched} mismatched, {missing} missing, {unavailable} unavailable"
    );

    for file in &scan.files {
        let ranges = file
            .ranges
            .iter()
            .map(|range| format!("{}..{}", range.from_block, range.to_block))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  - {}: {} epoch {} {} [{}]",
            file.filename,
            file.network,
            file.epoch,
            checksum_label(&file.checksum_status),
            ranges
        );
    }

    println!("Scopes");
    for scope in scopes {
        let requested = match scope.requested_to {
            Some(to) => format!("{}..{}", scope.requested_from, to),
            None => format!("{}..open", scope.requested_from),
        };
        if scope.missing.is_empty() {
            println!("  - {}: covered {}", scope.label, requested);
        } else {
            let missing = scope
                .missing
                .iter()
                .map(format_missing_range)
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "  - {}: missing {} (requested {})",
                scope.label, missing, requested
            );
        }
    }
}

fn checksum_label(status: &ChecksumStatus) -> &'static str {
    match status {
        ChecksumStatus::Verified => "checksum verified",
        ChecksumStatus::Mismatched { .. } => "checksum mismatched",
        ChecksumStatus::Missing => "checksum missing",
        ChecksumStatus::Unavailable => "checksum unavailable",
    }
}

fn format_missing_range(range: &BlockRange) -> String {
    if range.to == u64::MAX {
        format!("{}..open", range.from)
    } else {
        format!("{}..{}", range.from, range.to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use scopenode_core::config::{ContractConfig, NodeConfig};

    #[test]
    fn missing_ranges_reports_gap_after_available_range() {
        let missing = missing_ranges(0, 10_000, &[BlockRange { from: 0, to: 8191 }]);

        assert_eq!(
            missing,
            vec![BlockRange {
                from: 8192,
                to: 10_000
            }]
        );
    }

    #[test]
    fn missing_ranges_reports_covered_scope() {
        let missing = missing_ranges(0, 100, &[BlockRange { from: 0, to: 8191 }]);

        assert!(missing.is_empty());
    }

    #[test]
    fn build_scope_reports_marks_open_ended_scope_missing() {
        let cfg = Config {
            node: NodeConfig {
                port: 8545,
                data_dir: None,
                consensus_rpc: vec![],
                reorg_buffer: 64,
                execution_rpc: None,
                beacon_unverified_ack: false,
                beacon_fallback_unverified: false,
                allow_http_consensus_rpc: false,
                consensus_checkpoint: None,
                beacon_sync_timeout_secs: 300,
            },
            source: None,
            contracts: vec![ContractConfig {
                name: Some("scope".to_string()),
                address: address!("0000000000000000000000000000000000000001"),
                events: vec!["Transfer".to_string()],
                from_block: 0,
                to_block: None,
                abi_override: None,
                impl_address: None,
            }],
        };
        let scan = SourceScan {
            kind: "era1".to_string(),
            path: "fixtures/era1/mainnet".into(),
            network: Some("mainnet".to_string()),
            files: vec![],
        };

        let reports = build_scope_reports(&cfg, &scan);

        assert_eq!(
            reports[0].missing[0],
            BlockRange {
                from: 0,
                to: u64::MAX
            }
        );
    }
}
