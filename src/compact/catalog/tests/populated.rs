//! Float16 dense values in populated precision stores (review 9ebcbeff,
//! HIGH, and its transport finding). A float16 collection holds only
//! float16 values: full replay, the change feed and log export each verify
//! every primary and named dense value bit for bit (never rounding it) and
//! refuse any other as corruption before they return anything, and every
//! write rounds its dense values to float16 before it commits
//! (`tests/canonical.rs`). Float32 collections keep every finite value
//! unchanged.

use super::*;
use crate::DistanceMetric;
use crate::vectors::{RecordVectors, ReembedUpdate, VectorSpec, VersionedVector};
use std::collections::{BTreeMap, BTreeSet};

/// Values a float16 collection stores exactly: 0.5, negative zero, the
/// smallest subnormal (2^-24) and the largest finite value.
const EXACT: [f32; 4] = [0.5, -0.0, f32::from_bits(0x3380_0000), 65504.0];
/// 0.1 is no float16 value, and 65520 rounds to infinity.
const INEXACT: [[f32; 4]; 2] = [[0.1, 0.0, 0.0, 0.0], [65520.0, 0.0, 0.0, 0.0]];

/// The frame operation each dense write path of `write` commits: primary
/// puts (ops 1, 4 and 9), a batch, a batch with metadata and an import
/// chunk (op 3), a graph vector entry (op 12), and named versions (op 10).
const PATHS: [u8; 9] = [1, 4, 9, 3, 3, 3, 12, 10, 10];

/// Writes `vector` into `half` through dense write path `path`: record 2's
/// primary vector, or record 1's named `n` version `v2`.
fn write(db: &mut Database, path: usize, vector: &[f32]) -> Result<()> {
    let tagged = Metadata::from([("k".to_owned(), crate::MetadataValue::Integer(1))]);
    let put = Mutation::Put {
        id: 2,
        vector: vector.to_vec(),
        payload: b"p".to_vec(),
    };
    let named = |version: &str, vector: &[f32]| VersionedVector {
        version: version.into(),
        vector: vector.to_vec(),
    };
    if path == 5 {
        let job = db.next_import_job_id()?;
        return db
            .collection_mut("half")?
            .import_chunk(job, 1, &[put], POLICY)
            .map(drop);
    }
    if path == 6 {
        let entry = GraphOp::Vector {
            collection: "half".into(),
            mutation: put,
        };
        return db.graph_mut("g")?.write_batch(&[entry]).map(drop);
    }
    let mut half = db.collection_mut("half")?;
    match path {
        0 => half.put(2, vector, b"p").map(drop),
        1 => half.put_with_metadata(2, vector, b"p", &tagged).map(drop),
        2 => half
            .put_with_expiry(2, vector, b"p", &tagged, Some(FAR))
            .map(drop),
        3 => half.write_batch(&[put]),
        4 => half.write_batch(&[Mutation::PutWithMetadata {
            id: 2,
            vector: vector.to_vec(),
            payload: b"p".to_vec(),
            metadata: tagged,
        }]),
        7 => half.put_vector_version(1, "n", "v2", vector.to_vec()),
        _ => half.put_vectors(
            1,
            RecordVectors {
                dense: BTreeMap::from([(
                    "n".into(),
                    vec![named("v1", &EXACT), named("v2", vector)],
                )]),
                sparse: BTreeMap::new(),
            },
        ),
    }
}

/// Complete images of a precision store whose `half` (ID 1, four
/// dimensions) holds record 1 with primary vector `primary` and named
/// vector `n` version `v1` `named`, beside graph `g`: with `half` float32,
/// as written, and with it float16.
fn populated(key: Option<&EncryptionKey>, primary: [f32; 4], named: [f32; 4]) -> [Vec<u8>; 2] {
    let mut db = Database::open_from_bytes(image_of(&initial(key)), key).unwrap();
    db.create_collection_as("half", spec(4), F32).unwrap();
    db.create_graph("g").unwrap();
    let mut half = db.collection_mut("half").unwrap();
    half.put(1, &primary, b"p").unwrap();
    let spec = VectorSpec {
        dimensions: 4,
        metric: DistanceMetric::SquaredL2,
        active_version: "v1".into(),
        history: 1,
    };
    half.configure_vector("n", spec).unwrap();
    half.put_vector_version(1, "n", "v1", named.to_vec())
        .unwrap();
    let mut loaded = load(&db.to_bytes().unwrap(), key).unwrap();
    let full = image_of(&loaded);
    loaded.collections.get_mut(&1).unwrap().precision = Some(F16);
    [full, image_of(&loaded)]
}

