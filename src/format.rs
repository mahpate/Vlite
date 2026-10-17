use crate::{
    Error, MAX_BATCH_BYTES, MAX_BATCH_OPERATIONS, MAX_DIMENSIONS, MAX_PAYLOAD_BYTES, Mutation,
    Record, Result,
};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub(crate) const HEADER_SIZE: u64 = 32;
pub(crate) const PREFIX_SIZE: usize = 36;
pub(crate) const FOOTER_SIZE: usize = 16;
const MAGIC: &[u8; 8] = b"VLITEDB\0";
pub(crate) const CURRENT_VERSION: u32 = 3;

pub(crate) const MAX_EXTENDED_BODY: usize =
    MAX_DIMENSIONS * 4 + 4 + MAX_PAYLOAD_BYTES + crate::MAX_METADATA_BYTES;

fn extended_body(vector: &[f32], payload: &[u8], metadata: &crate::Metadata) -> Vec<u8> {
    let mut body = Vec::new();
    for value in vector {
        body.extend_from_slice(&value.to_le_bytes());
    }
    body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    body.extend_from_slice(payload);
    body.extend_from_slice(&crate::metadata::encode(metadata));
    body
}

fn decode_extended(body: &[u8], dimensions: usize, offset: u64) -> Result<Record> {
    let vector_end = dimensions * 4;
    if body.len() < vector_end + 8 {
        return Err(corrupt(offset, "truncated metadata record"));
    }
    let vector = decode_vector(&body[..vector_end], offset)?;
    let payload_len = u32_at(body, vector_end) as usize;
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(corrupt(offset, "oversized metadata record payload"));
    }
    let payload_end = vector_end + 4 + payload_len;
    let payload = body
        .get(vector_end + 4..payload_end)
        .ok_or_else(|| corrupt(offset, "truncated metadata payload"))?
        .to_vec();
    let metadata = crate::metadata::decode(
        body.get(payload_end..)
            .ok_or_else(|| corrupt(offset, "missing metadata"))?,
    )
    .map_err(|_| corrupt(offset, "invalid typed metadata encoding"))?;
    Ok(Record {
        vector,
        payload,
        metadata,
    })
}

pub(crate) struct Loaded {
    pub version: u32,
    pub dimensions: usize,
    pub records: BTreeMap<u64, Record>,
    pub sequence: u64,
    pub end: u64,
    pub incomplete_bytes: u64,
}

// IEEE CRC-32, reflected polynomial. Checksums detect accidents, not tampering.
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn corrupt(offset: u64, reason: &'static str) -> Error {
    Error::Corrupt { offset, reason }
}

pub(crate) fn header(dimensions: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_SIZE as usize);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&CURRENT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(dimensions as u32).to_le_bytes());
    bytes.extend_from_slice(&[0; 12]);
    bytes.extend_from_slice(&crc32(&bytes).to_le_bytes());
    bytes
}

pub(crate) fn frame(sequence: u64, id: u64, record: Option<&Record>) -> Vec<u8> {
    let extended = record
        .filter(|record| !record.metadata.is_empty())
        .map(|record| extended_body(&record.vector, &record.payload, &record.metadata));
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"VREC");
    bytes.push(if extended.is_some() {
        4
    } else if record.is_some() {
        1
    } else {
        2
    });
    bytes.extend_from_slice(&[0; 3]);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&id.to_le_bytes());
    let (dimensions, payload_len) = record
        .map(|r| {
            (
                r.vector.len() as u32,
                extended.as_ref().map_or(r.payload.len(), Vec::len) as u32,
            )
        })
        .unwrap_or((0, 0));
    bytes.extend_from_slice(&dimensions.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&crc32(&bytes).to_le_bytes());
    if let Some(body) = extended {
        bytes.extend_from_slice(&body);
    } else if let Some(record) = record {
        for value in &record.vector {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&record.payload);
    }
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(b"VCMT");
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes
}

