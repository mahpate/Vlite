//! P3a commit-time canonicalization (`p3-precision-api.md`; item 2 of
//! "Remaining P3a work" in `p3-compact-vectors.md`). Every primary and
//! named dense value written to a float16 collection is rounded by the one
//! binary16 conversion before it commits, so the log, installed state,
//! immediate borrowed reads, exact scores and reopened stores hold the same
//! bits: the shared goldens' float16 values, scored by the unchanged
//! float32 metric oracle. A value that is non-finite or rounds to float16
//! infinity is `NonFiniteVector` exactly where a non-finite value is, and
//! commits nothing. Float32 collections, sparse values and query vectors
//! keep every bit, and import retries are identified by their raw rows.
//! Checking allocates nothing, so a refused float16 write allocates what
//! the float32 one does, and a stored value is rounded in place in the
//! copy float32 makes too; a batch makes one rounded copy of its rows only
//! after all are checked (the review of 7f0ab774, counted by `peak.rs`).

use super::peak::{Use, counting_on, held as size, measured};
use super::*;
use crate::import::ImportChunkReport;
use crate::vectors::{RecordVectors, ReembedUpdate, VectorSpec, VersionedVector};
use crate::{DistanceMetric, MAX_PAYLOAD_BYTES, MetadataValue, SearchHit, metric_distance};
use std::collections::BTreeMap;

/// The shared goldens (`tests/fixtures/compact/float16-cases.json`): the
/// 62 finite inputs with the float32 bits of their float16 values, and the
/// 18 inputs float16 refuses (6 rounding to infinity, 12 non-finite).
struct Goldens {
    inputs: Vec<f32>,
    canonical: Vec<u32>,
    refused: Vec<f32>,
}

fn goldens() -> Goldens {
    let document: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/compact/float16-cases.json"
    ))
    .unwrap();
    assert_eq!(document["version"], 1);
    let bits = |case: &serde_json::Value, key: &str| {
        u32::from_str_radix(case[key].as_str().unwrap(), 16).unwrap()
    };
    let mut goldens = Goldens {
        inputs: Vec::new(),
        canonical: Vec::new(),
        refused: Vec::new(),
    };
    for case in document["cases"].as_array().unwrap() {
        let input = f32::from_bits(bits(case, "input_f32_bits"));
        if case.get("error").is_some() {
            goldens.refused.push(input);
        } else {
            goldens.inputs.push(input);
            goldens.canonical.push(bits(case, "canonical_f32_bits"));
        }
    }
    assert_eq!((goldens.inputs.len(), goldens.refused.len()), (62, 18));
    assert_ne!(bits_of(&goldens.inputs), goldens.canonical);
    goldens
}

fn bits_of(vector: &[f32]) -> Vec<u32> {
    vector.iter().map(|value| value.to_bits()).collect()
}

fn floats(bits: &[u32]) -> Vec<f32> {
    bits.iter().map(|&bits| f32::from_bits(bits)).collect()
}

fn reversed<T: Clone>(values: &[T]) -> Vec<T> {
    values.iter().rev().cloned().collect()
}

/// The float16 value of `value`, by the accepted conversion.
fn half(value: f32) -> u32 {
    F16.canonical(value).unwrap().to_bits()
}

fn tagged(id: u64) -> Metadata {
    Metadata::from([("k".to_owned(), MetadataValue::Integer(id as i64))])
}

fn put(id: u64, vector: &[f32]) -> Mutation {
    Mutation::Put {
        id,
        vector: vector.to_vec(),
        payload: b"p".to_vec(),
    }
}

fn vector_op(collection: &str, id: u64, vector: &[f32]) -> GraphOp {
    GraphOp::Vector {
        collection: collection.into(),
        mutation: put(id, vector),
    }
}

/// Primary dense write paths: put, put with metadata, a conditional put,
/// put with expiry, an ordered batch without and with metadata, an import
/// chunk under each policy, and a graph batch entry.
const PRIMARY: u64 = 9;
const IMPORTS: [u64; 2] = [6, 7];