/// The bits of every primary and named dense value `half` holds.
fn dense_bits(loaded: &Loaded) -> BTreeSet<u32> {
    let half = &loaded.collections[&1];
    let primary = half.records.values().flat_map(|record| &record.vector);
    let named = half
        .vectors
        .records
        .values()
        .flat_map(|record| record.dense.values().flatten())
        .flat_map(|version| &version.vector);
    primary.chain(named).map(|value| value.to_bits()).collect()
}

fn exact_bits() -> BTreeSet<u32> {
    EXACT.iter().map(|value| value.to_bits()).collect()
}

/// Outcomes of full replay, the change feed from `floor` and log export
/// after `floor` of `log`.
fn readers(log: &[u8], key: Option<&EncryptionKey>, floor: u64) -> [String; 3] {
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: floor,
    };
    let feed = crate::changes::read_changes(&mut Cursor::new(log), key, None, &cursor, 16);
    let export = logops::read_segment(&mut Cursor::new(log), key, None, floor, 1 << 20);
    [outcome(load(log, key)), outcome(feed), outcome(export)]
}

fn each(outcome: &str) -> [String; 3] {
    std::array::from_fn(|_| outcome.to_owned())
}

fn readers_verify_float16_values(key: Option<&EncryptionKey>) {
    let _gate = Activation::on();
    let [full, half] = populated(key, EXACT, EXACT);
    let floor = load(&full, key).unwrap().history_floor;
    let loaded = load(&half, key).unwrap();
    assert_eq!(loaded.collections[&1].precision, Some(F16));
    assert_eq!(dense_bits(&loaded), exact_bits());
    assert_eq!(readers(&half, key, floor), each("ok"));
    if key.is_none() {
        // The image restates record 1 in a record block and its named
        // vectors in an op-10 frame.
        let (_, frames) = split(&half);
        let ops: Vec<u8> = frames.iter().map(|frame| frame[4]).collect();
        assert!(ops.contains(&14) && ops.contains(&10), "{ops:?}");
    }
    // An image that restates any other value is corrupt, as float16 and
    // only as float16, to every reader.
    for vector in INEXACT {
        for [full, half] in [populated(key, vector, EXACT), populated(key, EXACT, vector)] {
            assert_eq!(readers(&full, key, floor), each("ok"));
            assert_eq!(readers(&half, key, floor), each("corrupt"), "{vector:?}");
        }
    }
    // Ordinary frames after the image, written by a float32 `half`.
    for (path, &operation) in PATHS.iter().enumerate() {
        for (vector, expected) in [
            (EXACT, "ok"),
            (INEXACT[0], "corrupt"),
            (INEXACT[1], "corrupt"),
        ] {
            let mut db = Database::open_from_bytes(full.clone(), key).unwrap();
            write(&mut db, path, &vector).unwrap();
            let log = db.to_bytes().unwrap();
            assert!(log.starts_with(&full));
            let suffix = &log[full.len()..];
            assert_eq!(suffix[4], operation, "{path}");
            assert_eq!(readers(&log, key, floor), each("ok"), "{path}");
            let mut log = half.clone();
            log.extend_from_slice(suffix);
            assert_eq!(
                readers(&log, key, floor),
                each(expected),
                "{path} {vector:?}"
            );
            if expected == "ok" {
                let loaded = load(&log, key).unwrap();
                assert_eq!(dense_bits(&loaded), exact_bits(), "{path}");
                let cursor = FeedCursor {
                    lineage: LINEAGE,
                    sequence: floor,
                };
                let (batch, _) =
                    crate::changes::read_changes(&mut Cursor::new(&log), key, None, &cursor, 16)
                        .unwrap();
                assert!(!batch.changes.is_empty(), "{path}");
                assert!(
                    batch
                        .changes
                        .iter()
                        .all(|change| change.sequence == floor + 1)
                );
                let (segment, _) =
                    logops::read_segment(&mut Cursor::new(&log), key, None, floor, 1 << 20)
                        .unwrap();
                assert_eq!(segment.frames, suffix, "{path}");
            }
        }
    }
}

