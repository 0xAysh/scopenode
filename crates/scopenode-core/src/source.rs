//! Local historical source scanning.

use crate::types::ScopeHeader;
use alloy_consensus::{Header, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{keccak256, Log as PrimitiveLog};
use alloy_rlp::Decodable;
use sha2::{Digest, Sha256};
use snap::read::FrameDecoder;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use thiserror::Error;

pub const ERA1_BLOCKS_PER_FILE: u64 = 8192;
const E2STORE_VERSION: [u8; 2] = [0x65, 0x32];
const ERA1_COMPRESSED_HEADER: [u8; 2] = [0x03, 0x00];
const ERA1_COMPRESSED_BODY: [u8; 2] = [0x04, 0x00];
const ERA1_COMPRESSED_RECEIPTS: [u8; 2] = [0x05, 0x00];
const ERA1_TOTAL_DIFFICULTY: [u8; 2] = [0x06, 0x00];
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

    #[error("invalid ERA1 header: {0}")]
    InvalidEra1Header(String),

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Era1BlockTuple {
    pub block_number: u64,
    pub compressed_header: Vec<u8>,
    pub compressed_body: Vec<u8>,
    pub compressed_receipts: Vec<u8>,
    pub total_difficulty: Vec<u8>,
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

        // Only SHA256 files that are in range.
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

pub fn read_era1_block_tuple(
    path: impl AsRef<Path>,
    block_number: u64,
) -> Result<Option<Era1BlockTuple>, SourceError> {
    let path = path.as_ref();
    let Some(range) = read_era1_block_index_range(path)? else {
        return Ok(None);
    };
    if block_number < range.from_block || block_number > range.to_block {
        return Ok(None);
    }

    let target_index = (block_number - range.from_block) as usize;
    let mut file = fs::File::open(path).map_err(|source| SourceError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    let mut headers = VecDeque::new();
    let mut bodies = VecDeque::new();
    let mut receipts = VecDeque::new();
    let mut difficulties = VecDeque::new();
    let mut block_index = 0_usize;

    while let Some(entry) = read_e2store_entry(&mut file, path)? {
        match entry.entry_type {
            E2STORE_VERSION => {}
            ERA1_COMPRESSED_HEADER => headers.push_back(entry.data),
            ERA1_COMPRESSED_BODY => bodies.push_back(entry.data),
            ERA1_COMPRESSED_RECEIPTS => receipts.push_back(entry.data),
            ERA1_TOTAL_DIFFICULTY => difficulties.push_back(entry.data),
            ERA1_BLOCK_INDEX => break,
            _ => {}
        }

        while !headers.is_empty()
            && !bodies.is_empty()
            && !receipts.is_empty()
            && !difficulties.is_empty()
        {
            let tuple = Era1BlockTuple {
                block_number: range.from_block + block_index as u64,
                compressed_header: headers.pop_front().unwrap(),
                compressed_body: bodies.pop_front().unwrap(),
                compressed_receipts: receipts.pop_front().unwrap(),
                total_difficulty: difficulties.pop_front().unwrap(),
            };
            if block_index == target_index {
                return Ok(Some(tuple));
            }
            block_index += 1;
        }
    }

    Ok(None)
}

/// A sequential iterator over every [`Era1BlockTuple`] in an ERA1 file.
///
/// Reads the file in a single O(N) pass. Construct via [`iter_era1_block_tuples`].
pub struct Era1BlockIter {
    file: fs::File,
    path: PathBuf,
    from_block: u64,
    pending_header: Option<Vec<u8>>,
    pending_body: Option<Vec<u8>>,
    pending_receipts: Option<Vec<u8>>,
    pending_difficulty: Option<Vec<u8>>,
    block_index: usize,
    done: bool,
}

impl Iterator for Era1BlockIter {
    type Item = Result<Era1BlockTuple, SourceError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            // If all four slots are filled, yield a tuple.
            if self.pending_header.is_some()
                && self.pending_body.is_some()
                && self.pending_receipts.is_some()
                && self.pending_difficulty.is_some()
            {
                let tuple = Era1BlockTuple {
                    block_number: self.from_block + self.block_index as u64,
                    compressed_header: self.pending_header.take().unwrap(),
                    compressed_body: self.pending_body.take().unwrap(),
                    compressed_receipts: self.pending_receipts.take().unwrap(),
                    total_difficulty: self.pending_difficulty.take().unwrap(),
                };
                self.block_index += 1;
                return Some(Ok(tuple));
            }

            // Need to read more entries to complete the next tuple.
            let entry = match read_e2store_entry(&mut self.file, &self.path) {
                Ok(Some(e)) => e,
                Ok(None) => {
                    self.done = true;
                    return None;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };

            match entry.entry_type {
                E2STORE_VERSION => {}
                ERA1_COMPRESSED_HEADER => self.pending_header = Some(entry.data),
                ERA1_COMPRESSED_BODY => self.pending_body = Some(entry.data),
                ERA1_COMPRESSED_RECEIPTS => self.pending_receipts = Some(entry.data),
                ERA1_TOTAL_DIFFICULTY => self.pending_difficulty = Some(entry.data),
                ERA1_BLOCK_INDEX => {
                    // Block index signals end of block data; stop iterating.
                    self.done = true;
                    return None;
                }
                _ => {}
            }
        }
    }
}

/// Open an ERA1 file and return an iterator that yields every block tuple in order.
///
/// The block index at the end of the file is read first to determine `from_block`,
/// then the file is rewound to the beginning for the sequential one-pass read.
pub fn iter_era1_block_tuples(path: impl AsRef<Path>) -> Result<Era1BlockIter, SourceError> {
    let path = path.as_ref();
    let range = read_era1_block_index_range(path)?.ok_or_else(|| SourceError::InvalidE2Store {
        path: path.to_owned(),
        message: "no block index found in ERA1 file".into(),
    })?;
    let from_block = range.from_block;

    let file = fs::File::open(path).map_err(|source| SourceError::ReadFile {
        path: path.to_owned(),
        source,
    })?;

    Ok(Era1BlockIter {
        file,
        path: path.to_owned(),
        from_block,
        pending_header: None,
        pending_body: None,
        pending_receipts: None,
        pending_difficulty: None,
        block_index: 0,
        done: false,
    })
}

pub fn decode_era1_header(compressed_header: &[u8]) -> Result<ScopeHeader, SourceError> {
    let mut decoder = FrameDecoder::new(compressed_header);
    let mut rlp = Vec::new();
    decoder
        .read_to_end(&mut rlp)
        .map_err(|source| SourceError::InvalidEra1Header(source.to_string()))?;

    let mut slice = rlp.as_slice();
    let header = Header::decode(&mut slice)
        .map_err(|source| SourceError::InvalidEra1Header(source.to_string()))?;

    Ok(ScopeHeader {
        number: header.number,
        hash: header.hash_slow(),
        parent_hash: header.parent_hash,
        timestamp: header.timestamp,
        receipts_root: header.receipts_root,
        logs_bloom: header.logs_bloom,
        gas_used: header.gas_used,
        base_fee_per_gas: header.base_fee_per_gas.map(|fee| fee as u128),
    })
}

pub fn decode_era1_receipts(
    compressed: &[u8],
) -> Result<Vec<ReceiptEnvelope<PrimitiveLog>>, SourceError> {
    let mut raw = Vec::new();
    FrameDecoder::new(compressed)
        .read_to_end(&mut raw)
        .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;

    let mut slice = raw.as_slice();

    let list_header = alloy_rlp::Header::decode(&mut slice)
        .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
    if !list_header.list {
        return Err(SourceError::InvalidEra1Header(
            "receipts block is not an RLP list".into(),
        ));
    }

    let mut payload = &slice[..list_header.payload_length];
    let mut receipts = Vec::new();

    while !payload.is_empty() {
        let first = *payload.first().unwrap();

        if first >= 0xc0 {
            // RLP list → legacy receipt
            let with_bloom = ReceiptWithBloom::<Receipt<PrimitiveLog>>::decode(&mut payload)
                .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
            receipts.push(ReceiptEnvelope::Legacy(with_bloom));
        } else if first >= 0x80 {
            // RLP byte string → typed receipt bytes
            let item_header = alloy_rlp::Header::decode(&mut payload)
                .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
            let item = &payload[..item_header.payload_length];
            payload = &payload[item_header.payload_length..];

            let receipt_type = *item.first().ok_or_else(|| {
                SourceError::InvalidEra1Header("typed receipt byte string is empty".into())
            })?;
            let mut body = &item[1..];
            let with_bloom = ReceiptWithBloom::<Receipt<PrimitiveLog>>::decode(&mut body)
                .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
            receipts.push(match receipt_type {
                1 => ReceiptEnvelope::Eip2930(with_bloom),
                2 => ReceiptEnvelope::Eip1559(with_bloom),
                3 => ReceiptEnvelope::Eip4844(with_bloom),
                4 => ReceiptEnvelope::Eip7702(with_bloom),
                t => {
                    return Err(SourceError::InvalidEra1Header(format!(
                        "unknown receipt type: {t}"
                    )))
                }
            });
        } else {
            // Direct type prefix byte
            let receipt_type = first;
            payload = &payload[1..];
            let with_bloom = ReceiptWithBloom::<Receipt<PrimitiveLog>>::decode(&mut payload)
                .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
            receipts.push(match receipt_type {
                1 => ReceiptEnvelope::Eip2930(with_bloom),
                2 => ReceiptEnvelope::Eip1559(with_bloom),
                3 => ReceiptEnvelope::Eip4844(with_bloom),
                4 => ReceiptEnvelope::Eip7702(with_bloom),
                t => {
                    return Err(SourceError::InvalidEra1Header(format!(
                        "unknown receipt type: {t}"
                    )))
                }
            });
        }
    }

    Ok(receipts)
}

pub fn decode_era1_tx_hashes(compressed: &[u8]) -> Result<Vec<alloy_primitives::B256>, SourceError> {
    let mut raw = Vec::new();
    FrameDecoder::new(compressed)
        .read_to_end(&mut raw)
        .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;

    let mut slice = raw.as_slice();

    // Outer body list: [transactions, uncles]
    let body_header = alloy_rlp::Header::decode(&mut slice)
        .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
    if !body_header.list {
        return Err(SourceError::InvalidEra1Header("body is not an RLP list".into()));
    }

    // Transactions list header
    let tx_list_header = alloy_rlp::Header::decode(&mut slice)
        .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
    if !tx_list_header.list {
        return Err(SourceError::InvalidEra1Header(
            "body transactions is not an RLP list".into(),
        ));
    }

    let mut tx_payload = &slice[..tx_list_header.payload_length];
    let mut hashes = Vec::new();

    while !tx_payload.is_empty() {
        let first = *tx_payload.first().unwrap();

        let raw_tx: Vec<u8> = if first >= 0xc0 {
            // Legacy tx: the RLP list item (header + content)
            let start = tx_payload;
            let h = match alloy_rlp::Header::decode(&mut tx_payload) {
                Ok(h) => h,
                Err(_) => {
                    hashes.push(alloy_primitives::B256::ZERO);
                    break;
                }
            };
            let consumed = start.len() - tx_payload.len();
            let item = start[..consumed + h.payload_length].to_vec();
            tx_payload = &tx_payload[h.payload_length..];
            item
        } else if first >= 0x80 {
            // Typed tx in RLP byte string: content = type_byte || RLP(body)
            let h = match alloy_rlp::Header::decode(&mut tx_payload) {
                Ok(h) => h,
                Err(_) => {
                    hashes.push(alloy_primitives::B256::ZERO);
                    break;
                }
            };
            let bytes = tx_payload[..h.payload_length].to_vec();
            tx_payload = &tx_payload[h.payload_length..];
            bytes
        } else {
            // Direct type prefix
            let type_byte = first;
            tx_payload = &tx_payload[1..];
            let start = tx_payload;
            let h = match alloy_rlp::Header::decode(&mut tx_payload) {
                Ok(h) => h,
                Err(_) => {
                    hashes.push(alloy_primitives::B256::ZERO);
                    break;
                }
            };
            let consumed = start.len() - tx_payload.len();
            let body_len = consumed + h.payload_length;
            let mut bytes = vec![type_byte];
            bytes.extend_from_slice(&start[..body_len]);
            tx_payload = &tx_payload[h.payload_length..];
            bytes
        };

        hashes.push(keccak256(&raw_tx));
    }

    Ok(hashes)
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
    Ok(alloy_primitives::hex::encode(hasher.finalize()))
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

#[derive(Debug)]
struct E2StoreEntry {
    entry_type: [u8; 2],
    data: Vec<u8>,
}

fn read_e2store_entry(
    reader: &mut fs::File,
    path: &Path,
) -> Result<Option<E2StoreEntry>, SourceError> {
    let Some(header) = read_e2store_entry_header(reader, path)? else {
        return Ok(None);
    };
    let mut data = vec![0_u8; header.length as usize];
    reader
        .read_exact(&mut data)
        .map_err(|source| SourceError::ReadFile {
            path: path.to_owned(),
            source,
        })?;

    Ok(Some(E2StoreEntry {
        entry_type: header.entry_type,
        data,
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

    fn e2store_entry(entry_type: [u8; 2], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + data.len());
        bytes.extend_from_slice(&entry_type);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[test]
    fn decodes_empty_receipt_list() {
        use std::io::Write;

        // RLP of an empty list: 0xC0
        let rlp_empty_list = vec![0xC0u8];
        let mut compressed = Vec::new();
        {
            let mut enc = snap::write::FrameEncoder::new(&mut compressed);
            enc.write_all(&rlp_empty_list).unwrap();
            enc.flush().unwrap();
        }

        let receipts = decode_era1_receipts(&compressed).unwrap();
        assert!(receipts.is_empty());
    }

    #[test]
    fn decodes_single_legacy_receipt() {
        use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom};
        use alloy_primitives::{Bloom, Log as PrimitiveLog};
        use alloy_rlp::{Encodable, Header as RlpHeader};
        use std::io::Write;

        let receipt = ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::default(),
        };

        let mut item_buf = Vec::new();
        receipt.encode(&mut item_buf);

        let outer_header = RlpHeader {
            list: true,
            payload_length: item_buf.len(),
        };
        let mut list_buf = Vec::new();
        outer_header.encode(&mut list_buf);
        list_buf.extend_from_slice(&item_buf);

        let mut compressed = Vec::new();
        {
            let mut enc = snap::write::FrameEncoder::new(&mut compressed);
            enc.write_all(&list_buf).unwrap();
            enc.flush().unwrap();
        }

        let receipts = decode_era1_receipts(&compressed).unwrap();
        assert_eq!(receipts.len(), 1);
        let inner = receipts[0].as_receipt_with_bloom().unwrap();
        assert_eq!(inner.receipt.cumulative_gas_used, 21_000);
    }

    #[test]
    fn decodes_empty_body_as_no_tx_hashes() {
        use alloy_rlp::Header as RlpHeader;
        use std::io::Write;

        // RLP of [[],[]] — empty transactions list and empty uncles list
        let empty = RlpHeader { list: true, payload_length: 0 };
        let mut txs = Vec::new();
        empty.encode(&mut txs);
        let mut uncles = Vec::new();
        empty.encode(&mut uncles);
        let body_payload = txs.len() + uncles.len();
        let body_h = RlpHeader { list: true, payload_length: body_payload };
        let mut body_buf = Vec::new();
        body_h.encode(&mut body_buf);
        body_buf.extend_from_slice(&txs);
        body_buf.extend_from_slice(&uncles);

        let mut compressed = Vec::new();
        {
            let mut enc = snap::write::FrameEncoder::new(&mut compressed);
            enc.write_all(&body_buf).unwrap();
            enc.flush().unwrap();
        }

        let hashes = decode_era1_tx_hashes(&compressed).unwrap();
        assert!(hashes.is_empty());
    }

    fn snappy_rlp(header: &alloy_consensus::Header) -> Vec<u8> {
        use alloy_rlp::Encodable;
        use std::io::Write;

        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        let mut compressed = Vec::new();
        {
            let mut encoder = snap::write::FrameEncoder::new(&mut compressed);
            encoder.write_all(&rlp).unwrap();
            encoder.flush().unwrap();
        }
        compressed
    }
}
