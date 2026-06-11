//! Archive block codec: Snappy/RLP decoding for headers, receipts, and body
//! transaction hashes.
//!
//! One receipt decode loop owns Snappy decompression, RLP list iteration, and
//! receipt-type dispatch for both archive formats. The ERA1/ERE difference is
//! confined to a small per-item layout adapter selected via [`ReceiptLayout`].

use crate::types::ScopeHeader;
use alloy_consensus::{Eip658Value, Header, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{keccak256, logs_bloom, Log as PrimitiveLog, B256};
use alloy_rlp::Decodable;
use snap::read::FrameDecoder;
use std::io::Read;
use thiserror::Error;

/// Decode failure in the codec layer.
///
/// `what` names the section and archive format honestly at the point of
/// failure, so a log line mid-failure never misdirects to the wrong format.
#[derive(Debug, Error)]
#[error("invalid {what}: {message}")]
pub struct CodecError {
    /// What was being decoded, e.g. "ERE slim receipts".
    pub what: &'static str,
    /// Why decoding failed.
    pub message: String,
}

/// Which on-disk receipt layout a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptLayout {
    /// ERA1 full receipts: each list item is a legacy `ReceiptWithBloom` RLP
    /// list, or a typed receipt as a byte string / raw type byte followed by
    /// the `ReceiptWithBloom` payload.
    Era1Full,
    /// ERE slim receipts: each list item is `[type, status, cumulative_gas,
    /// logs]`, with the logs bloom recomputed from the logs.
    EreSlim,
}

impl ReceiptLayout {
    fn section(self) -> &'static str {
        match self {
            Self::Era1Full => "ERA1 receipts",
            Self::EreSlim => "ERE slim receipts",
        }
    }
}

/// Decode a Snappy-compressed RLP receipt list in the given layout.
///
/// This is the one decode loop for both archive formats: decompression, outer
/// list iteration, and receipt-type dispatch live here. Only the per-item
/// field layout differs between formats.
pub fn decode_receipts(
    layout: ReceiptLayout,
    compressed: &[u8],
) -> Result<Vec<ReceiptEnvelope<PrimitiveLog>>, CodecError> {
    let what = layout.section();
    let err = |message: String| CodecError { what, message };

    let mut raw = Vec::new();
    FrameDecoder::new(compressed)
        .read_to_end(&mut raw)
        .map_err(|e| err(e.to_string()))?;

    let mut slice = raw.as_slice();
    let list_header = alloy_rlp::Header::decode(&mut slice).map_err(|e| err(e.to_string()))?;
    if !list_header.list {
        return Err(err("receipts block is not an RLP list".into()));
    }

    let mut payload = &slice[..list_header.payload_length];
    let mut receipts = Vec::new();

    while !payload.is_empty() {
        let envelope = match layout {
            ReceiptLayout::Era1Full => decode_full_receipt(&mut payload).map_err(err)?,
            ReceiptLayout::EreSlim => decode_slim_receipt(&mut payload).map_err(err)?,
        };
        receipts.push(envelope);
    }

    Ok(receipts)
}

/// Map an explicit receipt type byte to its envelope variant. Both layout
/// adapters share this dispatch; type 0 is never explicit (ERA1 legacy
/// receipts are bare RLP lists, ERE handles 0 before calling this).
fn typed_envelope(
    receipt_type: u8,
    with_bloom: ReceiptWithBloom<Receipt<PrimitiveLog>>,
) -> Result<ReceiptEnvelope<PrimitiveLog>, String> {
    match receipt_type {
        1 => Ok(ReceiptEnvelope::Eip2930(with_bloom)),
        2 => Ok(ReceiptEnvelope::Eip1559(with_bloom)),
        3 => Ok(ReceiptEnvelope::Eip4844(with_bloom)),
        4 => Ok(ReceiptEnvelope::Eip7702(with_bloom)),
        t => Err(format!("unknown receipt type: {t}")),
    }
}

