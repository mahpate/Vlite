use crate::{Error, MAX_DIMENSIONS, MAX_PAYLOAD_BYTES, Record, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub(crate) const HEADER_SIZE: u64 = 32;
pub(crate) const PREFIX_SIZE: usize = 36;
pub(crate) const FOOTER_SIZE: usize = 16;
const MAGIC: &[u8; 8] = b"VLITEDB\0";
const VERSION: u32 = 1;

pub(crate) struct Loaded {
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
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(dimensions as u32).to_le_bytes());
    bytes.extend_from_slice(&[0; 12]);
    bytes.extend_from_slice(&crc32(&bytes).to_le_bytes());
    bytes
}

pub(crate) fn frame(sequence: u64, id: u64, record: Option<&Record>) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"VREC");
    bytes.push(if record.is_some() { 1 } else { 2 });
    bytes.extend_from_slice(&[0; 3]);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&id.to_le_bytes());
    let (dimensions, payload_len) = record
        .map(|r| (r.vector.len() as u32, r.payload.len() as u32))
        .unwrap_or((0, 0));
    bytes.extend_from_slice(&dimensions.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&crc32(&bytes).to_le_bytes());
    if let Some(record) = record {
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
    if version != VERSION {
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
        if !matches!(operation, 1 | 2) || prefix[5..8] != [0; 3] {
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
        if (operation == 1 && count != dimensions)
            || (operation == 2 && (count != 0 || payload_len != 0))
            || payload_len > MAX_PAYLOAD_BYTES
        {
            return Err(corrupt(offset, "invalid vector or payload length"));
        }
        let body_len = count * 4 + payload_len;
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
            let vector: Vec<f32> = frame[PREFIX_SIZE..PREFIX_SIZE + count * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| f32::from_le_bytes(*chunk))
                .collect();
            if vector.iter().any(|value| !value.is_finite()) {
                return Err(corrupt(offset, "non-finite stored vector component"));
            }
            let payload = frame[PREFIX_SIZE + count * 4..].to_vec();
            loaded.records.insert(id, Record { vector, payload });
        } else {
            loaded.records.remove(&id);
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
        assert_eq!(&bytes[..16], b"VLITEDB\0\x01\0\0\0\x03\0\0\0");
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
        };
        let mut bytes = header(1);
        bytes.extend_from_slice(&frame(1, 1, Some(&record)));
        bytes.extend_from_slice(&frame(3, 2, Some(&record)));
        assert!(matches!(load_bytes(&bytes), Err(Error::Corrupt { .. })));
    }
}
