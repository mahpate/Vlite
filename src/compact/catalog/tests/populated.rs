//! Float16 dense values in populated precision stores (review 9ebcbeff,
//! HIGH). Until commit-time canonicalization, a float16 collection holds
//! only values that a complete image or ordinary frames already store
//! exactly in float16: replay verifies every primary and named dense value
//! bit for bit (never rounding it) and refuses any other as corruption,
//! and every write that would add a dense value is refused before it
//! commits. Float32 collections keep every finite value unchanged.

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

fn replay_verifies_float16_values(key: Option<&EncryptionKey>) {
    let _gate = Activation::on();
    let [full, half] = populated(key, EXACT, EXACT);
    let loaded = load(&half, key).unwrap();
    assert_eq!(loaded.collections[&1].precision, Some(F16));
    assert_eq!(dense_bits(&loaded), exact_bits());
    if key.is_none() {
        // The image restates record 1 in a record block and its named
        // vectors in an op-10 frame.
        let (_, frames) = split(&half);
        let ops: Vec<u8> = frames.iter().map(|frame| frame[4]).collect();
        assert!(ops.contains(&14) && ops.contains(&10), "{ops:?}");
    }
    // An image that restates any other value is corrupt, as float16 and
    // only as float16.
    for vector in INEXACT {
        for [full, half] in [populated(key, vector, EXACT), populated(key, EXACT, vector)] {
            assert_eq!(outcome(load(&full, key)), "ok");
            assert_eq!(outcome(load(&half, key)), "corrupt", "{vector:?}");
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
            assert_eq!(outcome(load(&log, key)), "ok");
            let mut log = half.clone();
            log.extend_from_slice(suffix);
            let replayed = load(&log, key);
            assert_eq!(outcome(replayed), expected, "{path} {vector:?}");
            if expected == "ok" {
                let loaded = load(&log, key).unwrap();
                assert_eq!(dense_bits(&loaded), exact_bits(), "{path}");
            }
        }
    }
}

#[test]
fn replay_verifies_float16_values_bit_for_bit() {
    replay_verifies_float16_values(None);
    replay_verifies_float16_values(Some(&key()));
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

fn float16_writes_refuse_dense_values(key: Option<&EncryptionKey>) {
    let _gate = Activation::on();
    let [_, half] = populated(key, EXACT, EXACT);
    let mut db = Database::open_from_bytes(half, key).unwrap();
    // Starting a re-embedding stores no dense value.
    db.collection_mut("half")
        .unwrap()
        .begin_reembed("n", "v2")
        .unwrap();
    let before = observe(&db);
    for path in 0..PATHS.len() {
        for vector in [EXACT, INEXACT[0]] {
            assert_eq!(
                outcome(write(&mut db, path, &vector)),
                "unsupported 32",
                "{path} {vector:?}"
            );
        }
    }
    let revision = db
        .collection("half")
        .unwrap()
        .reembed_pending_checked("n", 1)
        .unwrap()[0]
        .revision;
    let mut half = db.collection_mut("half").unwrap();
    assert_eq!(
        outcome(half.apply_reembed_batch("n", &[(1, EXACT.to_vec())])),
        "unsupported 32"
    );
    let update = ReembedUpdate {
        id: 1,
        revision,
        vector: EXACT.to_vec(),
    };
    assert_eq!(
        outcome(half.apply_reembed_batch_checked("n", &[update])),
        "unsupported 32"
    );
    assert_eq!(observe(&db), before);

    // Writes that add no dense value still commit: sparse values (float32
    // in every collection), declarations and history pruning.
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
    assert_eq!(db.sequence, before.0 + 3);
    let loaded = load(&db.to_bytes().unwrap(), key).unwrap();
    assert_eq!(dense_bits(&loaded), exact_bits());
    assert_eq!(
        loaded.collections[&1].vectors.records[&1].sparse["s"],
        [(3, 0.1)]
    );
}

#[test]
fn float16_collections_refuse_every_dense_write() {
    float16_writes_refuse_dense_values(None);
    float16_writes_refuse_dense_values(Some(&key()));
}
