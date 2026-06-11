//! Local historical source scanning.

pub use crate::codec::decode_era1_header;
use crate::e2store::read_era1_block_index_range;
pub use crate::e2store::read_era1_block_tuple;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use thiserror::Error;

pub const ERA1_BLOCKS_PER_FILE: u64 = 8192;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("source path does not exist: {0}")]
    MissingPath(PathBuf),

    #[error("source path is not a directory: {0}")]
    NotDirectory(PathBuf),

    #[error("failed to read source path {path}: {source}")]
    ReadDir { path: PathBuf, source: io::Error },

    #[error("failed to read source file metadata {path}: {source}")]
    Metadata { path: PathBuf, source: io::Error },

    #[error("failed to read source file {path}: {source}")]
    ReadFile { path: PathBuf, source: io::Error },

    #[error("invalid e2store source file {path}: {message}")]
    InvalidE2Store { path: PathBuf, message: String },

    #[error("block range overflow for {path}")]
    RangeOverflow { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceScan {
    pub kind: String,
    pub path: PathBuf,
    pub network: Option<String>,
    pub files: Vec<SourceFileManifest>,
}

#[derive(Debug, Clone)]
pub struct Era1Source {
    scan: SourceScan,
}

pub struct Era1BlockFactSelection {
    files: Vec<SourceFileManifest>,
    total_blocks: u64,
}

impl Era1BlockFactSelection {
    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

impl crate::era_pipeline::BlockFactStream for Era1BlockFactSelection {
    fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    /// One stream over every selected file in manifest order. File-open and
    /// per-block read failures surface as `Err` items carrying the file path;
    /// traversal continues with the next file. Format dispatch (`.era1` vs
    /// `.ere`) stays inside the reader this stream wraps, and decode errors
    /// are born carrying the file path and format identity.
    fn blocks(
        &self,
    ) -> Box<dyn Iterator<Item = Result<crate::era1_reader::Era1BlockFacts, SourceError>> + '_>
    {
        Box::new(self.files.iter().flat_map(|manifest| {
            match crate::era1_reader::iter_era1_block_facts(&manifest.path) {
                Ok(iter) => Box::new(iter)
                    as Box<
                        dyn Iterator<
                            Item = Result<crate::era1_reader::Era1BlockFacts, SourceError>,
                        >,
                    >,
                Err(e) => Box::new(std::iter::once(Err(e)))
                    as Box<
                        dyn Iterator<
                            Item = Result<crate::era1_reader::Era1BlockFacts, SourceError>,
                        >,
                    >,
            }
        }))
    }
}

impl Era1Source {
    pub fn scan(
        path: impl AsRef<Path>,
        network_override: Option<&str>,
        from_block: u64,
        to_block: u64,
    ) -> Result<Self, SourceError> {
        Ok(Self {
            scan: scan_era1_source(path, network_override, from_block, to_block)?,
        })
    }

    pub fn files(&self) -> &[SourceFileManifest] {
        &self.scan.files
    }

    pub fn block_facts_for_range(&self, from_block: u64, to_block: u64) -> Era1BlockFactSelection {
        self.block_facts_for_ranges(&[(from_block, to_block)])
    }

    pub fn block_facts_for_ranges(&self, ranges: &[(u64, u64)]) -> Era1BlockFactSelection {
        let files: Vec<SourceFileManifest> = self
            .scan
            .files
            .iter()
            .filter(|file| {
                file.ranges.iter().any(|file_range| {
                    ranges.iter().any(|(from_block, to_block)| {
                        file_range.to_block >= *from_block && file_range.from_block <= *to_block
                    })
                })
            })
            .cloned()
            .collect();

        let total_blocks = files
            .iter()
            .flat_map(|f| &f.ranges)
            .map(|r| r.to_block.saturating_sub(r.from_block) + 1)
            .sum();

        Era1BlockFactSelection {
            files,
            total_blocks,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFileManifest {
    pub format: String,
    pub path: PathBuf,
    pub filename: String,
    pub network: String,
    pub epoch: u64,
    pub file_hash: String,
    pub size_bytes: u64,
    pub modified_at: Option<i64>,
    pub sha256: String,
    pub checksum_status: ChecksumStatus,
    pub ranges: Vec<SourceRangeManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRangeManifest {
    pub from_block: u64,
    pub to_block: u64,
    pub completeness: RangeCompleteness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeCompleteness {
    Inferred,
    FileIndex,
}

impl RangeCompleteness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inferred => "inferred",
            Self::FileIndex => "file-index",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksumStatus {
    Verified,
    Mismatched { expected: String },
    Missing,
    Unavailable,
}

impl ChecksumStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Mismatched { .. } => "mismatched",
            Self::Missing => "missing",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn expected_sha256(&self) -> Option<&str> {
        match self {
            Self::Mismatched { expected } => Some(expected),
            Self::Verified | Self::Missing | Self::Unavailable => None,
        }
    }

    pub fn is_mismatched(&self) -> bool {
        matches!(self, Self::Mismatched { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Era1FileName {
    format: String,
    network: String,
    epoch: u64,
    file_hash: String,
    from_block: u64,
    to_block: u64,
}

/// Scan a directory for ERA1 files and return a manifest.
///
/// `from_block`/`to_block` define the union range of interest. Files whose
/// block range does not overlap `[from_block, to_block]` are excluded. SHA256
/// is only computed for files that pass the range filter (cheap seek-based check
/// runs first).
pub fn scan_era1_source(
    path: impl AsRef<Path>,
    network_override: Option<&str>,
    from_block: u64,
    to_block: u64,
) -> Result<SourceScan, SourceError> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(SourceError::MissingPath(path.to_owned()));
    }
    if !path.is_dir() {
        return Err(SourceError::NotDirectory(path.to_owned()));
    }

    let checksum_index = read_checksum_index(path);
    let mut entries = fs::read_dir(path)
        .map_err(|source| SourceError::ReadDir {
            path: path.to_owned(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| SourceError::ReadDir {
            path: path.to_owned(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());

    let mut files = Vec::new();
    for entry in entries {
        let file_path = entry.path();
        let Some(parsed) = parse_era1_filename(&file_path, network_override) else {
            continue;
        };
        let metadata = entry.metadata().map_err(|source| SourceError::Metadata {
            path: file_path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            continue;
        }

        // Read block range first (cheap seek-based pass) to filter out files
        // that don't overlap the requested range before computing SHA256.
        let content_range = read_era1_block_index_range(&file_path)?;

        let (file_from, file_to, completeness) = content_range
            .as_ref()
            .map(|range| (range.from_block, range.to_block, range.completeness))
            .unwrap_or((
                parsed.from_block,
                parsed.to_block,
                RangeCompleteness::Inferred,
            ));

        // Skip files that don't overlap [from_block, to_block].
        if file_to < from_block || file_from > to_block {
            continue;
        }

        // Only hash files when a checksum index is present. Hashing large ERA1
        // archives is expensive startup work and adds no value when there is
        // nothing to verify against.
        let sha256 = if checksum_index.available {
            sha256_file(&file_path)?
        } else {
            String::new()
        };

        let filename = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let checksum_status = checksum_index.status_for(&filename, parsed.epoch, &sha256);
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64);

        files.push(SourceFileManifest {
            format: parsed.format,
            path: file_path,
            filename,
            network: parsed.network,
            epoch: parsed.epoch,
            file_hash: parsed.file_hash,
            size_bytes: metadata.len(),
            modified_at,
            sha256,
            checksum_status,
            ranges: vec![SourceRangeManifest {
                from_block: file_from,
                to_block: file_to,
                completeness,
            }],
        });
    }

    let network = network_override
        .map(ToOwned::to_owned)
        .or_else(|| files.first().map(|file| file.network.clone()));

    Ok(SourceScan {
        kind: "era1".to_string(),
        path: path.to_owned(),
        network,
        files,
    })
}

fn parse_era1_filename(path: &Path, network_override: Option<&str>) -> Option<Era1FileName> {
    let extension = path.extension().and_then(|ext| ext.to_str())?;
    if extension != "era1" && extension != "ere" {
        return None;
    }

    let stem = path.file_stem()?.to_str()?;
    let parts = stem.split('-').collect::<Vec<_>>();
    if (extension == "era1" && parts.len() != 3) || (extension == "ere" && parts.len() < 3) {
        return None;
    }

    let network = network_override.unwrap_or(parts[0]).to_string();
    let epoch = parts[1].parse::<u64>().ok()?;
    let file_hash = parts[2].to_string();
    if file_hash.len() != 8 || !file_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if extension == "ere"
        && !parts[3..]
            .iter()
            .all(|profile| !profile.is_empty() && profile.chars().all(|c| c.is_ascii_lowercase()))
    {
        return None;
    }
    let from_block = epoch.checked_mul(ERA1_BLOCKS_PER_FILE)?;
    let to_block = from_block.checked_add(ERA1_BLOCKS_PER_FILE - 1)?;

    Some(Era1FileName {
        format: extension.to_string(),
        network,
        epoch,
        file_hash,
        from_block,
        to_block,
    })
}

#[derive(Debug, Default)]
struct ChecksumIndex {
    ordered: Vec<String>,
    by_filename: HashMap<String, String>,
    available: bool,
}

impl ChecksumIndex {
    fn status_for(&self, filename: &str, epoch: u64, actual: &str) -> ChecksumStatus {
        if !self.available {
            return ChecksumStatus::Unavailable;
        }

        let expected = self
            .by_filename
            .get(filename)
            .or_else(|| self.ordered.get(epoch as usize));

        match expected {
            Some(expected) if expected == actual => ChecksumStatus::Verified,
            Some(expected) => ChecksumStatus::Mismatched {
                expected: expected.clone(),
            },
            None => ChecksumStatus::Missing,
        }
    }
}

fn read_checksum_index(path: &Path) -> ChecksumIndex {
    let checksum_path = path.join("checksums.txt");
    let Ok(content) = fs::read_to_string(checksum_path) else {
        return ChecksumIndex::default();
    };

    let mut index = ChecksumIndex {
        ordered: Vec::new(),
        by_filename: HashMap::new(),
        available: true,
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(raw_hash) = parts.next() else {
            continue;
        };
        let Some(hash) = normalize_sha256(raw_hash) else {
            continue;
        };
        if let Some(name) = parts.next() {
            index.by_filename.insert(name.to_string(), hash.clone());
        }
        index.ordered.push(hash);
    }

    index
}

fn normalize_sha256(value: &str) -> Option<String> {
    let stripped = value.strip_prefix("0x").unwrap_or(value);
    if stripped.len() == 64 && stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(stripped.to_ascii_lowercase())
    } else {
        None
    }
}

fn sha256_file(path: &Path) -> Result<String, SourceError> {
    let mut file = fs::File::open(path).map_err(|source| SourceError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| SourceError::ReadFile {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(alloy_primitives::hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2store::iter_era1_block_tuples;
    use tempfile::tempdir;

    #[test]
    fn parses_standard_era1_filename() {
        let parsed = parse_era1_filename(Path::new("mainnet-00000-5ec1ffb8.era1"), None).unwrap();

        assert_eq!(parsed.network, "mainnet");
        assert_eq!(parsed.epoch, 0);
        assert_eq!(parsed.file_hash, "5ec1ffb8");
        assert_eq!(parsed.from_block, 0);
        assert_eq!(parsed.to_block, 8191);
    }

    #[test]
    fn parses_standard_ere_filename_with_profile() {
        let parsed =
            parse_era1_filename(Path::new("mainnet-00012-4bb7de2e-noproofs.ere"), None).unwrap();

        assert_eq!(parsed.network, "mainnet");
        assert_eq!(parsed.epoch, 12);
        assert_eq!(parsed.file_hash, "4bb7de2e");
        assert_eq!(parsed.from_block, 98_304);
        assert_eq!(parsed.to_block, 106_495);
    }

    #[test]
    fn scan_without_checksum_index_skips_sha256_hashing() {
        let dir = tempdir().unwrap();
        let era1 = dir.path().join("mainnet-00000-deadbeef.era1");
        fs::write(&era1, synthetic_era1_with_block_index(0, &[10])).unwrap();

        let scan = scan_era1_source(dir.path(), None, 0, u64::MAX).unwrap();

        assert_eq!(scan.files[0].checksum_status, ChecksumStatus::Unavailable);
        assert_eq!(scan.files[0].sha256, "");
    }

    #[test]
    fn ignores_unrelated_files() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();

        let scan = scan_era1_source(dir.path(), None, 0, u64::MAX).unwrap();

        assert!(scan.files.is_empty());
    }

    #[test]
    fn ordered_checksum_marks_first_file_verified() {
        let dir = tempdir().unwrap();
        let era1 = dir.path().join("mainnet-00000-5ec1ffb8.era1");
        fs::write(&era1, synthetic_era1_with_block_index(0, &[10])).unwrap();
        let hash = sha256_file(&era1).unwrap();
        fs::write(dir.path().join("checksums.txt"), format!("0x{hash}\n")).unwrap();

        let scan = scan_era1_source(dir.path(), None, 0, u64::MAX).unwrap();

        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].checksum_status, ChecksumStatus::Verified);
    }

    #[test]
    fn checksum_mismatch_is_recorded() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("mainnet-00000-5ec1ffb8.era1"),
            synthetic_era1_with_block_index(0, &[10]),
        )
        .unwrap();
        let wrong_hash = "00".repeat(32);
        fs::write(dir.path().join("checksums.txt"), format!("{wrong_hash}\n")).unwrap();

        let scan = scan_era1_source(dir.path(), None, 0, u64::MAX).unwrap();

        assert_eq!(
            scan.files[0].checksum_status,
            ChecksumStatus::Mismatched {
                expected: wrong_hash
            }
        );
    }

    #[test]
    fn block_index_overrides_filename_inferred_range() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("mainnet-00000-5ec1ffb8.era1"),
            synthetic_era1_with_block_index(64, &[10, 20, 30]),
        )
        .unwrap();

        let scan = scan_era1_source(dir.path(), None, 0, u64::MAX).unwrap();

        assert_eq!(scan.files[0].ranges[0].from_block, 64);
        assert_eq!(scan.files[0].ranges[0].to_block, 66);
        assert_eq!(scan.files[0].ranges[0].completeness.as_str(), "file-index");
    }

    #[test]
    fn empty_block_index_errors() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("mainnet-00000-5ec1ffb8.era1"),
            synthetic_era1_with_block_index(0, &[]),
        )
        .unwrap();

        let err = scan_era1_source(dir.path(), None, 0, u64::MAX).unwrap_err();

        assert!(
            err.to_string().contains("block index count is zero"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reads_raw_block_tuple_by_number() {
        let dir = tempdir().unwrap();
        let era1 = dir.path().join("mainnet-00000-5ec1ffb8.era1");
        fs::write(
            &era1,
            synthetic_era1_with_blocks(
                64,
                &[("h64", "b64", "r64", "d64"), ("h65", "b65", "r65", "d65")],
            ),
        )
        .unwrap();

        let block = read_era1_block_tuple(&era1, 65).unwrap().unwrap();

        assert_eq!(block.block_number, 65);
        assert_eq!(block.compressed_header, b"h65");
        assert_eq!(block.compressed_body, b"b65");
        assert_eq!(block.compressed_receipts, b"r65");
        assert_eq!(block.total_difficulty, b"d65");
    }

    #[test]
    fn iterates_all_block_tuples_in_order() {
        let dir = tempdir().unwrap();
        let era1 = dir.path().join("mainnet-00000-5ec1ffb8.era1");
        fs::write(
            &era1,
            synthetic_era1_with_blocks(
                10,
                &[
                    ("h10", "b10", "r10", "d10"),
                    ("h11", "b11", "r11", "d11"),
                    ("h12", "b12", "r12", "d12"),
                ],
            ),
        )
        .unwrap();

        let tuples: Vec<_> = iter_era1_block_tuples(&era1)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(tuples.len(), 3);
        assert_eq!(tuples[0].block_number, 10);
        assert_eq!(tuples[0].compressed_header, b"h10");
        assert_eq!(tuples[1].block_number, 11);
        assert_eq!(tuples[2].block_number, 12);
        assert_eq!(tuples[2].compressed_receipts, b"r12");
    }

    #[test]
    fn era1_selection_streams_block_facts_without_exposing_files() {
        use crate::era_pipeline::BlockFactStream;

        let dir = tempdir().unwrap();
        let era1 = dir.path().join("mainnet-00000-5ec1ffb8.era1");
        fs::write(&era1, synthetic_decodable_era1(64, &[64, 65])).unwrap();

        let source = Era1Source::scan(dir.path(), None, 0, u64::MAX).unwrap();
        let selection = source.block_facts_for_range(64, 65);

        let facts = selection.blocks().collect::<Result<Vec<_>, _>>().unwrap();

        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].block_number, 64);
        assert_eq!(facts[0].header.number, 64);
        assert_eq!(facts[1].block_number, 65);
        assert_eq!(facts[1].header.number, 65);
    }

    #[test]
    fn ere_selection_streams_block_facts_with_format_dispatch_hidden() {
        use crate::era_pipeline::BlockFactStream;

        let dir = tempdir().unwrap();
        let ere = dir.path().join("mainnet-00000-4bb7de2e-noproofs.ere");
        fs::write(&ere, synthetic_decodable_ere(64, &[64, 65])).unwrap();

        let source = Era1Source::scan(dir.path(), None, 0, u64::MAX).unwrap();
        let file = source
            .files()
            .first()
            .expect("scan should find one ERE file");
        assert_eq!(file.format, "ere");
        assert_eq!(file.ranges[0].from_block, 64);
        assert_eq!(file.ranges[0].to_block, 65);

        let selection = source.block_facts_for_range(64, 65);
        let facts = selection.blocks().collect::<Result<Vec<_>, _>>().unwrap();

        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].block_number, 64);
        assert_eq!(facts[0].header.number, 64);
        assert_eq!(facts[0].receipts.len(), 1);
        let receipt = facts[0].receipts[0].as_receipt_with_bloom().unwrap();
        assert_eq!(receipt.receipt.cumulative_gas_used, 21_000);
        assert_eq!(facts[1].block_number, 65);
        assert_eq!(facts[1].header.number, 65);
    }

    /// A decode failure on an `.ere` file must name the ERE format and carry
    /// the file path from the moment it is constructed — no downstream layer
    /// rewrites an ERA1-named error into an honest one.
    #[test]
    fn ere_decode_failure_names_ere_format_and_path() {
        use crate::era_pipeline::BlockFactStream;

        let dir = tempdir().unwrap();
        let ere = dir.path().join("mainnet-00000-4bb7de2e-noproofs.ere");
        fs::write(&ere, synthetic_ere_with_garbage_receipts(64)).unwrap();

        let source = Era1Source::scan(dir.path(), None, 0, u64::MAX).unwrap();
        let selection = source.block_facts_for_range(64, 64);

        let items: Vec<_> = selection.blocks().collect();
        let err = items[0]
            .as_ref()
            .expect_err("garbage slim receipts must surface as a stream error");

        let rendered = err.to_string();
        assert!(
            rendered.contains("mainnet-00000-4bb7de2e-noproofs.ere"),
            "decode failure must carry the file path, got: {rendered}"
        );
        assert!(
            rendered.contains("ERE"),
            "decode failure on an .ere file must name the ERE format, got: {rendered}"
        );
        assert!(
            !rendered.contains("ERA1"),
            "decode failure on an .ere file must not blame ERA1, got: {rendered}"
        );
    }

    /// Single-block ERE binary whose slim-receipts entry holds garbage bytes
    /// (valid e2store framing, invalid snappy payload).
    fn synthetic_ere_with_garbage_receipts(block_number: u64) -> Vec<u8> {
        let header = alloy_consensus::Header {
            number: block_number,
            ..Default::default()
        };

        let mut bytes = Vec::new();
        bytes.extend(e2store_entry([0x65, 0x32], &[]));
        bytes.extend(e2store_entry([0x03, 0x00], &snappy_rlp(&header)));
        bytes.extend(e2store_entry([0x04, 0x00], &snappy_empty_body()));
        bytes.extend(e2store_entry([0x0a, 0x00], &[0xDE, 0xAD, 0xBE, 0xEF]));

        let component_count = 3_u64;
        let mut index = Vec::new();
        index.extend_from_slice(&block_number.to_le_bytes());
        for slot in 0..component_count {
            index.extend_from_slice(&(-(100 + slot as i64)).to_le_bytes());
        }
        index.extend_from_slice(&component_count.to_le_bytes());
        index.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend(e2store_entry([0x67, 0x32], &index));
        bytes
    }

    #[test]
    fn selection_stream_surfaces_open_failure_with_context_and_continues() {
        use crate::era_pipeline::BlockFactStream;

        let dir = tempdir().unwrap();
        let first = dir.path().join("mainnet-00000-5ec1ffb8.era1");
        fs::write(&first, synthetic_decodable_era1(64, &[64, 65])).unwrap();
        fs::write(
            dir.path().join("mainnet-00001-deadbeef.era1"),
            synthetic_decodable_era1(8192, &[8192]),
        )
        .unwrap();

        let source = Era1Source::scan(dir.path(), None, 0, u64::MAX).unwrap();
        let selection = source.block_facts_for_range(64, 8192);
        assert_eq!(selection.total_blocks(), 3);

        // The first file disappears between scan and traversal.
        fs::remove_file(&first).unwrap();

        let items: Vec<_> = selection.blocks().collect();
        assert_eq!(items.len(), 2, "one open failure plus one streamed block");

        let err = items[0]
            .as_ref()
            .expect_err("missing file must surface as a stream error");
        assert!(
            err.to_string().contains("mainnet-00000-5ec1ffb8.era1"),
            "open failure must carry the file path, got: {err}"
        );

        let facts = items[1]
            .as_ref()
            .expect("traversal must continue with the next file");
        assert_eq!(facts.block_number, 8192);
    }

    #[test]
    fn era1_source_selects_block_fact_files_for_range() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("mainnet-00000-5ec1ffb8.era1"),
            synthetic_era1_with_block_index(64, &[10, 20, 30]),
        )
        .unwrap();
        fs::write(
            dir.path().join("mainnet-00001-deadbeef.era1"),
            synthetic_era1_with_block_index(8192, &[10, 20]),
        )
        .unwrap();

        let source = Era1Source::scan(dir.path(), None, 0, u64::MAX).unwrap();

        let selection = source.block_facts_for_range(65, 8200);

        assert_eq!(selection.file_count(), 2);
        assert_eq!(selection.total_blocks(), 5);
    }

    #[test]
    fn decodes_compressed_header_to_scope_header() {
        let header = alloy_consensus::Header {
            number: 64,
            parent_hash: alloy_primitives::B256::repeat_byte(0x11),
            receipts_root: alloy_primitives::B256::repeat_byte(0x22),
            timestamp: 12_345,
            gas_used: 21_000,
            base_fee_per_gas: Some(1_000),
            ..Default::default()
        };
        let compressed = snappy_rlp(&header);

        let decoded = decode_era1_header(&compressed).unwrap();

        assert_eq!(decoded.number, header.number);
        assert_eq!(decoded.hash, header.hash_slow());
        assert_eq!(decoded.parent_hash, header.parent_hash);
        assert_eq!(decoded.receipts_root, header.receipts_root);
        assert_eq!(decoded.timestamp, header.timestamp);
        assert_eq!(decoded.gas_used, header.gas_used);
        assert_eq!(decoded.base_fee_per_gas, Some(1_000));
    }

    fn synthetic_era1_with_block_index(starting_number: u64, offsets: &[i64]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(e2store_entry([0x65, 0x32], &[]));

        let mut index = Vec::new();
        index.extend_from_slice(&starting_number.to_le_bytes());
        for offset in offsets {
            index.extend_from_slice(&offset.to_le_bytes());
        }
        index.extend_from_slice(&(offsets.len() as i64).to_le_bytes());
        bytes.extend(e2store_entry([0x66, 0x32], &index));
        bytes
    }

    fn synthetic_era1_with_blocks(
        starting_number: u64,
        blocks: &[(&str, &str, &str, &str)],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(e2store_entry([0x65, 0x32], &[]));
        for (header, body, receipts, difficulty) in blocks {
            bytes.extend(e2store_entry([0x03, 0x00], header.as_bytes()));
            bytes.extend(e2store_entry([0x04, 0x00], body.as_bytes()));
            bytes.extend(e2store_entry([0x05, 0x00], receipts.as_bytes()));
            bytes.extend(e2store_entry([0x06, 0x00], difficulty.as_bytes()));
        }

        let offsets = (0..blocks.len())
            .map(|index| index as i64)
            .collect::<Vec<_>>();
        let mut index = Vec::new();
        index.extend_from_slice(&starting_number.to_le_bytes());
        for offset in offsets {
            index.extend_from_slice(&offset.to_le_bytes());
        }
        index.extend_from_slice(&(blocks.len() as i64).to_le_bytes());
        bytes.extend(e2store_entry([0x66, 0x32], &index));
        bytes
    }

    fn synthetic_decodable_era1(starting_number: u64, blocks: &[u64]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(e2store_entry([0x65, 0x32], &[]));
        for block_number in blocks {
            let header = alloy_consensus::Header {
                number: *block_number,
                ..Default::default()
            };
            bytes.extend(e2store_entry([0x03, 0x00], &snappy_rlp(&header)));
            bytes.extend(e2store_entry([0x04, 0x00], &snappy_empty_body()));
            bytes.extend(e2store_entry([0x05, 0x00], &snappy_empty_receipts()));
            bytes.extend(e2store_entry([0x06, 0x00], &0u64.to_le_bytes()));
        }

        let offsets = (0..blocks.len())
            .map(|index| index as i64)
            .collect::<Vec<_>>();
        let mut index = Vec::new();
        index.extend_from_slice(&starting_number.to_le_bytes());
        for offset in offsets {
            index.extend_from_slice(&offset.to_le_bytes());
        }
        index.extend_from_slice(&(blocks.len() as i64).to_le_bytes());
        bytes.extend(e2store_entry([0x66, 0x32], &index));
        bytes
    }

    fn synthetic_decodable_ere(starting_number: u64, blocks: &[u64]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(e2store_entry([0x65, 0x32], &[]));

        for block_number in blocks {
            let header = alloy_consensus::Header {
                number: *block_number,
                ..Default::default()
            };
            bytes.extend(e2store_entry([0x03, 0x00], &snappy_rlp(&header)));
        }
        for _ in blocks {
            bytes.extend(e2store_entry([0x04, 0x00], &snappy_empty_body()));
        }
        for _ in blocks {
            bytes.extend(e2store_entry([0x0a, 0x00], &snappy_one_slim_receipt()));
        }

        let component_count = 3_u64;
        let mut index = Vec::new();
        index.extend_from_slice(&starting_number.to_le_bytes());
        for i in 0..blocks.len() {
            for slot in 0..component_count {
                let offset = -(((i as i64) + 1) * 100 + slot as i64);
                index.extend_from_slice(&offset.to_le_bytes());
            }
        }
        index.extend_from_slice(&component_count.to_le_bytes());
        index.extend_from_slice(&(blocks.len() as u64).to_le_bytes());
        bytes.extend(e2store_entry([0x67, 0x32], &index));
        bytes
    }

    fn e2store_entry(entry_type: [u8; 2], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + data.len());
        bytes.extend_from_slice(&entry_type);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    fn snappy_empty_body() -> Vec<u8> {
        use alloy_rlp::Header as RlpHeader;

        let empty = RlpHeader {
            list: true,
            payload_length: 0,
        };
        let mut txs = Vec::new();
        empty.encode(&mut txs);
        let mut uncles = Vec::new();
        empty.encode(&mut uncles);
        let body_header = RlpHeader {
            list: true,
            payload_length: txs.len() + uncles.len(),
        };
        let mut body = Vec::new();
        body_header.encode(&mut body);
        body.extend_from_slice(&txs);
        body.extend_from_slice(&uncles);
        snappy_bytes(&body)
    }

    fn snappy_empty_receipts() -> Vec<u8> {
        snappy_bytes(&[0xC0])
    }

    fn snappy_one_slim_receipt() -> Vec<u8> {
        use alloy_consensus::Eip658Value;
        use alloy_rlp::{Encodable, Header as RlpHeader};

        let mut receipt_payload = Vec::new();
        0_u8.encode(&mut receipt_payload);
        Eip658Value::Eip658(true).encode(&mut receipt_payload);
        21_000_u64.encode(&mut receipt_payload);
        Vec::<alloy_primitives::Log>::new().encode(&mut receipt_payload);

        let mut receipt = Vec::new();
        RlpHeader {
            list: true,
            payload_length: receipt_payload.len(),
        }
        .encode(&mut receipt);
        receipt.extend_from_slice(&receipt_payload);

        let mut receipts_payload = Vec::new();
        receipts_payload.extend_from_slice(&receipt);
        let mut receipts = Vec::new();
        RlpHeader {
            list: true,
            payload_length: receipts_payload.len(),
        }
        .encode(&mut receipts);
        receipts.extend_from_slice(&receipts_payload);

        snappy_bytes(&receipts)
    }

    fn snappy_bytes(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;

        let mut compressed = Vec::new();
        {
            let mut encoder = snap::write::FrameEncoder::new(&mut compressed);
            encoder.write_all(bytes).unwrap();
            encoder.flush().unwrap();
        }
        compressed
    }

    fn snappy_rlp(header: &alloy_consensus::Header) -> Vec<u8> {
        use alloy_rlp::Encodable;

        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        snappy_bytes(&rlp)
    }
}
