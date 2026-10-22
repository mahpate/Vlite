use std::fs;
use tempfile::tempdir;
use vlite::{Database, FormatVersion, Metadata, MetadataValue};

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn header_v5(dimensions: u32) -> Vec<u8> {
    let mut bytes = b"VLITEDB\0".to_vec();
    bytes.extend_from_slice(&5u32.to_le_bytes());
    bytes.extend_from_slice(&dimensions.to_le_bytes());
    bytes.extend_from_slice(&0x1234u64.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes
}

fn frame(operation: u8, sequence: u64, id: u64, count: u32, body: &[u8]) -> Vec<u8> {
    let mut bytes = b"VREC".to_vec();
    bytes.push(operation);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(body);
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(b"VCMT");
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes
}

#[test]
fn v5_persists_expiry_and_metadata_only_frames_with_checksums() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("format-v5.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V5).unwrap();
    db.put_with_expiry(1, &[1.0, 2.0], b"data", &Metadata::new(), Some(u64::MAX))
        .unwrap();
    db.update_metadata(
        1,
        &Metadata::from([("kind".into(), MetadataValue::String("place".into()))]),
    )
    .unwrap();
    db.close().unwrap();

    let bytes = fs::read(&path).unwrap();
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 5);
    assert_eq!(
        bytes[36], 9,
        "expiry-aware put must have its own v5 record operation"
    );
    let next_frame = 32 + 36 + (9 + (2 * 4 + 4 + 4 + 4)) + 16;
    assert_eq!(
        bytes[next_frame + 4],
        8,
        "metadata update must be a compact metadata-only operation"
    );

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.expires_at(1).unwrap(), Some(u64::MAX));
    assert_eq!(
        reopened.get_at(1, 0).unwrap().unwrap().metadata["kind"],
        MetadataValue::String("place".into())
    );
}

#[test]
fn v5_new_operation_corruption_is_rejected_without_repair() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("bad-v5.vlite");
    let mut db = Database::create_with_format(&path, 1, FormatVersion::V5).unwrap();
    db.put_with_expiry(4, &[1.0], b"", &Metadata::new(), Some(1))
        .unwrap();
    db.close().unwrap();
    let valid = fs::read(&path).unwrap();
    let mut bytes = valid.clone();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert!(Database::open(&path).is_err());
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn independently_encoded_v5_expiry_and_metadata_update_frames_replay() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("independent-v5.vlite");
    let mut metadata = Vec::new();
    metadata.extend_from_slice(&1u32.to_le_bytes());
    metadata.extend_from_slice(&3u16.to_le_bytes());
    metadata.extend_from_slice(b"tag");
    metadata.push(2);
    metadata.extend_from_slice(&7i64.to_le_bytes());
    let mut record = vec![1];
    record.extend_from_slice(&300u64.to_le_bytes());
    record.extend_from_slice(&1.0f32.to_le_bytes());
    record.extend_from_slice(&1u32.to_le_bytes());
    record.push(b'x');
    record.extend_from_slice(&metadata);

    let mut updated = Vec::new();
    updated.extend_from_slice(&1u32.to_le_bytes());
    updated.extend_from_slice(&4u16.to_le_bytes());
    updated.extend_from_slice(b"flag");
    updated.extend_from_slice(&[4, 1]);
    let mut bytes = header_v5(1);
    bytes.extend_from_slice(&frame(9, 1, 44, 1, &record));
    bytes.extend_from_slice(&frame(8, 2, 44, 0, &updated));
    fs::write(&path, bytes).unwrap();

    let db = Database::open(path).unwrap();
    let item = db.get_at(44, 299).unwrap().unwrap();
    assert_eq!(item.vector, [1.0]);
    assert_eq!(item.payload, b"x");
    assert_eq!(item.metadata["flag"], MetadataValue::Boolean(true));
    assert_eq!(db.expires_at(44).unwrap(), Some(300));
    assert!(db.get_at(44, 300).unwrap().is_none());
}