/// ERA1 layout adapter: one full receipt — a legacy `ReceiptWithBloom` RLP
/// list, a typed receipt in an RLP byte string, or a raw type byte followed
/// by the receipt list.
fn decode_full_receipt(
    payload: &mut &[u8],
) -> Result<ReceiptEnvelope<PrimitiveLog>, String> {
    let Some(&first) = payload.first() else {
        return Err("empty receipt item".into());
    };

    if first >= 0xc0 {
        let with_bloom = ReceiptWithBloom::<Receipt<PrimitiveLog>>::decode(payload)
            .map_err(|e| e.to_string())?;
        return Ok(ReceiptEnvelope::Legacy(with_bloom));
    }

    if first >= 0x80 {
        let item_header = alloy_rlp::Header::decode(payload).map_err(|e| e.to_string())?;
        let item = &payload[..item_header.payload_length];
        *payload = &payload[item_header.payload_length..];

        let receipt_type = *item
            .first()
            .ok_or_else(|| "typed receipt byte string is empty".to_string())?;
        let mut body = &item[1..];
        let with_bloom =
            ReceiptWithBloom::<Receipt<PrimitiveLog>>::decode(&mut body).map_err(|e| e.to_string())?;
        return typed_envelope(receipt_type, with_bloom);
    }

    let receipt_type = first;
    *payload = &payload[1..];
    let with_bloom =
        ReceiptWithBloom::<Receipt<PrimitiveLog>>::decode(payload).map_err(|e| e.to_string())?;
    typed_envelope(receipt_type, with_bloom)
}