pub(crate) fn batch_frame(sequence: u64, operations: &[Mutation], body_bytes: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(PREFIX_SIZE + body_bytes + FOOTER_SIZE);
    bytes.extend_from_slice(b"VREC");
    bytes.extend_from_slice(&[3, 0, 0, 0]);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&(operations.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(body_bytes as u32).to_le_bytes());
    bytes.extend_from_slice(&crc32(&bytes).to_le_bytes());
    for operation in operations {
        match operation {
            Mutation::PutWithMetadata {
                id,
                vector,
                payload,
                metadata,
            } => {
                let body = extended_body(vector, payload, metadata);
                bytes.extend_from_slice(&[4, 0, 0, 0]);
                bytes.extend_from_slice(&id.to_le_bytes());
                bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&body);
            }
            Mutation::Put {
                id,
                vector,
                payload,
            } => {
                bytes.extend_from_slice(&[1, 0, 0, 0]);
                bytes.extend_from_slice(&id.to_le_bytes());
                bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                for value in vector {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
                bytes.extend_from_slice(payload);
            }
            Mutation::Delete { id } => {
                bytes.extend_from_slice(&[2, 0, 0, 0]);
                bytes.extend_from_slice(&id.to_le_bytes());
                bytes.extend_from_slice(&0u32.to_le_bytes());
            }
        }
    }
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(b"VCMT");
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes
}

fn decode_vector(bytes: &[u8], offset: u64) -> Result<Vec<f32>> {
    let vector: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect();
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(corrupt(offset, "non-finite stored vector component"));
    }
    Ok(vector)
}

fn decode_batch(
    body: &[u8],
    count: usize,
    dimensions: usize,
    offset: u64,
    version: u32,
) -> Result<Vec<Mutation>> {
    let mut operations = Vec::with_capacity(count);
    let mut position = 0usize;
    for _ in 0..count {
        let entry = body
            .get(position..position + 16)
            .ok_or_else(|| corrupt(offset, "truncated batch entry header"))?;
        let operation = entry[0];
        if !(matches!(operation, 1 | 2) || (operation == 4 && version >= 3))
            || entry[1..4] != [0; 3]
        {
            return Err(corrupt(
                offset,
                "invalid batch entry operation or reserved bytes",
            ));
        }
        let id = u64_at(entry, 4);
        let payload_len = u32_at(entry, 12) as usize;
        if (operation != 4 && payload_len > MAX_PAYLOAD_BYTES)
            || (operation == 4 && payload_len > MAX_EXTENDED_BODY)
            || (operation == 2 && payload_len != 0)
        {
            return Err(corrupt(offset, "invalid batch entry payload length"));
        }
        position += 16;
        if operation == 4 {
            let end = position + payload_len;
            let record = decode_extended(
                body.get(position..end)
                    .ok_or_else(|| corrupt(offset, "truncated extended batch entry"))?,
                dimensions,
                offset,
            )?;
            operations.push(Mutation::PutWithMetadata {
                id,
                vector: record.vector,
                payload: record.payload,
                metadata: record.metadata,
            });
            position = end;
        } else if operation == 1 {
            let end = position + dimensions * 4 + payload_len;
            let data = body
                .get(position..end)
                .ok_or_else(|| corrupt(offset, "truncated batch entry body"))?;
            let vector = decode_vector(&data[..dimensions * 4], offset)?;
            operations.push(Mutation::Put {
                id,
                vector,
                payload: data[dimensions * 4..].to_vec(),
            });
            position = end;
        } else {
            operations.push(Mutation::Delete { id });
        }
    }
    if position != body.len() {
        return Err(corrupt(offset, "extra bytes after batch entries"));
    }
    Ok(operations)
}