#[test]
fn readers_verify_float16_values_bit_for_bit() {
    readers_verify_float16_values(None);
    readers_verify_float16_values(Some(&key()));
}

/// `base` followed by `frames`.
fn with(base: &[u8], frames: &[&[u8]]) -> Vec<u8> {
    let mut log = base.to_vec();
    for frame in frames {
        log.extend_from_slice(frame);
    }
    log
}

/// The frames that `write` commits to the plaintext store `log`.
fn committed(log: Vec<u8>, write: impl FnOnce(&mut Database) -> Result<()>) -> Vec<u8> {
    let mut db = Database::open_from_bytes(log.clone(), None).unwrap();
    write(&mut db).unwrap();
    db.to_bytes().unwrap()[log.len()..].to_vec()
}

#[test]
fn readers_follow_precision_through_creates_and_drops() {
    let _gate = Activation::on();
    let [full, half] = populated(None, EXACT, EXACT);
    let floor = load(&full, None).unwrap().history_floor;
    let create = |at: u64, id: u16, name: &str, precision| {
        let settings = CollectionSettings::default();
        format::precise_create_frame(floor + at, id, name, 4, &settings, precision)
    };
    let put = |log: Vec<u8>, name: &str, vector: [f32; 4]| {
        committed(log, |db| {
            db.collection_mut(name)?.put(1, &vector, b"p").map(drop)
        })
    };
    // `later` (ID 9) created after the image, as float32 and as float16,
    // and record 1 put into it at the next sequence.
    let [later32, later16] = [F32, F16].map(|precision| create(1, 9, "later", precision));
    let into_later = |vector| put(with(&full, &[&later32]), "later", vector);
    let [exact, inexact, overflow] = [EXACT, INEXACT[0], INEXACT[1]].map(into_later);
    // `later` dropped empty, `again` (ID 10, float32) created and 0.1 put
    // into it; and the same with `later`'s ID reused instead, which a
    // reader that forgot the float16 `later` would take for float32.
    let dropped = committed(with(&full, &[&later32]), |db| db.drop_collection("later"));
    let again = create(3, 10, "again", F32);
    let into_again = put(
        with(&full, &[&later32, &dropped, &again]),
        "again",
        INEXACT[0],
    );
    let reused = create(3, 9, "later", F32);
    let into_reused = refit(&into_again, |bytes| {
        bytes[5..7].copy_from_slice(&9u16.to_le_bytes())
    });
    let cases: [(&[&[u8]], &str); 6] = [
        (&[&later16, &exact], "ok"),
        (&[&later16, &inexact], "corrupt"),
        (&[&later16, &overflow], "corrupt"),
        (&[&later32, &inexact], "ok"),
        (&[&later16, &dropped, &again, &into_again], "ok"),
        (&[&later16, &dropped, &reused, &into_reused], "corrupt"),
    ];
    for (at, (frames, expected)) in cases.into_iter().enumerate() {
        let log = with(&half, frames);
        assert_eq!(readers(&log, None, floor), each(expected), "{at}");
        let sealed = encrypted(&log);
        assert_eq!(
            readers(&sealed, Some(&key()), floor),
            each(expected),
            "{at}"
        );
    }
}

/// The observable state of `half` that a refused write must not change.
fn observe(db: &Database) -> (u64, Vec<u8>, Option<RecordVectors>, Option<String>) {
    let half = db.collection("half").unwrap();
    let job = half.reembed_status("n").unwrap();
    (
        db.sequence,
        db.to_bytes().unwrap(),
        half.record_vectors(1).unwrap().cloned(),
        job.map(|job| format!("{job:?}")),
    )
}