/// Writes record `id` of `name` with `vector` through primary path `path`:
/// the `Database` methods for `default`, `CollectionMut` otherwise.
fn primary(db: &mut Database, name: &str, path: u64, id: u64, vector: &[f32]) -> Result<()> {
    let (payload, metadata) = (b"p".as_slice(), tagged(id));
    if path == 8 {
        let entry = vector_op(name, id, vector);
        return db.graph_mut("g")?.write_batch(&[entry]).map(drop);
    }
    if IMPORTS.contains(&path) {
        let policy = match path {
            6 => POLICY,
            _ => InvalidRowPolicy::SkipInvalid,
        };
        let job = db.next_import_job_id()?;
        let rows = [put(id, vector)];
        let report = match name {
            "default" => db.import_chunk(job, 1, &rows, policy)?,
            _ => db
                .collection_mut(name)?
                .import_chunk(job, 1, &rows, policy)?,
        };
        assert_eq!(report.outcome, ImportChunkOutcome::Committed);
        assert_eq!((report.rows_committed, report.invalid_rows), (1, 0));
        return Ok(());
    }
    let with = Mutation::PutWithMetadata {
        id,
        vector: vector.to_vec(),
        payload: payload.to_vec(),
        metadata: metadata.clone(),
    };
    if name == "default" {
        return match path {
            0 => db.put(id, vector, payload),
            1 => db.put_with_metadata(id, vector, payload, &metadata),
            2 => db
                .put_with_metadata_if(id, vector, payload, &metadata, Expected::Absent)
                .map(drop),
            3 => db
                .put_with_expiry(id, vector, payload, &metadata, Some(FAR))
                .map(drop),
            4 => db.write_batch(&[put(id, vector)]),
            _ => db.write_batch(&[with]),
        };
    }
    let mut collection = db.collection_mut(name)?;
    match path {
        0 => collection.put(id, vector, payload).map(drop),
        1 => collection
            .put_with_metadata(id, vector, payload, &metadata)
            .map(drop),
        2 => collection
            .put_with_metadata_if(id, vector, payload, &metadata, Expected::Absent)
            .map(drop),
        3 => collection
            .put_with_expiry(id, vector, payload, &metadata, Some(FAR))
            .map(drop),
        4 => collection.write_batch(&[put(id, vector)]),
        _ => collection.write_batch(&[with]),
    }
}

/// Named dense write paths into `n`: a model version, a record's whole
/// named vectors (with a sparse value), and a re-embedding batch, unchecked
/// and checked.
const NAMED: u64 = 4;

fn named(db: &mut Database, name: &str, path: u64, id: u64, vector: &[f32]) -> Result<()> {
    let revision = db.collection(name)?.revision(id)?.unwrap_or(0);
    let mut collection = db.collection_mut(name)?;
    let vector = vector.to_vec();
    match path {
        0 => collection.put_vector_version(id, "n", "v1", vector),
        1 => collection.put_vectors(
            id,
            RecordVectors {
                dense: BTreeMap::from([(
                    "n".into(),
                    vec![VersionedVector {
                        version: "v1".into(),
                        vector,
                    }],
                )]),
                sparse: BTreeMap::from([("s".into(), vec![(3, 0.1)])]),
            },
        ),
        2 => collection.apply_reembed_batch("n", &[(id, vector)]),
        _ => collection.apply_reembed_batch_checked(
            "n",
            &[ReembedUpdate {
                id,
                revision,
                vector,
            }],
        ),
    }
}

/// A record's primary vector bits, payload, metadata and expiry.
type Row = (Vec<u32>, Vec<u8>, Metadata, Option<u64>);

/// Everything a collection holds, read through borrowed records: each
/// record's primary row; each named dense version's bits; and each sparse
/// value's index and bits.
#[derive(Clone, Debug, PartialEq)]
struct Held {
    primary: BTreeMap<u64, Row>,
    named: BTreeMap<(u64, String, String), Vec<u32>>,
    sparse: Vec<(u64, u32, u32)>,
}

fn held(db: &Database, name: &str) -> Held {
    let collection = db.collection(name).unwrap();
    let ids: Vec<u64> = db
        .state(collection.id)
        .records
        .resident()
        .unwrap()
        .keys()
        .copied()
        .collect();
    let mut held = Held {
        primary: BTreeMap::new(),
        named: BTreeMap::new(),
        sparse: Vec::new(),
    };
    for id in ids {
        let record = collection.get(id).unwrap().unwrap();
        let expiry = collection.expires_at(id).unwrap();
        let entry = (
            bits_of(&record.vector),
            record.payload.clone(),
            record.metadata.clone(),
            expiry,
        );
        held.primary.insert(id, entry);
        let Some(vectors) = collection.record_vectors(id).unwrap() else {
            continue;
        };
        for (n, versions) in &vectors.dense {
            for version in versions {
                let key = (id, n.clone(), version.version.clone());
                held.named.insert(key, bits_of(&version.vector));
            }
        }
        for values in vectors.sparse.values() {
            let values = values
                .iter()
                .map(|&(index, value)| (id, index, value.to_bits()));
            held.sparse.extend(values);
        }
    }
    held
}

/// The exact hits of `query` among `name`'s primary vectors and its named
/// `n` version `v2`, as IDs and distance bits.
type Scores = (Vec<(u64, u64)>, Vec<(u64, u64)>);

