use crate::source::{RangeCompleteness, SourceError, SourceRangeManifest};
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const E2STORE_VERSION: [u8; 2] = [0x65, 0x32];
const ERA1_COMPRESSED_HEADER: [u8; 2] = [0x03, 0x00];
const ERA1_COMPRESSED_BODY: [u8; 2] = [0x04, 0x00];
const ERA1_COMPRESSED_RECEIPTS: [u8; 2] = [0x05, 0x00];
const ERE_COMPRESSED_SLIM_RECEIPTS: [u8; 2] = [0x0a, 0x00];
const ERA1_TOTAL_DIFFICULTY: [u8; 2] = [0x06, 0x00];
const ERA1_BLOCK_INDEX: [u8; 2] = [0x66, 0x32];
const ERE_DYNAMIC_BLOCK_INDEX: [u8; 2] = [0x67, 0x32];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Era1BlockTuple {
    pub block_number: u64,
    pub compressed_header: Vec<u8>,
    pub compressed_body: Vec<u8>,
    pub compressed_receipts: Vec<u8>,
    pub total_difficulty: Vec<u8>,
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

/// Per-format e2store traversal knowledge. The traversal loop is shared; a
/// format differs only in which entry types carry receipts, which entry ends
/// the block section, and whether blocks interleave a total-difficulty entry.
struct TraversalFormat {
    name: &'static str,
    receipts_entry: [u8; 2],
    block_index_entry: [u8; 2],
    /// ERA1 interleaves one total-difficulty entry per block; ERE carries
    /// none, so its tuples get an empty `total_difficulty`.
    has_difficulty: bool,
}

const ERA1_TRAVERSAL: TraversalFormat = TraversalFormat {
    name: "ERA1",
    receipts_entry: ERA1_COMPRESSED_RECEIPTS,
    block_index_entry: ERA1_BLOCK_INDEX,
    has_difficulty: true,
};

const ERE_TRAVERSAL: TraversalFormat = TraversalFormat {
    name: "ERE",
    receipts_entry: ERE_COMPRESSED_SLIM_RECEIPTS,
    block_index_entry: ERE_DYNAMIC_BLOCK_INDEX,
    has_difficulty: false,
};

/// True when the archive at `path` uses the ERE/eraE slim-receipt layout, as
/// opposed to the ERA1 full-receipt layout. Both `.ere` and `.erae` share the
/// slim receipts (`0x0a`) and dynamic block index (`0x67 0x32`); only the file
/// extension distinguishes them from legacy `.era1` archives.
pub(crate) fn is_slim_receipt_format(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("ere") | Some("erae")
    )
}

/// A sequential iterator over every [`Era1BlockTuple`] in an archive file.
///
/// Reads the file in a single O(N) pass with pending-slot queues: a block
/// tuple is yielded as soon as all of its components have been read, never
/// after buffering the whole file. Construct via [`iter_era1_block_tuples`].
pub(crate) struct BlockTupleIter {
    file: fs::File,
    path: PathBuf,
    format: &'static TraversalFormat,
    from_block: u64,
    expected_blocks: u64,
    yielded: u64,
    headers: VecDeque<Vec<u8>>,
    bodies: VecDeque<Vec<u8>>,
    receipts: VecDeque<Vec<u8>>,
    difficulties: VecDeque<Vec<u8>>,
    done: bool,
}

impl BlockTupleIter {
    /// Pop one complete block tuple if every component is pending.
    fn try_pop_block(&mut self) -> Option<Era1BlockTuple> {
        if self.headers.is_empty()
            || self.bodies.is_empty()
            || self.receipts.is_empty()
            || (self.format.has_difficulty && self.difficulties.is_empty())
        {
            return None;
        }

        let compressed_header = self.headers.pop_front()?;
        let compressed_body = self.bodies.pop_front()?;
        let compressed_receipts = self.receipts.pop_front()?;
        let total_difficulty = if self.format.has_difficulty {
            self.difficulties.pop_front()?
        } else {
            Vec::new()
        };

        let tuple = Era1BlockTuple {
            block_number: self.from_block + self.yielded,
            compressed_header,
            compressed_body,
            compressed_receipts,
            total_difficulty,
        };
        self.yielded += 1;
        Some(tuple)
    }

    /// End of the block section: report leftover components or a shortfall
    /// against the block index, then stop.
    fn finish(&mut self) -> Option<Result<Era1BlockTuple, SourceError>> {
        self.done = true;

        if !self.headers.is_empty()
            || !self.bodies.is_empty()
            || !self.receipts.is_empty()
            || !self.difficulties.is_empty()
        {
            return Some(Err(SourceError::InvalidE2Store {
                path: self.path.clone(),
                message: format!(
                    "{} file ended mid-block: {} header(s), {} body(ies), {} receipt(s) left unmatched",
                    self.format.name,
                    self.headers.len(),
                    self.bodies.len(),
                    self.receipts.len(),
                ),
            }));
        }

        if self.yielded != self.expected_blocks {
            return Some(Err(SourceError::InvalidE2Store {
                path: self.path.clone(),
                message: format!(
                    "{} block index records {} blocks but the file contains {}",
                    self.format.name, self.expected_blocks, self.yielded,
                ),
            }));
        }

        None
    }
}

