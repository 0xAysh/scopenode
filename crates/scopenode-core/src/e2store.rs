use crate::source::{RangeCompleteness, SourceError, SourceRangeManifest};
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const E2STORE_VERSION: [u8; 2] = [0x65, 0x32];
const ERA1_COMPRESSED_HEADER: [u8; 2] = [0x03, 0x00];
const ERA1_COMPRESSED_BODY: [u8; 2] = [0x04, 0x00];
const ERA1_COMPRESSED_RECEIPTS: [u8; 2] = [0x05, 0x00];
const ERA1_TOTAL_DIFFICULTY: [u8; 2] = [0x06, 0x00];
const ERA1_BLOCK_INDEX: [u8; 2] = [0x66, 0x32];

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
pub(crate) fn iter_era1_block_tuples(path: impl AsRef<Path>) -> Result<Era1BlockIter, SourceError> {
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
