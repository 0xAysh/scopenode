//! Local historical source scanning.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use thiserror::Error;

pub const ERA1_BLOCKS_PER_FILE: u64 = 8192;
const E2STORE_VERSION: [u8; 2] = [0x65, 0x32];
const ERA1_BLOCK_INDEX: [u8; 2] = [0x66, 0x32];

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
    network: String,
    epoch: u64,
    file_hash: String,
    from_block: u64,
    to_block: u64,
}

pub fn scan_era1_source(
    path: impl AsRef<Path>,
    network_override: Option<&str>,
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

        let sha256 = sha256_file(&file_path)?;
        let content_range = read_era1_block_index_range(&file_path)?;
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

        let (from_block, to_block, completeness) = content_range
            .as_ref()
            .map(|range| (range.from_block, range.to_block, range.completeness))
            .unwrap_or((
                parsed.from_block,
                parsed.to_block,
                RangeCompleteness::Inferred,
            ));

        files.push(SourceFileManifest {
            format: "era1".to_string(),
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
                from_block,
                to_block,
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
    if path.extension().and_then(|ext| ext.to_str()) != Some("era1") {
        return None;
    }

    let stem = path.file_stem()?.to_str()?;
    let parts = stem.split('-').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }

    let network = network_override.unwrap_or(parts[0]).to_string();
    let epoch = parts[1].parse::<u64>().ok()?;
    let file_hash = parts[2].to_string();
    if file_hash.len() != 8 || !file_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let from_block = epoch.checked_mul(ERA1_BLOCKS_PER_FILE)?;
    let to_block = from_block.checked_add(ERA1_BLOCKS_PER_FILE - 1)?;

    Some(Era1FileName {
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
    Ok(hex::encode(hasher.finalize()))
}

fn read_era1_block_index_range(path: &Path) -> Result<Option<SourceRangeManifest>, SourceError> {
    let mut file = fs::File::open(path).map_err(|source| SourceError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    let mut offset = 0_u64;
    let mut first = true;

    while let Some(entry) = read_e2store_entry_header(&mut file, path)? {
        let data_offset = offset
            .checked_add(8)
            .ok_or_else(|| SourceError::RangeOverflow {
                path: path.to_owned(),
            })?;
        offset = data_offset
            .checked_add(entry.length as u64)
            .ok_or_else(|| SourceError::RangeOverflow {
                path: path.to_owned(),
            })?;

        if first {
            first = false;
            if entry.entry_type != E2STORE_VERSION {
                return Err(SourceError::InvalidE2Store {
                    path: path.to_owned(),
                    message: "first entry is not the e2store version record".to_string(),
                });
            }
        }

        if entry.entry_type == ERA1_BLOCK_INDEX {
            let mut data = vec![0_u8; entry.length as usize];
            file.read_exact(&mut data)
                .map_err(|source| SourceError::ReadFile {
                    path: path.to_owned(),
                    source,
                })?;
            return parse_block_index_entry(path, &data).map(Some);
        }

        file.seek(SeekFrom::Current(entry.length as i64))
            .map_err(|source| SourceError::ReadFile {
                path: path.to_owned(),
                source,
            })?;
    }

    Ok(None)
}

#[derive(Debug)]
struct E2StoreEntryHeader {
    entry_type: [u8; 2],
    length: u32,
}

fn read_e2store_entry_header(
    reader: &mut fs::File,
    path: &Path,
) -> Result<Option<E2StoreEntryHeader>, SourceError> {
    let mut header = [0_u8; 8];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(source) => {
            return Err(SourceError::ReadFile {
                path: path.to_owned(),
                source,
            })
        }
    }

    let reserved = u16::from_le_bytes([header[6], header[7]]);
    if reserved != 0 {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: "entry header reserved field is not zero".to_string(),
        });
    }

    Ok(Some(E2StoreEntryHeader {
        entry_type: [header[0], header[1]],
        length: u32::from_le_bytes([header[2], header[3], header[4], header[5]]),
    }))
}

fn parse_block_index_entry(path: &Path, data: &[u8]) -> Result<SourceRangeManifest, SourceError> {
    if data.len() < 16 {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: "block index entry is shorter than 16 bytes".to_string(),
        });
    }

    let count = i64::from_le_bytes(data[data.len() - 8..].try_into().unwrap());
    if count < 0 {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: "block index count is negative".to_string(),
        });
    }
    let count = count as usize;
    if count == 0 {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: "block index count is zero".to_string(),
        });
    }
    let expected_len = 8 + count * 8 + 8;
    if data.len() != expected_len {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: format!(
                "block index length mismatch: expected {expected_len}, got {}",
                data.len()
            ),
        });
    }

    let starting_number = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let to_block = starting_number
        .checked_add(count.saturating_sub(1) as u64)
        .ok_or_else(|| SourceError::RangeOverflow {
            path: path.to_owned(),
        })?;

    Ok(SourceRangeManifest {
        from_block: starting_number,
        to_block,
        completeness: RangeCompleteness::FileIndex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn ignores_unrelated_files() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();

        let scan = scan_era1_source(dir.path(), None).unwrap();

        assert!(scan.files.is_empty());
    }

    #[test]
    fn ordered_checksum_marks_first_file_verified() {
        let dir = tempdir().unwrap();
        let era1 = dir.path().join("mainnet-00000-5ec1ffb8.era1");
        fs::write(&era1, synthetic_era1_with_block_index(0, &[10])).unwrap();
        let hash = sha256_file(&era1).unwrap();
        fs::write(dir.path().join("checksums.txt"), format!("0x{hash}\n")).unwrap();

        let scan = scan_era1_source(dir.path(), None).unwrap();

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

        let scan = scan_era1_source(dir.path(), None).unwrap();

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

        let scan = scan_era1_source(dir.path(), None).unwrap();

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

        let err = scan_era1_source(dir.path(), None).unwrap_err();

        assert!(
            err.to_string().contains("block index count is zero"),
            "unexpected error: {err}"
        );
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

    fn e2store_entry(entry_type: [u8; 2], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + data.len());
        bytes.extend_from_slice(&entry_type);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }
}