impl Iterator for BlockTupleIter {
    type Item = Result<Era1BlockTuple, SourceError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            if let Some(tuple) = self.try_pop_block() {
                return Some(Ok(tuple));
            }

            let entry = match read_e2store_entry(&mut self.file, &self.path) {
                Ok(Some(e)) => e,
                Ok(None) => return self.finish(),
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };

            match entry.entry_type {
                t if t == self.format.block_index_entry => return self.finish(),
                t if t == self.format.receipts_entry => self.receipts.push_back(entry.data),
                ERA1_COMPRESSED_HEADER => self.headers.push_back(entry.data),
                ERA1_COMPRESSED_BODY => self.bodies.push_back(entry.data),
                ERA1_TOTAL_DIFFICULTY if self.format.has_difficulty => {
                    self.difficulties.push_back(entry.data)
                }
                _ => {}
            }
        }
    }
}

/// Open an era file and return an iterator that yields every block tuple in order.
///
/// The block index at the end of the file is read first to determine the
/// block range, then the file is rewound to the beginning for the sequential
/// one-pass read.
pub(crate) fn iter_era1_block_tuples(
    path: impl AsRef<Path>,
) -> Result<BlockTupleIter, SourceError> {
    let path = path.as_ref();
    let format = if is_slim_receipt_format(path) {
        &ERE_TRAVERSAL
    } else {
        &ERA1_TRAVERSAL
    };

    let range = read_era1_block_index_range(path)?.ok_or_else(|| SourceError::InvalidE2Store {
        path: path.to_owned(),
        message: format!("no block index found in {} file", format.name),
    })?;

    let file = fs::File::open(path).map_err(|source| SourceError::ReadFile {
        path: path.to_owned(),
        source,
    })?;

    Ok(BlockTupleIter {
        file,
        path: path.to_owned(),
        format,
        from_block: range.from_block,
        expected_blocks: range.to_block.saturating_sub(range.from_block) + 1,
        yielded: 0,
        headers: VecDeque::new(),
        bodies: VecDeque::new(),
        receipts: VecDeque::new(),
        difficulties: VecDeque::new(),
        done: false,
    })
}

pub(crate) fn read_era1_block_index_range(
    path: &Path,
) -> Result<Option<SourceRangeManifest>, SourceError> {
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

        if entry.entry_type == ERA1_BLOCK_INDEX || entry.entry_type == ERE_DYNAMIC_BLOCK_INDEX {
            let mut data = vec![0_u8; entry.length as usize];
            file.read_exact(&mut data)
                .map_err(|source| SourceError::ReadFile {
                    path: path.to_owned(),
                    source,
                })?;
            let range = if entry.entry_type == ERA1_BLOCK_INDEX {
                parse_block_index_entry(path, &data)?
            } else {
                parse_dynamic_block_index_entry(path, &data)?
            };
            return Ok(Some(range));
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
            });
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

fn parse_dynamic_block_index_entry(
    path: &Path,
    data: &[u8],
) -> Result<SourceRangeManifest, SourceError> {
    if data.len() < 24 {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: "dynamic block index entry is shorter than 24 bytes".to_string(),
        });
    }
    let count = u64::from_le_bytes(data[data.len() - 8..].try_into().unwrap()) as usize;
    let component_count =
        u64::from_le_bytes(data[data.len() - 16..data.len() - 8].try_into().unwrap()) as usize;
    if !(2..=5).contains(&component_count) {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: format!(
                "dynamic block index component count must be 2-5, got {component_count}"
            ),
        });
    }
    if count == 0 {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: "dynamic block index count is zero".to_string(),
        });
    }
    let expected_len = 8 + count * component_count * 8 + 16;
    if data.len() != expected_len {
        return Err(SourceError::InvalidE2Store {
            path: path.to_owned(),
            message: format!(
                "dynamic block index length mismatch: expected {expected_len}, got {}",
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

    #[test]
    fn slim_format_detected_for_ere_and_erae() {
        assert!(is_slim_receipt_format(Path::new("mainnet-00012-4bb7de2e.ere")));
        assert!(is_slim_receipt_format(Path::new("mainnet-00000-5ec1ffb8.erae")));
    }

    #[test]
    fn slim_format_not_detected_for_era1() {
        assert!(!is_slim_receipt_format(Path::new(
            "mainnet-00000-5ec1ffb8.era1"
        )));
    }
}
