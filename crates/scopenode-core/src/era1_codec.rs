//! ERA1 block codec functions.
//!
//! This module owns Snappy/RLP decoding for ERA1 headers, receipts, and body
//! transaction hashes. Source discovery and e2store iteration stay elsewhere.

use crate::source::SourceError;
use crate::types::ScopeHeader;
use alloy_consensus::{Eip658Value, Header, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{keccak256, logs_bloom, Log as PrimitiveLog};
use alloy_rlp::Decodable;
use snap::read::FrameDecoder;
use std::io::Read;

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

pub(crate) fn decode_era1_receipts(
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
            let with_bloom = ReceiptWithBloom::<Receipt<PrimitiveLog>>::decode(&mut payload)
                .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
            receipts.push(ReceiptEnvelope::Legacy(with_bloom));
        } else if first >= 0x80 {
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

pub(crate) fn decode_ere_slim_receipts(
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
            "slim receipts block is not an RLP list".into(),
        ));
    }

    let mut payload = &slice[..list_header.payload_length];
    let mut receipts = Vec::new();

    while !payload.is_empty() {
        let receipt_header = alloy_rlp::Header::decode(&mut payload)
            .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
        if !receipt_header.list {
            return Err(SourceError::InvalidEra1Header(
                "slim receipt is not an RLP list".into(),
            ));
        }
        let mut receipt_payload = &payload[..receipt_header.payload_length];
        payload = &payload[receipt_header.payload_length..];

        let receipt_type = u8::decode(&mut receipt_payload)
            .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
        let status = Eip658Value::decode(&mut receipt_payload)
            .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
        let cumulative_gas_used = u64::decode(&mut receipt_payload)
            .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
        let logs = Vec::<PrimitiveLog>::decode(&mut receipt_payload)
            .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
        if !receipt_payload.is_empty() {
            return Err(SourceError::InvalidEra1Header(
                "slim receipt has trailing RLP data".into(),
            ));
        }

        let logs_bloom = logs_bloom(&logs);
        let with_bloom = ReceiptWithBloom::<Receipt<PrimitiveLog>> {
            receipt: Receipt {
                status,
                cumulative_gas_used,
                logs,
            },
            logs_bloom,
        };
        receipts.push(match receipt_type {
            0 => ReceiptEnvelope::Legacy(with_bloom),
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

    Ok(receipts)
}

pub(crate) fn decode_era1_tx_hashes(
    compressed: &[u8],
) -> Result<Vec<alloy_primitives::B256>, SourceError> {
    let mut raw = Vec::new();
    FrameDecoder::new(compressed)
        .read_to_end(&mut raw)
        .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;

    let mut slice = raw.as_slice();

    let body_header = alloy_rlp::Header::decode(&mut slice)
        .map_err(|e| SourceError::InvalidEra1Header(e.to_string()))?;
    if !body_header.list {
        return Err(SourceError::InvalidEra1Header(
            "body is not an RLP list".into(),
        ));
    }

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