pub(crate) fn load(file: &mut File) -> Result<Loaded> {
    file.seek(SeekFrom::Start(0))?;
    let length = file.metadata()?.len();
    if length < HEADER_SIZE {
        return Err(corrupt(0, "missing or truncated database header"));
    }
    let mut bytes = [0; HEADER_SIZE as usize];
    file.read_exact(&mut bytes)?;
    if &bytes[..8] != MAGIC {
        return Err(corrupt(0, "invalid database magic"));
    }
    if crc32(&bytes[..28]) != u32_at(&bytes, 28) {
        return Err(corrupt(0, "database header checksum mismatch"));
    }
    let version = u32_at(&bytes, 8);
    if !matches!(version, 1..=3) {
        return Err(Error::UnsupportedVersion(version));
    }
    if bytes[16..28] != [0; 12] {
        return Err(corrupt(0, "nonzero reserved database header bytes"));
    }
    let dimensions = u32_at(&bytes, 12) as usize;
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        return Err(corrupt(0, "invalid database dimensions"));
    }
    let mut loaded = Loaded {
        version,
        dimensions,
        records: BTreeMap::new(),
        sequence: 0,
        end: HEADER_SIZE,
        incomplete_bytes: 0,
    };
    while loaded.end < length {
        let offset = loaded.end;
        let remaining = length - offset;
        if remaining < PREFIX_SIZE as u64 {
            let mut partial = vec![0; remaining as usize];
            file.read_exact(&mut partial)?;
            let magic_len = partial.len().min(4);
            if partial[..magic_len] != b"VREC"[..magic_len] {
                return Err(corrupt(offset, "invalid trailing record magic"));
            }
            loaded.incomplete_bytes = remaining;
            break;
        }
        let mut prefix = [0; PREFIX_SIZE];
        file.read_exact(&mut prefix)?;
        if &prefix[..4] != b"VREC" {
            return Err(corrupt(offset, "invalid record magic"));
        }
        if crc32(&prefix[..32]) != u32_at(&prefix, 32) {
            return Err(corrupt(offset, "record header checksum mismatch"));
        }
        let operation = prefix[4];
        if !(matches!(operation, 1 | 2)
            || (operation == 3 && version >= 2)
            || (operation == 4 && version >= 3))
            || prefix[5..8] != [0; 3]
        {
            return Err(corrupt(
                offset,
                "invalid operation or reserved record bytes",
            ));
        }
        let sequence = u64_at(&prefix, 8);
        if loaded.sequence.checked_add(1) != Some(sequence) {
            return Err(corrupt(offset, "non-contiguous commit sequence"));
        }
        let id = u64_at(&prefix, 16);
        let count = u32_at(&prefix, 24) as usize;
        let payload_len = u32_at(&prefix, 28) as usize;
        let body_len = if operation == 3 {
            if id != 0
                || !(1..=MAX_BATCH_OPERATIONS).contains(&count)
                || payload_len > MAX_BATCH_BYTES
                || payload_len < count * 16
            {
                return Err(corrupt(
                    offset,
                    "invalid batch count, reserved ID or body length",
                ));
            }
            payload_len
        } else if operation == 4 {
            if count != dimensions
                || payload_len > MAX_EXTENDED_BODY
                || payload_len < dimensions * 4 + 8
            {
                return Err(corrupt(offset, "invalid extended record length"));
            }
            payload_len
        } else {
            if (operation == 1 && count != dimensions)
                || (operation == 2 && (count != 0 || payload_len != 0))
                || payload_len > MAX_PAYLOAD_BYTES
            {
                return Err(corrupt(offset, "invalid vector or payload length"));
            }
            count * 4 + payload_len
        };
        let frame_len = (PREFIX_SIZE + body_len + FOOTER_SIZE) as u64;
        if remaining < frame_len {
            loaded.incomplete_bytes = remaining;
            break;
        }
        let mut frame = prefix.to_vec();
        frame.resize(PREFIX_SIZE + body_len, 0);
        file.read_exact(&mut frame[PREFIX_SIZE..])?;
        let mut footer = [0; FOOTER_SIZE];
        file.read_exact(&mut footer)?;
        if &footer[..4] != b"VCMT"
            || u64_at(&footer, 8) != sequence
            || u32_at(&footer, 4) != crc32(&frame)
        {
            return Err(corrupt(offset, "commit footer or record checksum mismatch"));
        }
        if operation == 1 {
            let vector = decode_vector(&frame[PREFIX_SIZE..PREFIX_SIZE + count * 4], offset)?;
            let payload = frame[PREFIX_SIZE + count * 4..].to_vec();
            loaded.records.insert(
                id,
                Record {
                    vector,
                    payload,
                    metadata: crate::Metadata::new(),
                },
            );
        } else if operation == 4 {
            loaded.records.insert(
                id,
                decode_extended(&frame[PREFIX_SIZE..], dimensions, offset)?,
            );
        } else if operation == 2 {
            loaded.records.remove(&id);
        } else {
            let operations =
                decode_batch(&frame[PREFIX_SIZE..], count, dimensions, offset, version)?;
            for operation in operations {
                match operation {
                    Mutation::Put {
                        id,
                        vector,
                        payload,
                    } => {
                        loaded.records.insert(
                            id,
                            Record {
                                vector,
                                payload,
                                metadata: crate::Metadata::new(),
                            },
                        );
                    }
                    Mutation::PutWithMetadata {
                        id,
                        vector,
                        payload,
                        metadata,
                    } => {
                        loaded.records.insert(
                            id,
                            Record {
                                vector,
                                payload,
                                metadata,
                            },
                        );
                    }
                    Mutation::Delete { id } => {
                        loaded.records.remove(&id);
                    }
                }
            }
        }
        loaded.sequence = sequence;
        loaded.end += frame_len;
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_crc32_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn header_has_stable_portable_encoding() {
        let bytes = header(3);
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[..16], b"VLITEDB\0\x03\0\0\0\x03\0\0\0");
        assert_eq!(u32_at(&bytes, 28), crc32(&bytes[..28]));
    }

    fn load_bytes(bytes: &[u8]) -> Result<Loaded> {
        use std::io::Write;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        load(&mut file)
    }

    fn refresh_header_checksum(bytes: &mut [u8]) {
        let checksum = crc32(&bytes[..28]);
        bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
    }

    fn refresh_record_checksums(bytes: &mut [u8]) {
        let checksum = crc32(&bytes[..32]);
        bytes[32..36].copy_from_slice(&checksum.to_le_bytes());
        let end = bytes.len() - FOOTER_SIZE;
        let checksum = crc32(&bytes[..end]);
        bytes[end + 4..end + 8].copy_from_slice(&checksum.to_le_bytes());
    }

    #[test]
    fn metadata_validation_cannot_be_bypassed_with_valid_checksums() {
        let record = Record {
            vector: vec![1.0],
            payload: vec![],
            metadata: std::collections::BTreeMap::from([(
                "x".into(),
                crate::MetadataValue::Boolean(true),
            )]),
        };
        let original = frame(1, 7, Some(&record));
        let tag_offset = PREFIX_SIZE + 4 + 4 + 4 + 2 + 1;
        for (position, value) in [
            (tag_offset, 99),
            (tag_offset + 1, 2),
            (PREFIX_SIZE + 4, 255),
        ] {
            let mut frame = original.clone();
            frame[position] = value;
            refresh_record_checksums(&mut frame);
            let mut bytes = header(1);
            bytes.extend_from_slice(&frame);
            assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
        }
        for version in [1u32, 2] {
            let mut bytes = header(1);
            bytes[8..12].copy_from_slice(&version.to_le_bytes());
            refresh_header_checksum(&mut bytes);
            bytes.extend_from_slice(&original);
            assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
        }
        let operations = [Mutation::PutWithMetadata {
            id: 7,
            vector: record.vector,
            payload: record.payload,
            metadata: record.metadata,
        }];
        let original = batch_frame(1, &operations, 16 + 4 + 4 + 9);
        let mut frame = original.clone();
        frame[tag_offset + 16] = 99;
        refresh_record_checksums(&mut frame);
        let mut bytes = header(1);
        bytes.extend_from_slice(&frame);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
        let mut bytes = header(1);
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        refresh_header_checksum(&mut bytes);
        bytes.extend_from_slice(&original);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn valid_checksums_do_not_bypass_header_validation() {
        let mut bytes = header(1);
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        refresh_header_checksum(&mut bytes);
        assert!(matches!(
            load_bytes(&bytes),
            Err(Error::UnsupportedVersion(99))
        ));
        for dimensions in [0u32, MAX_DIMENSIONS as u32 + 1, u32::MAX] {
            let mut bytes = header(1);
            bytes[12..16].copy_from_slice(&dimensions.to_le_bytes());
            refresh_header_checksum(&mut bytes);
            assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
        }
        let mut bytes = header(1);
        bytes[16] = 1;
        refresh_header_checksum(&mut bytes);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn valid_checksums_do_not_bypass_record_validation() {
        let record = Record {
            vector: vec![1.0],
            payload: Vec::new(),
            metadata: crate::Metadata::new(),
        };
        let original = frame(1, 0x0807_0605_0403_0201, Some(&record));
        assert_eq!(&original[16..24], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&original[PREFIX_SIZE..PREFIX_SIZE + 4], &[0, 0, 128, 63]);
        let cases: Vec<(usize, Vec<u8>)> = vec![
            (4, vec![3]),
            (5, vec![1]),
            (8, 2u64.to_le_bytes().to_vec()),
            (24, u32::MAX.to_le_bytes().to_vec()),
            (28, u32::MAX.to_le_bytes().to_vec()),
            (PREFIX_SIZE, f32::NAN.to_le_bytes().to_vec()),
            (PREFIX_SIZE, f32::INFINITY.to_le_bytes().to_vec()),
        ];
        for (offset, replacement) in cases {
            let mut frame = original.clone();
            frame[offset..offset + replacement.len()].copy_from_slice(&replacement);
            refresh_record_checksums(&mut frame);
            let mut bytes = header(1);
            bytes.extend_from_slice(&frame);
            assert!(
                matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })),
                "invalid field at offset {offset}"
            );
        }
        let mut invalid_delete = original;
        invalid_delete[4] = 2;
        refresh_record_checksums(&mut invalid_delete);
        let mut bytes = header(1);
        bytes.extend_from_slice(&invalid_delete);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn complete_frame_after_a_sequence_gap_is_not_salvaged() {
        let record = Record {
            vector: vec![1.0],
            payload: Vec::new(),
            metadata: crate::Metadata::new(),
        };
        let mut bytes = header(1);
        bytes.extend_from_slice(&frame(1, 1, Some(&record)));
        bytes.extend_from_slice(&frame(3, 2, Some(&record)));
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }

    #[test]
    fn valid_checksums_cannot_hide_malformed_batch_entries() {
        let operations = [
            Mutation::Put {
                id: 1,
                vector: vec![1.0],
                payload: vec![42],
            },
            Mutation::Delete { id: 2 },
        ];
        let original = batch_frame(1, &operations, 37);
        let cases = [
            (16, 1u64.to_le_bytes().to_vec()),
            (24, 0u32.to_le_bytes().to_vec()),
            (24, 1u32.to_le_bytes().to_vec()),
            (24, 3u32.to_le_bytes().to_vec()),
            (24, (MAX_BATCH_OPERATIONS as u32 + 1).to_le_bytes().to_vec()),
            (28, (MAX_BATCH_BYTES as u32 + 1).to_le_bytes().to_vec()),
            (PREFIX_SIZE, vec![3]),
            (PREFIX_SIZE + 1, vec![1]),
            (PREFIX_SIZE + 12, u32::MAX.to_le_bytes().to_vec()),
            (PREFIX_SIZE + 16, f32::NAN.to_le_bytes().to_vec()),
            (PREFIX_SIZE + 16, f32::INFINITY.to_le_bytes().to_vec()),
            (PREFIX_SIZE + 21 + 12, 1u32.to_le_bytes().to_vec()),
        ];
        for (position, replacement) in cases {
            let mut frame = original.clone();
            frame[position..position + replacement.len()].copy_from_slice(&replacement);
            refresh_record_checksums(&mut frame);
            let mut bytes = header(1);
            bytes.extend_from_slice(&frame);
            assert!(
                matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })),
                "field at {position}"
            );
        }
        // Keep total body bytes unchanged but declare a larger entry payload.
        let mut frame = original.clone();
        frame[PREFIX_SIZE + 12..PREFIX_SIZE + 16].copy_from_slice(&32u32.to_le_bytes());
        refresh_record_checksums(&mut frame);
        let mut bytes = header(1);
        bytes.extend_from_slice(&frame);
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));

        let mut legacy = header(1);
        legacy[8..12].copy_from_slice(&1u32.to_le_bytes());
        refresh_header_checksum(&mut legacy);
        legacy.extend_from_slice(&original);
        assert!(matches!(load_bytes(&legacy), Err(Error::Corrupt { .. })));
    }
}
