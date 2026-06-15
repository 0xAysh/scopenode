//! Checksum index parsing and per-file checksum status.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use super::SourceError;

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

#[derive(Debug, Default)]
pub(super) struct ChecksumIndex {
    pub(super) ordered: Vec<String>,
    pub(super) by_filename: HashMap<String, String>,
    pub(super) available: bool,
}

impl ChecksumIndex {
    pub(super) fn status_for(&self, filename: &str, epoch: u64, actual: &str) -> ChecksumStatus {
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

pub(super) fn read_checksum_index(path: &Path) -> ChecksumIndex {
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

pub(super) fn sha256_file(path: &Path) -> Result<String, SourceError> {
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

    #[test]
    fn checksum_status_unavailable_when_index_missing() {
        let index = ChecksumIndex::default();

        assert_eq!(
            index.status_for("mainnet-00000-5ec1ffb8.era1", 0, "abc"),
            ChecksumStatus::Unavailable
        );
    }

    #[test]
    fn checksum_status_missing_when_not_in_index() {
        let index = ChecksumIndex {
            ordered: Vec::new(),
            by_filename: HashMap::new(),
            available: true,
        };

        assert_eq!(
            index.status_for("mainnet-00000-5ec1ffb8.era1", 0, "abc"),
            ChecksumStatus::Missing
        );
    }

    #[test]
    fn checksum_status_falls_back_to_ordered_index_by_epoch() {
        let index = ChecksumIndex {
            ordered: vec!["aaaa".to_string(), "bbbb".to_string()],
            by_filename: HashMap::new(),
            available: true,
        };

        assert_eq!(
            index.status_for("mainnet-00001-5ec1ffb8.era1", 1, "bbbb"),
            ChecksumStatus::Verified
        );
    }

    #[test]
    fn normalize_sha256_accepts_0x_prefix_and_lowercases() {
        let hash = "A".repeat(64);

        assert_eq!(normalize_sha256(&format!("0x{hash}")), Some("a".repeat(64)));
    }

    #[test]
    fn normalize_sha256_rejects_wrong_length() {
        assert_eq!(normalize_sha256("abcd"), None);
    }
}
