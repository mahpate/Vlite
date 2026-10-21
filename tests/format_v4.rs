//! TEST-001: format v4 round trip and fail-closed validation of every new field.
//! Frames are built by an independent encoder with valid CRCs, so each case
//! exercises semantic validation rather than checksum detection.

use std::fs;
use std::path::Path;
use vlite::{CollectionSettings, CollectionSpec, Database, Error, FormatVersion, maintenance};

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

fn header(version: u32, dimensions: u32, lineage: u64, reserved: u32) -> Vec<u8> {
    let mut bytes = b"VLITEDB\0".to_vec();
    bytes.extend_from_slice(&version.to_le_bytes());
    bytes.extend_from_slice(&dimensions.to_le_bytes());
    bytes.extend_from_slice(&lineage.to_le_bytes());
    bytes.extend_from_slice(&reserved.to_le_bytes());
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes
}

struct Frame {
    op: u8,
    collection: u16,
    byte7: u8,
    sequence: u64,
    id: u64,
    count: u32,
    body: Vec<u8>,
    payload_len: Option<u32>,
}

impl Frame {
    fn new(op: u8, collection: u16, sequence: u64, id: u64, count: u32, body: Vec<u8>) -> Self {
        Self {
            op,
            collection,
            byte7: 0,
            sequence,
            id,
            count,
            body,
            payload_len: None,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = b"VREC".to_vec();
        bytes.push(self.op);
        bytes.extend_from_slice(&self.collection.to_le_bytes());
        bytes.push(self.byte7);
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        bytes.extend_from_slice(&self.id.to_le_bytes());
        bytes.extend_from_slice(&self.count.to_le_bytes());
        let payload_len = self.payload_len.unwrap_or(match self.op {
            1 => self.body.len() as u32 - self.count * 4,
            _ => self.body.len() as u32,
        });
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        let checksum = crc32(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes.extend_from_slice(&self.body);
        let checksum = crc32(&bytes);
        bytes.extend_from_slice(b"VCMT");
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        bytes
    }
}

fn settings_blob(text_fields: &[&str]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(&100u16.to_le_bytes());
    bytes.extend_from_slice(&42u64.to_le_bytes());
    bytes.extend_from_slice(&[0, 4]);
    bytes.extend_from_slice(&4096u32.to_le_bytes());
    bytes.extend_from_slice(&20_000u32.to_le_bytes());
    bytes.push(0);
    bytes.push(text_fields.len() as u8);
    for field in text_fields {
        bytes.extend_from_slice(&(field.len() as u16).to_le_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    bytes
}

fn create(sequence: u64, id: u16, name: &str, dimensions: u32) -> Frame {
    let mut body = vec![name.len() as u8];
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(&dimensions.to_le_bytes());
    body.extend_from_slice(&settings_blob(&[]));
    Frame::new(5, 0, sequence, u64::from(id), 0, body)
}

fn put(sequence: u64, collection: u16, id: u64, vector: &[f32]) -> Frame {
    let body = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
    Frame::new(1, collection, sequence, id, vector.len() as u32, body)
}

fn put_entry(id: u64, vector: &[f32]) -> Vec<u8> {
    let mut bytes = vec![1, 0, 0, 0];
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend(vector.iter().flat_map(|v| v.to_le_bytes()));
    bytes
}

fn marker_entry(job: u64, chunk: u64, rows: u32, len: usize) -> Vec<u8> {
    let mut bytes = vec![8, 0, 0, 0];
    bytes.extend_from_slice(&job.to_le_bytes());
    bytes.extend_from_slice(&(len as u32).to_le_bytes());
    let mut body = Vec::new();
    body.extend_from_slice(&chunk.to_le_bytes());
    body.extend_from_slice(&rows.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&[0xab; 32]);
    body.resize(len, 0);
    bytes.extend_from_slice(&body);
    bytes
}

/// Snapshot checkpoint entry (flag 4). `wide` selects the 60-byte layout
/// (low 32 bits at 8..16, high 32 bits at 52..60); otherwise 52 bytes.
fn snapshot_entry(
    job: u64,
    chunk: u64,
    committed: u64,
    skipped: u64,
    digest: [u8; 32],
    flags: u32,
    wide: bool,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&chunk.to_le_bytes());
    body.extend_from_slice(&(committed as u32).to_le_bytes());
    body.extend_from_slice(&(skipped as u32).to_le_bytes());
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&digest);
    if wide {
        body.extend_from_slice(&((committed >> 32) as u32).to_le_bytes());
        body.extend_from_slice(&((skipped >> 32) as u32).to_le_bytes());
    }
    let mut bytes = vec![8, 0, 0, 0];
    bytes.extend_from_slice(&job.to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    bytes
}

/// REQ-009 digest of an all-valid chunk: SHA-256(canonical entries ++ policy).
fn chunk_digest(entries: &[Vec<u8>], policy: u8) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut bytes = entries.concat();
    bytes.push(policy);
    Sha256::digest(&bytes).into()
}

fn batch(sequence: u64, collection: u16, entries: &[Vec<u8>]) -> Frame {
    Frame::new(
        3,
        collection,
        sequence,
        0,
        entries.len() as u32,
        entries.concat(),
    )
}

fn file(header: Vec<u8>, frames: &[Frame]) -> Vec<u8> {
    let mut bytes = header;
    for frame in frames {
        bytes.extend_from_slice(&frame.encode());
    }
    bytes
}

fn v4(frames: &[Frame]) -> Vec<u8> {
    file(header(4, 2, 0x55, 0), frames)
}

fn open_bytes(path: &Path, bytes: &[u8]) -> vlite::Result<Database> {
    fs::write(path, bytes).unwrap();
    let result = Database::open(path);
    if result.is_err() {
        // Validation never truncates or rewrites.
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    result
}

#[test]
fn independently_encoded_v4_file_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ok.vlite");
    let mut settings = Frame::new(7, 0, 3, 0, 0, settings_blob(&["title"]));
    settings.payload_len = None;
    let bytes = v4(&[
        create(1, 4, "docs", 3),
        put(2, 4, 1, &[1.0, 2.0, 3.0]),
        settings,
        put(4, 0, 1, &[5.0, 6.0]),
        batch(
            5,
            4,
            &[put_entry(2, &[0.0, 0.0, 1.0]), marker_entry(9, 1, 1, 52)],
        ),
    ]);
    let db = open_bytes(&path, &bytes).unwrap();
    assert_eq!(db.format_version(), 4);
    assert_eq!(db.lineage(), Some(0x55));
    let docs = db.collection("docs").unwrap();
    assert_eq!((docs.id(), docs.dimensions(), docs.len()), (4, 3, 2));
    assert_eq!(docs.get(1).unwrap().unwrap().vector, [1.0, 2.0, 3.0]);
    assert_eq!(docs.revision(2).unwrap(), Some(5));
    assert_eq!(db.get(1).unwrap().unwrap().vector, [5.0, 6.0]);
    assert_eq!(
        db.collection("default").unwrap().settings().text_fields,
        ["title"]
    );
    assert_eq!(db.import_status(9).unwrap().rows_committed, 1);
    db.close().unwrap();
    let detail = maintenance::inspect_detail(&path).unwrap();
    assert_eq!(detail.inspection.live_records, 3);
    assert_eq!((detail.collections, detail.open_imports), (2, 1));
}

#[test]
fn api_created_v4_file_reopens_with_collections_and_lineage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("api.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V4).unwrap();
    let lineage = db.lineage().unwrap();
    assert_ne!(lineage, 0);
    db.create_collection(
        "a",
        CollectionSpec {
            dimensions: 3,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    db.put(1, &[1.0, 1.0], b"x").unwrap();
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(db.lineage(), Some(lineage));
    assert_eq!(db.collections().unwrap().len(), 2);
    assert_eq!(db.len(), 1);
    drop(db);
    let bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[16..24], lineage.to_le_bytes());
    assert_eq!(&bytes[24..28], [0; 4]);
}

#[test]
fn every_malformed_v4_field_with_valid_checksums_is_corrupt() {
    let directory = tempfile::tempdir().unwrap();
    let mut nonzero_byte7 = put(1, 0, 1, &[1.0, 1.0]);
    nonzero_byte7.byte7 = 1;
    let mut trailing_settings = settings_blob(&[]);
    trailing_settings.push(0);
    let mut bad_rerank = settings_blob(&[]);
    bad_rerank[16] = 0;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("lineage-zero", file(header(4, 2, 0, 0), &[])),
        ("reserved-header", file(header(4, 2, 1, 1 << 16), &[])),
        ("v3-lineage-bytes", file(header(3, 2, 1, 0), &[])),
        ("unknown-collection", v4(&[put(1, 3, 1, &[1.0, 1.0])])),
        ("nonzero-byte-7", v4(&[nonzero_byte7])),
        (
            "op5-on-v3",
            file(header(3, 2, 0, 0), &[create(1, 1, "docs", 2)]),
        ),
        (
            "op6-on-v3",
            file(header(3, 2, 0, 0), &[Frame::new(6, 0, 1, 1, 0, vec![])]),
        ),
        (
            "op7-on-v3",
            file(
                header(3, 2, 0, 0),
                &[Frame::new(7, 0, 1, 0, 0, settings_blob(&[]))],
            ),
        ),
        (
            "op8-on-v3",
            file(
                header(3, 2, 0, 0),
                &[batch(1, 0, &[marker_entry(1, 1, 0, 52)])],
            ),
        ),
        (
            "collection-on-v3",
            file(header(3, 2, 0, 0), &[put(1, 1, 1, &[1.0, 1.0])]),
        ),
        (
            "duplicate-name",
            v4(&[create(1, 1, "docs", 2), create(2, 2, "docs", 2)]),
        ),
        (
            "duplicate-id",
            v4(&[create(1, 1, "docs", 2), create(2, 1, "other", 2)]),
        ),
        (
            "id-reused-after-drop",
            v4(&[
                create(1, 1, "docs", 2),
                Frame::new(6, 0, 2, 1, 0, vec![]),
                create(3, 1, "docs", 2),
            ]),
        ),
        (
            "id-below-earlier-create",
            v4(&[create(1, 2, "b", 2), create(2, 1, "a", 2)]),
        ),
        (
            "id-at-floor-after-prefix",
            file(
                header(4, 2, 1, 5),
                &[
                    create(1, 3, "kept", 2),
                    put(2, 0, 1, &[1.0, 1.0]),
                    create(3, 4, "reused", 2),
                ],
            ),
        ),
        ("default-name", v4(&[create(1, 1, "default", 2)])),
        ("invalid-name", v4(&[create(1, 1, "Docs", 2)])),
        ("create-id-zero", v4(&[create(1, 0, "docs", 2)])),
        ("zero-dimensions", v4(&[create(1, 1, "docs", 0)])),
        (
            "drop-non-empty",
            v4(&[
                create(1, 1, "docs", 2),
                put(2, 1, 1, &[1.0, 1.0]),
                Frame::new(6, 0, 3, 1, 0, vec![]),
            ]),
        ),
        ("drop-unknown", v4(&[Frame::new(6, 0, 1, 1, 0, vec![])])),
        ("drop-default", v4(&[Frame::new(6, 0, 1, 0, 0, vec![])])),
        (
            "settings-trailing-bytes",
            v4(&[Frame::new(7, 0, 1, 0, 0, trailing_settings)]),
        ),
        (
            "settings-invalid-rerank",
            v4(&[Frame::new(7, 0, 1, 0, 0, bad_rerank)]),
        ),
        (
            "settings-unknown-collection",
            v4(&[Frame::new(7, 0, 1, 2, 0, settings_blob(&[]))]),
        ),
        (
            "marker-not-last",
            v4(&[batch(
                1,
                0,
                &[marker_entry(1, 1, 1, 52), put_entry(1, &[1.0, 1.0])],
            )]),
        ),
        (
            "marker-gap",
            v4(&[batch(
                1,
                0,
                &[put_entry(1, &[1.0, 1.0]), marker_entry(1, 2, 1, 52)],
            )]),
        ),
        (
            "marker-row-count",
            v4(&[batch(
                1,
                0,
                &[put_entry(1, &[1.0, 1.0]), marker_entry(1, 1, 2, 52)],
            )]),
        ),
        (
            "marker-short-body",
            v4(&[batch(1, 0, &[marker_entry(1, 1, 0, 51)])]),
        ),
        (
            "marker-long-body",
            v4(&[batch(1, 0, &[marker_entry(1, 1, 0, 53)])]),
        ),
        (
            "marker-job-zero",
            v4(&[batch(1, 0, &[marker_entry(0, 1, 0, 52)])]),
        ),
        (
            "two-markers",
            v4(&[batch(
                1,
                0,
                &[marker_entry(1, 1, 0, 52), marker_entry(2, 1, 0, 52)],
            )]),
        ),
    ];
    for (name, bytes) in cases {
        let path = directory.path().join(format!("{name}.vlite"));
        let result = open_bytes(&path, &bytes);
        assert!(
            matches!(result, Err(Error::Corrupt { .. })),
            "{name}: {:?}",
            result.err()
        );
    }
}

#[test]
fn header_collection_floor_restates_live_ids_and_blocks_reuse() {
    let directory = tempfile::tempdir().unwrap();
    let spec = || CollectionSpec {
        dimensions: 2,
        settings: CollectionSettings::default(),
    };
    // Floor 5 (bytes 24..26): the leading create of ID 3 restates a live
    // collection, later creates must exceed the floor.
    let path = directory.path().join("floor.vlite");
    let bytes = file(
        header(4, 2, 1, 5),
        &[
            create(1, 3, "kept", 2),
            put(2, 3, 1, &[1.0, 1.0]),
            create(3, 6, "late", 2),
        ],
    );
    let mut db = open_bytes(&path, &bytes).unwrap();
    assert_eq!(db.collection("kept").unwrap().id(), 3);
    assert_eq!(db.collection("late").unwrap().id(), 6);
    assert_eq!(db.create_collection("next", spec()).unwrap().id, 7);
    db.close().unwrap();
    assert_eq!(&fs::read(&path).unwrap()[24..28], [5, 0, 0, 0]);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.collection("next").unwrap().id(), 7);
    drop(db);

    // An exhausted ID space refuses creation without writing anything.
    let path = directory.path().join("exhausted.vlite");
    let bytes = file(header(4, 2, 1, u32::from(u16::MAX)), &[]);
    let mut db = open_bytes(&path, &bytes).unwrap();
    assert!(matches!(
        db.create_collection("none", spec()),
        Err(Error::TooManyCollections)
    ));
    db.put(1, &[1.0, 1.0], b"").unwrap();
    let len = fs::metadata(&path).unwrap().len();
    assert!(matches!(
        db.create_collection("none", spec()),
        Err(Error::TooManyCollections)
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
    assert!(db.collection("none").is_err());
}

#[test]
fn snapshot_checkpoints_keep_full_u64_row_counts_through_compaction() {
    use vlite::{ImportChunkOutcome, ImportState, InvalidRowPolicy, Mutation};
    let directory = tempfile::tempdir().unwrap();
    let reject = InvalidRowPolicy::RejectChunk;
    let row = |id: u64| Mutation::Put {
        id,
        vector: vec![id as f32, 2.0],
        payload: Vec::new(),
    };
    // Counts above u32::MAX in both halves; no billions of ops are needed.
    let committed = 5_000_000_123u64;
    let skipped = (1u64 << 32) + 5;
    let last_chunk = 1_000_000u64;
    let last_rows = [put_entry(1, &[1.0, 2.0])];
    let digest = chunk_digest(&last_rows, 0);
    let path = directory.path().join("high.vlite");
    let bytes = v4(&[batch(
        1,
        0,
        &[snapshot_entry(
            7, last_chunk, committed, skipped, digest, 4, true,
        )],
    )]);
    let mut db = open_bytes(&path, &bytes).unwrap();
    let expect = |db: &Database, chunk: u64, rows: u64| {
        let status = db.import_status(7).unwrap();
        assert_eq!(
            (
                status.state,
                status.last_chunk,
                status.rows_committed,
                status.rows_skipped
            ),
            (ImportState::Open, chunk, rows, skipped)
        );
    };
    expect(&db, last_chunk, committed);
    // The snapshot's digest still identifies the last chunk exactly once.
    assert_eq!(
        db.import_chunk(7, last_chunk, &[row(1)], reject)
            .unwrap()
            .outcome,
        ImportChunkOutcome::AlreadyCommitted
    );
    db.close().unwrap();

    // Exact before/after compaction, then the next chunk continues the counts.
    let compacted = directory.path().join("compacted.vlite");
    maintenance::compact(&path, &compacted).unwrap();
    let mut db = Database::open(&compacted).unwrap();
    expect(&db, last_chunk, committed);
    assert_eq!(
        db.import_chunk(7, last_chunk, &[row(1)], reject)
            .unwrap()
            .outcome,
        ImportChunkOutcome::AlreadyCommitted
    );
    assert!(matches!(
        db.import_chunk(7, last_chunk + 2, &[row(2)], reject),
        Err(Error::ImportConflict(_))
    ));
    let report = db
        .import_chunk(7, last_chunk + 1, &[row(2), row(3)], reject)
        .unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    expect(&db, last_chunk + 1, committed + 2);
    db.close().unwrap();
    let db = Database::open(&compacted).unwrap();
    expect(&db, last_chunk + 1, committed + 2);
    drop(db);
    let twice = directory.path().join("twice.vlite");
    maintenance::upgrade(&compacted, &twice).unwrap();
    let mut db = Database::open(&twice).unwrap();
    expect(&db, last_chunk + 1, committed + 2);
    db.finish_import(7).unwrap();
    assert_eq!(db.import_status(7).unwrap().rows_committed, committed + 2);

    // A job at the u64 limit refuses more rows explicitly and writes nothing.
    let path = directory.path().join("max.vlite");
    let bytes = v4(&[batch(
        1,
        0,
        &[snapshot_entry(9, 1, u64::MAX, 0, digest, 4, true)],
    )]);
    let mut db = open_bytes(&path, &bytes).unwrap();
    assert_eq!(db.import_status(9).unwrap().rows_committed, u64::MAX);
    let len = fs::metadata(&path).unwrap().len();
    assert!(matches!(
        db.import_chunk(9, 2, &[row(5)], reject),
        Err(Error::ImportConflict(_))
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
    assert_eq!(db.import_status(9).unwrap().rows_committed, u64::MAX);

    // Layout/flag mismatches and overflowing replay are corrupt, not clamped.
    let narrow_snapshot = snapshot_entry(7, 1, 1, 0, digest, 4, false);
    let wide_chunk = snapshot_entry(7, 1, 1, 0, digest, 0, true);
    let at_max = snapshot_entry(9, 1, u64::MAX, 0, digest, 4, true);
    let overflow = [
        put_entry(5, &[1.0, 1.0]),
        snapshot_entry(9, 2, 1, 0, digest, 0, false),
    ];
    for (name, bytes) in [
        ("narrow-snapshot", v4(&[batch(1, 0, &[narrow_snapshot])])),
        ("wide-chunk-marker", v4(&[batch(1, 0, &[wide_chunk])])),
        (
            "row-count-overflow",
            v4(&[batch(1, 0, &[at_max]), batch(2, 0, &overflow)]),
        ),
    ] {
        let path = directory.path().join(format!("{name}.vlite"));
        let result = open_bytes(&path, &bytes);
        assert!(
            matches!(result, Err(Error::Corrupt { .. })),
            "{name}: {:?}",
            result.err()
        );
    }
}

#[test]
fn incomplete_v4_tail_is_reported_not_truncated() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tail.vlite");
    let complete = v4(&[create(1, 1, "docs", 2)]);
    let frame = put(2, 1, 1, &[1.0, 1.0]).encode();
    for cut in [1, 20, 36, frame.len() - 1] {
        let mut bytes = complete.clone();
        bytes.extend_from_slice(&frame[..cut]);
        assert!(matches!(
            open_bytes(&path, &bytes),
            Err(Error::IncompleteTail { .. })
        ));
    }
    let mut bytes = complete.clone();
    let mut unknown = put(2, 9, 1, &[1.0, 1.0]).encode();
    unknown.truncate(40);
    bytes.extend_from_slice(&unknown);
    // Prefix fields (including the collection) are validated before the tail check.
    assert!(matches!(
        open_bytes(&path, &bytes),
        Err(Error::Corrupt { .. })
    ));
}