fn scores(db: &Database, name: &str, query: &[f32]) -> Scores {
    let collection = db.collection(name).unwrap();
    let hits = |hits: Vec<SearchHit>| {
        let hits = hits.into_iter();
        hits.map(|hit| (hit.id, hit.distance.to_bits())).collect()
    };
    (
        hits(collection.search(query, 64).unwrap()),
        hits(collection.search_named("n", Some("v2"), query, 64).unwrap()),
    )
}

/// The unchanged float32 oracle over `held`'s values: squared L2 in f64,
/// ties by ascending ID.
fn oracle(held: &Held, query: &[f32]) -> Scores {
    let rank = |rows: Vec<(u64, &Vec<u32>)>| {
        let mut hits: Vec<(f64, u64)> = rows
            .into_iter()
            .map(|(id, bits)| {
                let distance =
                    metric_distance(query, &floats(bits), DistanceMetric::SquaredL2, 0.0, 0.0);
                (distance.unwrap(), id)
            })
            .collect();
        hits.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        hits.into_iter()
            .map(|(distance, id)| (id, distance.to_bits()))
            .collect()
    };
    let primary = held.primary.iter().map(|(&id, row)| (id, &row.0)).collect();
    let named = held
        .named
        .iter()
        .filter(|((_, n, version), _)| n == "n" && version == "v2")
        .map(|((id, _, _), bits)| (*id, bits))
        .collect();
    (rank(primary), rank(named))
}

/// Every primary and named dense write path into `half` (float16) and
/// `full` (float32), with the goldens' raw inputs and their reverse, then
/// a settings replacement, a metadata update and a write after reopening:
/// `half` holds exactly the float16 values and `full` every input bit,
/// immediately, after reopening (and for directories a checkpoint), in the
/// stored log and in exact scores of an unrounded query. Payloads,
/// metadata, expiries and sparse values are the same in both.
fn canonical_writes(backing: Backing, key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let goldens = goldens();
    let raw = &goldens.inputs;
    let dimensions = raw.len();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let key = key.as_ref();
    let mut db = create_store(backing, &path, key);
    db.create_graph("g").unwrap();
    let named_spec = VectorSpec {
        dimensions,
        metric: DistanceMetric::SquaredL2,
        active_version: "v1".into(),
        history: 1,
    };
    let changed = CollectionSettings {
        rerank_multiplier: 7,
        ..CollectionSettings::default()
    };
    for (name, precision) in [("half", F16), ("full", F32)] {
        db.create_collection_as(name, spec(dimensions), precision)
            .unwrap();
        let mut collection = db.collection_mut(name).unwrap();
        collection
            .configure_vector("n", named_spec.clone())
            .unwrap();
        for path in 0..PRIMARY {
            primary(&mut db, name, path, path + 1, raw).unwrap();
        }
        named(&mut db, name, 0, 1, raw).unwrap();
        named(&mut db, name, 1, 2, raw).unwrap();
        let mut collection = db.collection_mut(name).unwrap();
        collection.begin_reembed("n", "v2").unwrap();
        named(&mut db, name, 2, 1, &reversed(raw)).unwrap();
        named(&mut db, name, 3, 2, &reversed(raw)).unwrap();
        // Ordered duplicates: the later value wins.
        let batch = [put(10, &reversed(raw)), put(10, raw)];
        db.collection_mut(name)
            .unwrap()
            .write_batch(&batch)
            .unwrap();
        // Settings replacement keeps the precision; a metadata update
        // keeps the stored vector.
        db.set_collection_settings(name, changed.clone()).unwrap();
        primary(&mut db, name, 0, 11, raw).unwrap();
        let mut collection = db.collection_mut(name).unwrap();
        collection.update_metadata(11, &tagged(12)).unwrap();
    }

    // `full` keeps every input bit; `half` holds the same with every dense
    // value rounded, and sparse values unchanged.
    let full = held(&db, "full");
    let (inputs, backwards) = (bits_of(raw), bits_of(&reversed(raw)));
    let round = |bits: &Vec<u32>| match *bits == inputs {
        true => goldens.canonical.clone(),
        false => {
            assert_eq!(*bits, backwards);
            reversed(&goldens.canonical)
        }
    };
    let mut expected = full.clone();
    expected
        .primary
        .values_mut()
        .for_each(|row| row.0 = round(&row.0));
    expected
        .named
        .values_mut()
        .for_each(|bits| *bits = round(bits));
    assert_eq!(full.primary.len(), 11);
    assert_eq!(full.named.len(), 4);
    assert_eq!(full.sparse, [(2, 3, 0.1f32.to_bits())]);
    assert_eq!(full.primary[&4].3, Some(FAR));
    assert_eq!(full.primary[&11].2, tagged(12));
    assert!(full.primary.values().all(|row| row.0 == inputs));
    assert_eq!(held(&db, "half"), expected);

    // Exact scores of the unrounded query use exactly the stored values.
    let query = reversed(raw);
    let before = [scores(&db, "half", &query), scores(&db, "full", &query)];
    assert_eq!(before, [oracle(&expected, &query), oracle(&full, &query)]);
    let rounded = floats(&reversed(&goldens.canonical));
    assert_ne!(before[0], oracle(&expected, &rounded));

    if let Backing::Directory = backing {
        assert!(db.checkpoint().unwrap().is_some());
    }
    let (loaded, mut db) = cycle(backing, &path, key, db);
    assert_eq!(held(&db, "half"), expected, "{backing:?}");
    assert_eq!(held(&db, "full"), full, "{backing:?}");
    assert_eq!(
        [scores(&db, "half", &query), scores(&db, "full", &query)],
        before
    );
    for (name, held) in [("half", &expected), ("full", &full)] {
        let stored = loaded.collections.values().find(|c| c.name == name);
        let records = &stored.unwrap().records;
        let stored: BTreeMap<u64, Vec<u32>> = records
            .iter()
            .map(|(&id, record)| (id, bits_of(&record.vector)))
            .collect();
        let rows = held.primary.iter().map(|(&id, row)| (id, row.0.clone()));
        assert_eq!(stored, rows.collect(), "{name}");
    }
    // The reopened store rounds too.
    primary(&mut db, "half", 0, 20, raw).unwrap();
    assert_eq!(held(&db, "half").primary[&20].0, goldens.canonical);
}