/// ERE layout adapter: one slim receipt — an RLP list of `[type, status,
/// cumulative_gas, logs]` with the bloom recomputed from the logs.
fn decode_slim_receipt(
    payload: &mut &[u8],
) -> Result<ReceiptEnvelope<PrimitiveLog>, String> {
    let receipt_header = alloy_rlp::Header::decode(payload).map_err(|e| e.to_string())?;
    if !receipt_header.list {
        return Err("slim receipt is not an RLP list".into());
    }
    let mut receipt_payload = &payload[..receipt_header.payload_length];
    *payload = &payload[receipt_header.payload_length..];

    let receipt_type = u8::decode(&mut receipt_payload).map_err(|e| e.to_string())?;
    let status = Eip658Value::decode(&mut receipt_payload).map_err(|e| e.to_string())?;
    let cumulative_gas_used = u64::decode(&mut receipt_payload).map_err(|e| e.to_string())?;
    let logs = Vec::<PrimitiveLog>::decode(&mut receipt_payload).map_err(|e| e.to_string())?;
    if !receipt_payload.is_empty() {
        return Err("slim receipt has trailing RLP data".into());
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
    if receipt_type == 0 {
        return Ok(ReceiptEnvelope::Legacy(with_bloom));
    }
    typed_envelope(receipt_type, with_bloom)
}

/// Decode a Snappy-compressed RLP block header. Both archive formats share
/// this header encoding.
pub fn decode_era1_header(compressed_header: &[u8]) -> Result<ScopeHeader, CodecError> {
    let err = |message: String| CodecError {
        what: "block header",
        message,
    };

    let mut decoder = FrameDecoder::new(compressed_header);
    let mut rlp = Vec::new();
    decoder
        .read_to_end(&mut rlp)
        .map_err(|source| err(source.to_string()))?;

    let mut slice = rlp.as_slice();
    let header = Header::decode(&mut slice).map_err(|source| err(source.to_string()))?;

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

/// Compute transaction hashes from a Snappy-compressed RLP block body. Both
/// archive formats share this body encoding.
pub(crate) fn decode_era1_tx_hashes(compressed: &[u8]) -> Result<Vec<B256>, CodecError> {
    let err = |message: String| CodecError {
        what: "block body",
        message,
    };

    let mut raw = Vec::new();
    FrameDecoder::new(compressed)
        .read_to_end(&mut raw)
        .map_err(|e| err(e.to_string()))?;

    let mut slice = raw.as_slice();

    let body_header = alloy_rlp::Header::decode(&mut slice).map_err(|e| err(e.to_string()))?;
    if !body_header.list {
        return Err(err("body is not an RLP list".into()));
    }

    let tx_list_header = alloy_rlp::Header::decode(&mut slice).map_err(|e| err(e.to_string()))?;
    if !tx_list_header.list {
        return Err(err("body transactions is not an RLP list".into()));
    }

    let mut tx_payload = &slice[..tx_list_header.payload_length];
    let mut hashes = Vec::new();

    while !tx_payload.is_empty() {
        let Some(&first) = tx_payload.first() else {
            break;
        };

        let raw_tx: Vec<u8> = if first >= 0xc0 {
            let start = tx_payload;
            let h = match alloy_rlp::Header::decode(&mut tx_payload) {
                Ok(h) => h,
                Err(_) => {
                    hashes.push(B256::ZERO);
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
                    hashes.push(B256::ZERO);
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
                    hashes.push(B256::ZERO);
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom};
    use alloy_primitives::{logs_bloom, Address, Bloom, Bytes, LogData, B256};
    use alloy_rlp::{Encodable, Header as RlpHeader};
    use std::io::Write;

    fn snappy_bytes(bytes: &[u8]) -> Vec<u8> {
        let mut compressed = Vec::new();
        {
            let mut encoder = snap::write::FrameEncoder::new(&mut compressed);
            encoder.write_all(bytes).unwrap();
            encoder.flush().unwrap();
        }
        compressed
    }

    fn rlp_list(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        RlpHeader {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(payload);
        out
    }

    fn sample_log() -> PrimitiveLog {
        PrimitiveLog {
            address: Address::repeat_byte(0x11),
            data: LogData::new_unchecked(
                vec![B256::repeat_byte(0x22)],
                Bytes::from(vec![0x01, 0x02, 0x03]),
            ),
        }
    }

    fn full_receipt(gas: u64, logs: Vec<PrimitiveLog>) -> ReceiptWithBloom<Receipt<PrimitiveLog>> {
        let bloom = logs_bloom(&logs);
        ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: gas,
                logs,
            },
            logs_bloom: bloom,
        }
    }

    /// ERA1 list item: legacy receipt encoded directly as an RLP list.
    fn era1_legacy_item(receipt: &ReceiptWithBloom<Receipt<PrimitiveLog>>) -> Vec<u8> {
        let mut item = Vec::new();
        receipt.encode(&mut item);
        item
    }

    /// ERA1 list item: typed receipt wrapped in an RLP byte string
    /// (`rlp_string(type || rlp(receipt))`).
    fn era1_typed_string_item(
        receipt_type: u8,
        receipt: &ReceiptWithBloom<Receipt<PrimitiveLog>>,
    ) -> Vec<u8> {
        let mut inner = vec![receipt_type];
        receipt.encode(&mut inner);
        let mut item = Vec::new();
        inner.as_slice().encode(&mut item);
        item
    }

    /// ERA1 list item: typed receipt as a raw type byte followed by the
    /// receipt RLP list (no byte-string wrapper).
    fn era1_typed_raw_item(
        receipt_type: u8,
        receipt: &ReceiptWithBloom<Receipt<PrimitiveLog>>,
    ) -> Vec<u8> {
        let mut item = vec![receipt_type];
        receipt.encode(&mut item);
        item
    }

    /// ERE list item: `[type, status, cumulative_gas, logs]`.
    fn ere_slim_item(receipt_type: u8, gas: u64, logs: &[PrimitiveLog]) -> Vec<u8> {
        let mut payload = Vec::new();
        receipt_type.encode(&mut payload);
        Eip658Value::Eip658(true).encode(&mut payload);
        gas.encode(&mut payload);
        logs.to_vec().encode(&mut payload);
        rlp_list(&payload)
    }

    fn compressed_list(items: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = items.iter().flatten().copied().collect();
        snappy_bytes(&rlp_list(&payload))
    }

    // ── ERA1 full layout ─────────────────────────────────────────────────────

    #[test]
    fn era1_empty_list_decodes_to_no_receipts() {
        let receipts = decode_receipts(ReceiptLayout::Era1Full, &snappy_bytes(&[0xC0])).unwrap();
        assert!(receipts.is_empty());
    }

    #[test]
    fn era1_legacy_receipt_decodes_as_legacy_envelope() {
        let receipt = full_receipt(21_000, vec![sample_log()]);
        let bytes = compressed_list(&[era1_legacy_item(&receipt)]);

        let receipts = decode_receipts(ReceiptLayout::Era1Full, &bytes).unwrap();

        assert_eq!(receipts.len(), 1);
        assert!(matches!(receipts[0], ReceiptEnvelope::Legacy(_)));
        let inner = receipts[0].as_receipt_with_bloom().unwrap();
        assert_eq!(inner.receipt.cumulative_gas_used, 21_000);
        assert_eq!(inner.receipt.logs, vec![sample_log()]);
    }

    #[test]
    fn era1_typed_byte_string_receipts_dispatch_on_type() {
        let receipt = full_receipt(42_000, vec![]);
        let bytes = compressed_list(&[
            era1_typed_string_item(1, &receipt),
            era1_typed_string_item(2, &receipt),
            era1_typed_string_item(3, &receipt),
            era1_typed_string_item(4, &receipt),
        ]);

        let receipts = decode_receipts(ReceiptLayout::Era1Full, &bytes).unwrap();

        assert_eq!(receipts.len(), 4);
        assert!(matches!(receipts[0], ReceiptEnvelope::Eip2930(_)));
        assert!(matches!(receipts[1], ReceiptEnvelope::Eip1559(_)));
        assert!(matches!(receipts[2], ReceiptEnvelope::Eip4844(_)));
        assert!(matches!(receipts[3], ReceiptEnvelope::Eip7702(_)));
    }

    #[test]
    fn era1_raw_type_byte_receipt_dispatches_on_type() {
        let receipt = full_receipt(30_000, vec![]);
        let bytes = compressed_list(&[era1_typed_raw_item(2, &receipt)]);

        let receipts = decode_receipts(ReceiptLayout::Era1Full, &bytes).unwrap();

        assert_eq!(receipts.len(), 1);
        assert!(matches!(receipts[0], ReceiptEnvelope::Eip1559(_)));
    }

    #[test]
    fn era1_unknown_receipt_type_errors_naming_era1() {
        let receipt = full_receipt(21_000, vec![]);
        let bytes = compressed_list(&[era1_typed_string_item(9, &receipt)]);

        let err = decode_receipts(ReceiptLayout::Era1Full, &bytes).unwrap_err();

        assert!(
            err.to_string().contains("unknown receipt type: 9"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("ERA1"),
            "error must name the ERA1 format: {err}"
        );
    }

    /// ERA1 never uses an explicit type byte 0 — legacy receipts are bare RLP
    /// lists. An explicit 0 stays rejected, exactly as before unification.
    #[test]
    fn era1_explicit_type_zero_is_rejected() {
        let receipt = full_receipt(21_000, vec![]);
        let bytes = compressed_list(&[era1_typed_string_item(0, &receipt)]);

        let err = decode_receipts(ReceiptLayout::Era1Full, &bytes).unwrap_err();

        assert!(
            err.to_string().contains("unknown receipt type: 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn era1_non_list_payload_errors_naming_era1() {
        // RLP byte string "abc" instead of a list.
        let mut payload = Vec::new();
        b"abc".as_slice().encode(&mut payload);

        let err = decode_receipts(ReceiptLayout::Era1Full, &snappy_bytes(&payload)).unwrap_err();

        assert!(
            err.to_string().contains("not an RLP list"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("ERA1"),
            "error must name the ERA1 format: {err}"
        );
    }

    #[test]
    fn era1_garbage_snappy_errors() {
        let err =
            decode_receipts(ReceiptLayout::Era1Full, &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap_err();
        assert!(err.to_string().contains("ERA1"), "unexpected error: {err}");
    }

    // ── ERE slim layout ──────────────────────────────────────────────────────

    #[test]
    fn ere_empty_list_decodes_to_no_receipts() {
        let receipts = decode_receipts(ReceiptLayout::EreSlim, &snappy_bytes(&[0xC0])).unwrap();
        assert!(receipts.is_empty());
    }

    #[test]
    fn ere_slim_legacy_receipt_recomputes_bloom_from_logs() {
        let logs = vec![sample_log()];
        let bytes = compressed_list(&[ere_slim_item(0, 21_000, &logs)]);

        let receipts = decode_receipts(ReceiptLayout::EreSlim, &bytes).unwrap();

        assert_eq!(receipts.len(), 1);
        assert!(matches!(receipts[0], ReceiptEnvelope::Legacy(_)));
        let inner = receipts[0].as_receipt_with_bloom().unwrap();
        assert_eq!(inner.receipt.cumulative_gas_used, 21_000);
        assert_eq!(inner.receipt.logs, logs);
        assert_eq!(inner.logs_bloom, logs_bloom(&logs));
        assert_ne!(inner.logs_bloom, Bloom::default());
    }

    #[test]
    fn ere_slim_typed_receipts_dispatch_on_type() {
        let bytes = compressed_list(&[
            ere_slim_item(1, 1_000, &[]),
            ere_slim_item(2, 2_000, &[]),
            ere_slim_item(3, 3_000, &[]),
            ere_slim_item(4, 4_000, &[]),
        ]);

        let receipts = decode_receipts(ReceiptLayout::EreSlim, &bytes).unwrap();

        assert_eq!(receipts.len(), 4);
        assert!(matches!(receipts[0], ReceiptEnvelope::Eip2930(_)));
        assert!(matches!(receipts[1], ReceiptEnvelope::Eip1559(_)));
        assert!(matches!(receipts[2], ReceiptEnvelope::Eip4844(_)));
        assert!(matches!(receipts[3], ReceiptEnvelope::Eip7702(_)));
    }

    #[test]
    fn ere_unknown_receipt_type_errors_naming_ere() {
        let bytes = compressed_list(&[ere_slim_item(9, 21_000, &[])]);

        let err = decode_receipts(ReceiptLayout::EreSlim, &bytes).unwrap_err();

        assert!(
            err.to_string().contains("unknown receipt type: 9"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("ERE"),
            "error must name the ERE format: {err}"
        );
    }

    #[test]
    fn ere_slim_receipt_with_trailing_data_errors() {
        // A slim receipt list with an extra RLP item after the logs.
        let mut payload = Vec::new();
        0_u8.encode(&mut payload);
        Eip658Value::Eip658(true).encode(&mut payload);
        21_000_u64.encode(&mut payload);
        Vec::<PrimitiveLog>::new().encode(&mut payload);
        7_u8.encode(&mut payload); // trailing junk
        let bytes = compressed_list(&[rlp_list(&payload)]);

        let err = decode_receipts(ReceiptLayout::EreSlim, &bytes).unwrap_err();

        assert!(
            err.to_string().contains("trailing"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ere_non_list_payload_errors_naming_ere() {
        let mut payload = Vec::new();
        b"abc".as_slice().encode(&mut payload);

        let err = decode_receipts(ReceiptLayout::EreSlim, &snappy_bytes(&payload)).unwrap_err();

        assert!(
            err.to_string().contains("not an RLP list"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("ERE"),
            "error must name the ERE format: {err}"
        );
    }

    #[test]
    fn ere_garbage_snappy_errors() {
        let err = decode_receipts(ReceiptLayout::EreSlim, &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap_err();
        assert!(err.to_string().contains("ERE"), "unexpected error: {err}");
    }

    // ── Block body ───────────────────────────────────────────────────────────

    #[test]
    fn decodes_empty_body_as_no_tx_hashes() {
        // RLP of [[],[]] — empty transactions list and empty uncles list.
        let compressed = snappy_bytes(&rlp_list(&[0xC0, 0xC0]));

        let hashes = decode_era1_tx_hashes(&compressed).unwrap();
        assert!(hashes.is_empty());
    }
}
