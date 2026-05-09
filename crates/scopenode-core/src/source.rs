//! Local historical source scanning.

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
}

impl RangeCompleteness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inferred => "inferred",
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
                from_block: parsed.from_block,
                to_block: parsed.to_block,
                completeness: RangeCompleteness::Inferred,
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
        fs::write(&era1, b"era1 bytes").unwrap();
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
            b"era1 bytes",
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
}