/// Applies `vector` as record 1's re-embedding of `n`, checked against its
/// current revision or unchecked.
fn reembed(db: &mut Database, checked: bool, vector: [f32; 4]) -> Result<()> {
    let revision = db.collection("half")?.revision(1)?.unwrap();
    let mut half = db.collection_mut("half")?;
    match checked {
        true => half.apply_reembed_batch_checked(
            "n",
            &[ReembedUpdate {
                id: 1,
                revision,
                vector: vector.to_vec(),
            }],
        ),
        false => half.apply_reembed_batch("n", &[(1, vector.to_vec())]),
    }
}

/// Every dense write into a populated float16 `half` commits its input's
/// float16 value, which full replay, the change feed and log export then
/// verify; an input that rounds to float16 infinity is `NonFiniteVector`
/// on every path and commits nothing.
fn float16_writes_store_dense_values(key: Option<&EncryptionKey>) {
    let _gate = Activation::on();
    let [full, half] = populated(key, EXACT, EXACT);
    let floor = load(&full, key).unwrap().history_floor;
    let mut db = Database::open_from_bytes(half, key).unwrap();
    // Starting a re-embedding stores no dense value.
    db.collection_mut("half")
        .unwrap()
        .begin_reembed("n", "v2")
        .unwrap();
    let before = observe(&db);
    for path in 0..PATHS.len() {
        if path == 5 {
            // RejectChunk reports the row and commits nothing.
            let job = db.next_import_job_id().unwrap();
            let row = Mutation::Put {
                id: 2,
                vector: INEXACT[1].to_vec(),
                payload: b"p".to_vec(),
            };
            let mut half = db.collection_mut("half").unwrap();
            let report = half.import_chunk(job, 1, &[row], POLICY).unwrap();
            assert_eq!(report.outcome, ImportChunkOutcome::Rejected);
            assert_eq!(report.invalid[0].code, "non_finite_vector");
            continue;
        }
        assert_eq!(
            outcome(write(&mut db, path, &INEXACT[1])),
            "NonFiniteVector",
            "{path}"
        );
    }
    for checked in [true, false] {
        let refused = reembed(&mut db, checked, INEXACT[1]);
        assert_eq!(outcome(refused), "NonFiniteVector");
    }
    assert_eq!(observe(&db), before);

    let mut stored = exact_bits();
    stored.extend([INEXACT[0][0], 0.0].map(|value| F16.canonical(value).unwrap().to_bits()));
    for path in 0..PATHS.len() {
        for vector in [EXACT, INEXACT[0]] {
            write(&mut db, path, &vector).unwrap();
            let log = db.to_bytes().unwrap();
            assert_eq!(readers(&log, key, floor), each("ok"), "{path} {vector:?}");
            let bits = dense_bits(&load(&log, key).unwrap());
            assert!(bits.is_subset(&stored), "{path} {vector:?} {bits:x?}");
        }
    }
    for checked in [true, false] {
        reembed(&mut db, checked, INEXACT[0]).unwrap();
        reembed(&mut db, checked, EXACT).unwrap();
    }
    let log = db.to_bytes().unwrap();
    assert_eq!(readers(&log, key, floor), each("ok"));
    assert_eq!(dense_bits(&load(&log, key).unwrap()), stored);

    // Writes that add no dense value commit as before: sparse values
    // (float32 in every collection), declarations and history pruning.
    let head = db.sequence;
    let mut half = db.collection_mut("half").unwrap();
    let mut vectors = half.view().record_vectors(1).unwrap().unwrap().clone();
    vectors.sparse.insert("s".into(), vec![(3, 0.1)]);
    half.put_vectors(1, vectors).unwrap();
    let spec = VectorSpec {
        dimensions: 2,
        metric: DistanceMetric::Cosine,
        active_version: "m1".into(),
        history: 0,
    };
    half.configure_vector("m", spec).unwrap();
    half.prune_vector_history("n", 0).unwrap();
    assert_eq!(db.sequence, head + 3);
    let loaded = load(&db.to_bytes().unwrap(), key).unwrap();
    assert!(dense_bits(&loaded).is_subset(&stored));
    assert_eq!(
        loaded.collections[&1].vectors.records[&1].sparse["s"],
        [(3, 0.1)]
    );
}

#[test]
fn float16_collections_store_every_dense_write() {
    float16_writes_store_dense_values(None);
    float16_writes_store_dense_values(Some(&key()));
}