#[test]
fn memory_float16_writes_store_canonical_values() {
    canonical_writes(Backing::Memory, None);
    canonical_writes(Backing::Memory, Some(key()));
}

#[test]
fn file_float16_writes_store_canonical_values() {
    canonical_writes(Backing::File, None);
    canonical_writes(Backing::File, Some(key()));
}

#[test]
fn directory_float16_writes_store_canonical_values() {
    canonical_writes(Backing::Directory, None);
    canonical_writes(Backing::Directory, Some(key()));
}

/// The `Database` methods write `default` (collection 0, whose batches use
/// the legacy batch frame) as its float16 precision stores it.
fn default_collection(key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let goldens = goldens();
    let key = key.as_ref();
    let mut loaded = initial(key);
    loaded.collections.get_mut(&0).unwrap().precision = Some(F16);
    let mut db = Database::open_from_bytes(image_of(&loaded), key).unwrap();
    db.create_graph("g").unwrap();
    let pairs: Vec<&[f32]> = goldens.inputs.chunks(2).collect();
    for (path, pair) in (0..PRIMARY).zip(&pairs) {
        primary(&mut db, "default", path, path + 1, pair).unwrap();
    }
    // One batch of every golden pair.
    let batch: Vec<Mutation> = (100..)
        .zip(&pairs)
        .map(|(id, pair)| put(id, pair))
        .collect();
    db.write_batch(&batch).unwrap();
    let canonical: Vec<&[u32]> = goldens.canonical.chunks(2).collect();
    let mut want: BTreeMap<u64, Vec<u32>> = (100..)
        .zip(&canonical)
        .map(|(id, bits)| (id, bits.to_vec()))
        .collect();
    for (path, bits) in (0..PRIMARY).zip(&canonical) {
        want.insert(path + 1, bits.to_vec());
    }
    let observe = |db: &Database| {
        let ids = db
            .default_state()
            .records
            .resident()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        ids.into_iter()
            .map(|id| (id, bits_of(&db.get(id).unwrap().unwrap().vector)))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(observe(&db), want);
    let reopened = Database::open_from_bytes(db.to_bytes().unwrap(), key).unwrap();
    assert_eq!(observe(&reopened), want);
}

#[test]
fn default_float16_collections_store_canonical_values() {
    default_collection(None);
    default_collection(Some(key()));
}

/// A dense value that is non-finite or rounds to float16 infinity is
/// `NonFiniteVector` on every path, exactly where a non-finite value is
/// refused, and commits nothing: no record, revision, import marker or
/// graph change. `SkipInvalid` skips such a row like any invalid one.
fn refusals(key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let goldens = goldens();
    let key = key.as_ref();
    let mut db = Database::open_from_bytes(image_of(&initial(key)), key).unwrap();
    db.create_graph("g").unwrap();
    db.create_collection_as("half", spec(2), F16).unwrap();
    let mut half = db.collection_mut("half").unwrap();
    half.put(1, &[0.5, 0.5], b"p").unwrap();
    for (n, history) in [("n", 1), ("h0", 0)] {
        let spec = VectorSpec {
            dimensions: 2,
            metric: DistanceMetric::SquaredL2,
            active_version: "v1".into(),
            history,
        };
        half.configure_vector(n, spec).unwrap();
    }
    half.begin_reembed("n", "v2").unwrap();
    let observe = |db: &Database| {
        let job = db.next_import_job_id().unwrap();
        (db.sequence, db.to_bytes().unwrap(), job)
    };
    let before = observe(&db);
    let ok = [0.5, 0.5];
    for &value in &goldens.refused {
        let vector = [0.25, value];
        let refused = |result: Result<()>| {
            assert_eq!(outcome(result), "NonFiniteVector", "{value:?}");
        };
        for path in (0..PRIMARY).filter(|path| !IMPORTS.contains(path)) {
            refused(primary(&mut db, "half", path, 2, &vector));
        }
        for path in 0..NAMED {
            refused(named(&mut db, "half", path, 1, &vector));
        }
        let mut half = db.collection_mut("half").unwrap();
        // A version that pruning would discard at once.
        refused(half.put_vector_version(1, "h0", "old", vector.to_vec()));
        // Validation precedes the precondition.
        let none = Metadata::new();
        let conditional = half.put_with_metadata_if(1, &vector, b"", &none, Expected::Revision(9));
        refused(conditional.map(drop));
        // Batches are atomic: valid entries before it commit nothing.
        refused(half.write_batch(&[put(2, &ok), put(3, &vector)]));
        let node = GraphOp::CreateNode {
            key: "x".into(),
            labels: Vec::new(),
            properties: Metadata::new(),
        };
        let entries = [
            node,
            vector_op("half", 2, &ok),
            vector_op("half", 3, &vector),
        ];
        refused(db.graph_mut("g").unwrap().write_batch(&entries).map(drop));
        // RejectChunk reports the row and commits nothing.
        let job = db.next_import_job_id().unwrap();
        let rows = [put(2, &ok), put(3, &vector)];
        let mut half = db.collection_mut("half").unwrap();
        let report = half.import_chunk(job, 1, &rows, POLICY).unwrap();
        assert_eq!(report.outcome, ImportChunkOutcome::Rejected);
        assert_eq!(report.invalid_rows, 1);
        assert_eq!(
            (report.invalid[0].row, report.invalid[0].code),
            (1, "non_finite_vector")
        );
        assert_eq!(observe(&db), before, "{value:?}");
    }
    // As for NaN: a wrong width is reported first, the value before the
    // payload.
    let big = vec![0; MAX_PAYLOAD_BYTES + 1];
    for value in [f32::NAN, 65520.0] {
        let mut half = db.collection_mut("half").unwrap();
        assert_eq!(
            outcome(half.put(2, &[value], b"")),
            "DimensionMismatch { expected: 2, actual: 1 }"
        );
        assert_eq!(outcome(half.put(2, &[value, 0.5], &big)), "NonFiniteVector");
        let batch = Mutation::Put {
            id: 2,
            vector: vec![value, 0.5],
            payload: big.clone(),
        };
        assert_eq!(outcome(half.write_batch(&[batch])), "NonFiniteVector");
    }
    assert_eq!(observe(&db), before);

    // SkipInvalid skips the refused row and commits the rest, rounded.
    let job = db.next_import_job_id().unwrap();
    let rows = [put(2, &[0.1, 0.5]), put(3, &[65520.0, 0.5])];
    let mut half = db.collection_mut("half").unwrap();
    let skip = InvalidRowPolicy::SkipInvalid;
    let report = half.import_chunk(job, 1, &rows, skip).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert_eq!((report.rows_committed, report.rows_skipped), (1, 1));
    assert_eq!(
        (report.invalid[0].row, report.invalid[0].code),
        (1, "non_finite_vector")
    );
    let half = db.collection("half").unwrap();
    let stored = bits_of(&half.get(2).unwrap().unwrap().vector);
    assert_eq!(stored, [self::half(0.1), 0.5f32.to_bits()]);
    assert!(half.get(3).unwrap().is_none());
}

#[test]
fn float16_refusals_commit_nothing() {
    refusals(None);
    refusals(Some(key()));
}

fn import(
    db: &mut Database,
    job: u64,
    chunk: u64,
    rows: &[Mutation],
    policy: InvalidRowPolicy,
) -> Result<ImportChunkReport> {
    db.collection_mut("half")?
        .import_chunk(job, chunk, rows, policy)
}

/// Import retries are identified by their raw rows as submitted: the same
/// rows are `AlreadyCommitted`, while other rows with the same float16
/// values are a different chunk, also after reopening.
fn import_retries(backing: Backing, key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let key = key.as_ref();
    let mut db = create_store(backing, &path, key);
    db.create_collection_as("half", spec(2), F16).unwrap();
    // 1 + 2^-11 ties to 1.0, as 1 + 2^-11 - 2^-23 rounds to it.
    let (tie, below) = (f32::from_bits(0x3f80_1000), f32::from_bits(0x3f80_0fff));
    let first = [put(1, &[tie, 0.5])];
    let other = [put(1, &[below, 0.5])];
    let skipped = [put(2, &[tie, 0.5]), put(3, &[65520.0, 0.5])];
    let skipped_other = [put(2, &[below, 0.5]), put(3, &[65520.0, 0.5])];
    let skip = InvalidRowPolicy::SkipInvalid;
    let job = db.next_import_job_id().unwrap();
    let report = import(&mut db, job, 1, &first, POLICY).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    let skip_job = db.next_import_job_id().unwrap();
    let report = import(&mut db, skip_job, 1, &skipped, skip).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert_eq!((report.rows_committed, report.rows_skipped), (1, 1));
    let differs = "ImportConflict(\"chunk differs from the committed chunk with this number\")";
    let one = [0x3f80_0000, 0.5f32.to_bits()];
    for reopen in [false, true] {
        if reopen {
            db = cycle(backing, &path, key, db).1;
        }
        let head = db.sequence;
        let retried = import(&mut db, job, 1, &first, POLICY).unwrap();
        assert_eq!(retried.outcome, ImportChunkOutcome::AlreadyCommitted);
        assert_eq!(outcome(import(&mut db, job, 1, &other, POLICY)), differs);
        let retried = import(&mut db, skip_job, 1, &skipped, skip).unwrap();
        assert_eq!(retried.outcome, ImportChunkOutcome::AlreadyCommitted);
        assert_eq!((retried.rows_committed, retried.rows_skipped), (1, 1));
        let conflict = import(&mut db, skip_job, 1, &skipped_other, skip);
        assert_eq!(outcome(conflict), differs);
        assert_eq!(db.sequence, head, "{backing:?} {reopen}");
        let half = db.collection("half").unwrap();
        for id in [1, 2] {
            assert_eq!(bits_of(&half.get(id).unwrap().unwrap().vector), one);
        }
    }
    // The other raw rows are a new chunk.
    let report = import(&mut db, job, 2, &other, POLICY).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
}

#[test]
fn float16_import_retries_compare_raw_rows() {
    for backing in [Backing::Memory, Backing::File, Backing::Directory] {
        import_retries(backing, None);
        import_retries(backing, Some(key()));
    }
}

/// One graph batch commits each vector entry as its target collection
/// stores it, in order, atomically with its graph changes.
fn graph_batches(key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let key = key.as_ref();
    let mut db = Database::open_from_bytes(image_of(&initial(key)), key).unwrap();
    db.create_graph("g").unwrap();
    db.create_collection_as("half", spec(2), F16).unwrap();
    db.create_collection_as("full", spec(2), F32).unwrap();
    let (value, later) = ([0.1, f32::from_bits(0x3380_0001)], [0.3, -0.0]);
    let entries = [
        vector_op("half", 1, &value),
        vector_op("full", 1, &value),
        GraphOp::CreateNode {
            key: "x".into(),
            labels: Vec::new(),
            properties: Metadata::new(),
        },
        GraphOp::Attach {
            node: "x".into(),
            collection: "half".into(),
            record: 1,
        },
        vector_op("half", 2, &value),
        vector_op("half", 2, &later),
    ];
    db.graph_mut("g").unwrap().write_batch(&entries).unwrap();
    let observe = |db: &Database| {
        let vector = |name: &str, id: u64| {
            let collection = db.collection(name).unwrap();
            bits_of(&collection.get(id).unwrap().unwrap().vector)
        };
        let node = db
            .graph("g")
            .unwrap()
            .get_node("x")
            .unwrap()
            .output
            .unwrap();
        (
            [vector("half", 1), vector("full", 1), vector("half", 2)],
            node.revision,
        )
    };
    let immediate = observe(&db);
    let small = 0x3380_0000;
    assert_eq!(
        immediate.0,
        [
            vec![half(0.1), small],
            bits_of(&value),
            vec![half(0.3), (-0.0f32).to_bits()]
        ]
    );
    assert_eq!(immediate.1, db.sequence);
    let reopened = Database::open_from_bytes(db.to_bytes().unwrap(), key).unwrap();
    assert_eq!(observe(&reopened), immediate);
}

#[test]
fn float16_graph_batches_store_canonical_values() {
    graph_batches(None);
    graph_batches(Some(key()));
}

/// Twin memory stores, the same but for collection `c`'s precision,
/// float16 then float32: each holds record 1, attached to node `x` of
/// graph `g`, and named vector `n` re-embedding to `v2`.
fn twins() -> [Database; 2] {
    [F16, F32].map(|precision| {
        let mut db = Database::open_from_bytes(image_of(&initial(None)), None).unwrap();
        db.create_graph("g").unwrap();
        db.create_collection_as("c", spec(2), precision).unwrap();
        let mut c = db.collection_mut("c").unwrap();
        c.put(1, &[0.5, 0.5], b"p").unwrap();
        let named = VectorSpec {
            dimensions: 2,
            metric: DistanceMetric::SquaredL2,
            active_version: "v1".into(),
            history: 1,
        };
        c.configure_vector("n", named).unwrap();
        c.begin_reembed("n", "v2").unwrap();
        let attach = [
            GraphOp::CreateNode {
                key: "x".into(),
                labels: Vec::new(),
                properties: Metadata::new(),
            },
            GraphOp::Attach {
                node: "x".into(),
                collection: "c".into(),
                record: 1,
            },
        ];
        db.graph_mut("g").unwrap().write_batch(&attach).unwrap();
        db
    })
}

/// The value each twin refuses: one rounding to float16 infinity, and NaN.
const REFUSED: [f32; 2] = [65520.0, f32::NAN];

/// `write` on the float16 then the float32 twin (told which), measured:
/// the same outcome, returned, and the same retained bytes, with a float16
/// peak at most `extra` bytes above the float32 one.
fn twinned(
    twins: &mut [Database; 2],
    case: &str,
    extra: isize,
    write: impl Fn(&mut Database, usize) -> Result<()>,
) -> (String, [Use; 2]) {
    let [half, full] = twins;
    let (half_result, half_use) = measured(|| write(half, 0));
    let (full_result, full_use) = measured(|| write(full, 1));
    let report = format!("{case}: float16 {half_use:?}, float32 {full_use:?}, extra {extra}");
    let (half_outcome, full_outcome) = (outcome(half_result), outcome(full_result));
    assert_eq!(half_outcome, full_outcome, "{report}");
    assert_eq!(half_use.retained, full_use.retained, "{report}");
    assert!(half_use.peak <= full_use.peak + extra, "{report}");
    (half_outcome, [half_use, full_use])
}

/// A large valid row: two dimensions and a 1 MiB payload.
fn large(id: u64) -> Mutation {
    Mutation::Put {
        id,
        vector: vec![0.1, 0.3],
        payload: vec![7; MAX_PAYLOAD_BYTES],
    }
}

/// A refused float16 batch allocates nothing, as the float32 one, and a
/// refused import chunk what the float32 one does, however large the valid
/// rows before the refusal: every row and the batch limits are checked, in
/// order, before a rounded copy exists.
#[test]
fn float16_refused_batches_allocate_nothing() {
    counting_on(1, || {
        let mut twins = twins();
        let heads = twins.each_ref().map(|db| db.sequence);
        let prefix: Vec<Mutation> = (10..14).map(large).collect();
        let after =
            |last: Mutation| -> Vec<Mutation> { prefix.iter().cloned().chain([last]).collect() };
        let wide = Mutation::Put {
            id: 11,
            vector: vec![0.5; 8 << 20],
            payload: Vec::new(),
        };
        let oversized = Mutation::Put {
            id: 14,
            vector: vec![0.1, 0.3],
            payload: vec![7; MAX_PAYLOAD_BYTES + 1],
        };
        // The rows of both twins, or of each.
        let cases: [(&str, Vec<Vec<Mutation>>, &str); 5] = [
            // 32 valid rows of 1 MiB exceed the 16 MiB batch limit.
            (
                "aggregate",
                vec![(10..42).map(large).collect()],
                "BatchTooLarge",
            ),
            // A valid row, then a 32 MiB vector of the wrong width.
            (
                "width",
                vec![vec![put(10, &[0.1, 0.3]), wide]],
                "DimensionMismatch { expected: 2, actual: 8388608 }",
            ),
            (
                "payload",
                vec![after(oversized)],
                "PayloadTooLarge(1048577)",
            ),
            (
                "value",
                REFUSED.map(|value| after(put(14, &[0.1, value]))).into(),
                "NonFiniteVector",
            ),
            // Deleting attached record 1.
            (
                "attached",
                vec![after(Mutation::Delete { id: 1 })],
                "GraphAttachedRecord",
            ),
        ];
        for (case, rows, error) in &cases {
            let rows = move |twin: usize| &rows[twin % rows.len()];
            let (outcome, uses) = twinned(&mut twins, case, 0, |db, twin| {
                db.collection_mut("c")?.write_batch(rows(twin))
            });
            assert_eq!(outcome, *error, "{case}");
            assert_eq!(uses.map(|used| used.peak), [0, 0], "{case}: {uses:?}");
            let (outcome, _) = twinned(&mut twins, case, 0, |db, twin| {
                let job = db.next_import_job_id()?;
                let mut c = db.collection_mut("c")?;
                let report = c.import_chunk(job, 1, rows(twin), POLICY)?;
                assert_eq!(report.outcome, ImportChunkOutcome::Rejected);
                Ok(())
            });
            let rejected = ["aggregate", "attached"].contains(case);
            assert_eq!(outcome, if rejected { *error } else { "ok" }, "{case}");
        }
        // Nothing committed: no record, revision or import job.
        assert_eq!(twins.each_ref().map(|db| db.sequence), heads);
        for db in &twins {
            assert_eq!(db.collection("c").unwrap().len(), 1);
            assert_eq!(db.next_import_job_id().unwrap(), 1);
        }
    });
}

/// A refused float16 write of one vector, primary or named, allocates
/// what the float32 one does and keeps nothing.
#[test]
fn float16_refused_writes_allocate_as_float32() {
    counting_on(1, || {
        let mut twins = twins();
        for path in (0..PRIMARY).filter(|path| !IMPORTS.contains(path)) {
            let (outcome, _) = twinned(&mut twins, &format!("primary {path}"), 0, |db, twin| {
                primary(db, "c", path, 20, &[0.1, REFUSED[twin]])
            });
            assert_eq!(outcome, "NonFiniteVector", "primary {path}");
        }
        for path in 0..NAMED {
            let (outcome, _) = twinned(&mut twins, &format!("named {path}"), 0, |db, twin| {
                named(db, "c", path, 1, &[0.1, REFUSED[twin]])
            });
            assert_eq!(outcome, "NonFiniteVector", "named {path}");
        }
    });
}

/// A float16 write allocates and keeps what the float32 one does: a value
/// is rounded in place in the copy float32 makes too. A batch or import
/// chunk also makes one rounded copy of its rows, after all are checked:
/// for the largest valid batch of 1 MiB rows, 15 MiB and a few bytes.
#[test]
fn float16_writes_allocate_as_float32() {
    counting_on(1, || {
        let mut twins = twins();
        let ok = [0.1, 0.3];
        for path in 0..PRIMARY {
            let id = 20 + path;
            let row = match path {
                5 => Mutation::PutWithMetadata {
                    id,
                    vector: ok.to_vec(),
                    payload: b"p".to_vec(),
                    metadata: tagged(id),
                },
                _ => put(id, &ok),
            };
            let copy = match path {
                4..=7 => size(vec![row]),
                _ => 0,
            };
            let case = format!("primary {path}");
            let (outcome, _) = twinned(&mut twins, &case, copy, |db, _| {
                primary(db, "c", path, id, &ok)
            });
            assert_eq!(outcome, "ok", "{case}");
        }
        for path in 0..NAMED {
            let case = format!("named {path}");
            let (outcome, _) = twinned(&mut twins, &case, 0, |db, _| named(db, "c", path, 1, &ok));
            assert_eq!(outcome, "ok", "{case}");
        }
        let batch: Vec<Mutation> = (40..55).map(large).collect();
        let copy = size(batch.clone());
        assert!(copy < 15 * (MAX_PAYLOAD_BYTES as isize + 128), "{copy}");
        let (outcome, _) = twinned(&mut twins, "batch", copy, |db, _| {
            db.collection_mut("c")?.write_batch(&batch)
        });
        assert_eq!(outcome, "ok");
        let chunk: Vec<Mutation> = (60..62).map(large).collect();
        let (outcome, _) = twinned(&mut twins, "import", size(chunk.clone()), |db, _| {
            let job = db.next_import_job_id()?;
            let report = db
                .collection_mut("c")?
                .import_chunk(job, 1, &chunk, POLICY)?;
            assert_eq!(report.outcome, ImportChunkOutcome::Committed);
            Ok(())
        });
        assert_eq!(outcome, "ok");
        for id in [20, 40, 61] {
            let stored = |db: &Database| {
                let record = db.collection("c").unwrap().get(id).unwrap().unwrap();
                bits_of(&record.vector)
            };
            assert_eq!(stored(&twins[0]), [half(0.1), half(0.3)], "{id}");
            assert_eq!(stored(&twins[1]), bits_of(&ok), "{id}");
        }
    });
}
