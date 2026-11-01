//! P5 step 2: derived block summaries prune the existing exact filtered
//! search without changing its answers.
//!
//! Every query is compared with an independent brute force over a model
//! (IDs, distance bits and order) and, where a twin exists, with the same
//! data in a collection without declared indexes, which takes today's
//! unindexed scan. Queries that only a summary can narrow must scan at most
//! half the records. Covers typed bounds, signed zero, numeric extremes,
//! UTF-8, booleans, geo radius as unknown, absence, delete/reinsert,
//! metadata and TTL edits, expiry at the query clock, merge, declaration
//! changes, reopen, replay, directory checkpoints, conversion, compaction,
//! encryption and in-memory stores.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use vlite::{
    AnnOptions, CollectionSettings, Condition, CreateOptions, Database, DistanceMetric,
    EncryptionKey, HnswConfig, Metadata, MetadataIndexKind, MetadataIndexSpec, MetadataValue,
    Mutation, PlanStrategy, SearchHit, maintenance,
};

const ROWS: u64 = 5000;
/// A past absolute expiry: invisible at the wall clock, visible before it.
const T: u64 = 50_000;
const QUERIES: [[f32; 2]; 2] = [[3.0, 1.5], [12.0, 0.0]];
const METRICS: [DistanceMetric; 3] = [
    DistanceMetric::SquaredL2,
    DistanceMetric::Cosine,
    DistanceMetric::InnerProduct,
];

/// Live records: vector, metadata and absolute expiry.
type Model = BTreeMap<u64, (Vec<f32>, Metadata, Option<u64>)>;

fn int(value: i64) -> MetadataValue {
    MetadataValue::Integer(value)
}

fn float(value: f64) -> MetadataValue {
    MetadataValue::Float(value)
}

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

fn boolean(value: bool) -> MetadataValue {
    MetadataValue::Boolean(value)
}

fn eq(field: &str, value: MetadataValue) -> Condition {
    Condition::Eq {
        field: field.into(),
        value,
    }
}

fn range(field: &str, min: Option<MetadataValue>, max: Option<MetadataValue>) -> Condition {
    Condition::Range {
        field: field.into(),
        min,
        max,
    }
}

/// Covers the whole globe: half the circumference is about 2.0015e7 m.
fn everywhere() -> Condition {
    Condition::GeoRadius {
        field: "geo".into(),
        latitude: 0.0,
        longitude: 0.0,
        radius_meters: 2.1e7,
    }
}

/// Few distinct vectors, so distance ties are common and ordered by ID.
fn vector(id: u64) -> Vec<f32> {
    vec![(id % 13) as f32, (id % 7) as f32 * 0.5]
}

/// Values correlated with the ID, so 1024-row blocks have disjoint ranges.
fn row(id: u64) -> Metadata {
    let mut metadata = Metadata::new();
    let ts = match id {
        0 => i64::MIN,
        id if id == ROWS - 1 => i64::MAX,
        id => id as i64 - 2500,
    };
    metadata.insert("ts".into(), int(ts));
    let temp = if id == 2500 {
        -0.0
    } else {
        (id as f64 - 2500.0) / 4.0
    };
    metadata.insert("temp".into(), float(temp));
    metadata.insert("tag".into(), text(&format!("région-{}", id / 700)));
    metadata.insert("flag".into(), boolean(id < 1200));
    let mix = match id % 4 {
        0 => Some(int((id / 1000) as i64)),
        1 => Some(float((id / 1000) as f64)),
        2 => Some(text(&format!("m{}", id / 1000))),
        _ => None,
    };
    if let Some(mix) = mix {
        metadata.insert("mix".into(), mix);
    }
    metadata.insert(
        "geo".into(),
        MetadataValue::GeoPoint {
            latitude: (id % 90) as f64,
            longitude: (id % 180) as f64 - 90.0,
        },
    );
    metadata.insert("free".into(), int((id % 5) as i64));
    metadata
}

fn with(id: u64, field: &str, value: Option<MetadataValue>) -> Metadata {
    let mut metadata = row(id);
    match value {
        Some(value) => metadata.insert(field.into(), value),
        None => metadata.remove(field),
    };
    metadata
}

fn spec(field: &str, kind: MetadataIndexKind) -> MetadataIndexSpec {
    MetadataIndexSpec {
        field: field.into(),
        kind,
    }
}

/// `ts` and `temp` have equality postings only and `tag`, `flag` and `mix`
/// ordered numeric postings only, so ranges on the former and string or
/// boolean equality on the latter have no usable postings.
fn declared() -> CollectionSettings {
    let mut settings = CollectionSettings::default();
    settings.metadata_indexes = vec![
        spec("ts", MetadataIndexKind::Eq),
        spec("temp", MetadataIndexKind::Eq),
        spec("tag", MetadataIndexKind::Range),
        spec("flag", MetadataIndexKind::Range),
        spec("mix", MetadataIndexKind::Range),
        spec("geo", MetadataIndexKind::Eq),
    ];
    settings
}

/// Queries that only a block summary can narrow.
fn summary_only() -> Vec<Vec<Condition>> {
    vec![
        vec![range("ts", Some(int(-2400)), Some(int(-2350)))],
        vec![range("ts", None, Some(int(i64::MIN)))],
        vec![range("ts", Some(int(i64::MAX)), None)],
        vec![range("ts", Some(float(0.0)), Some(float(10.0)))],
        vec![range("ts", Some(int(2390)), Some(int(2410)))],
        vec![range("ts", Some(int(-2496)), Some(int(-2494)))],
        vec![range("temp", Some(float(-0.0)), Some(float(0.0)))],
        vec![range("temp", Some(int(0)), None)],
        vec![eq("tag", text("région-3"))],
        vec![eq("tag", text("région-6"))],
        vec![eq("tag", text("région-99"))],
        vec![eq("flag", boolean(true))],
        vec![eq("mix", text("m4"))],
        vec![eq("mix", boolean(true))],
        vec![
            range("ts", Some(int(-10)), Some(int(10))),
            eq("free", int(1)),
        ],
        vec![range("ts", Some(int(0)), Some(int(5))), everywhere()],
    ]
}

/// Summary-only queries that keep three of the five blocks, more than the
/// cap of half the records, once summaries are derived again over rows that
/// `mutations()` moved: ID 10 widens the first block's `ts` to 2400, ID 4000
/// widens the fourth block's to -2495, and IDs 6000 (`ts` i64::MIN, `flag`
/// true) and 100 000 (`ts` 0) widen the last block. Min/max summaries keep
/// them, so they scan everything there and stay exact.
fn wide_after_moves() -> Vec<Vec<Condition>> {
    vec![
        vec![range("ts", Some(int(-2400)), Some(int(-2350)))],
        vec![range("ts", Some(int(-2496)), Some(int(-2494)))],
        vec![eq("flag", boolean(true))],
        vec![
            range("ts", Some(int(-10)), Some(int(10))),
            eq("free", int(1)),
        ],
        vec![range("ts", Some(int(0)), Some(int(5))), everywhere()],
    ]
}

/// Queries a summary must not change: postings, the empty filter, fields
/// without coverage, geo radius and candidates over the cap.
fn unchanged() -> Vec<Vec<Condition>> {
    vec![
        vec![],
        vec![eq("temp", float(0.0))],
        vec![eq("temp", float(-0.0))],
        vec![eq("mix", int(2))],
        vec![range("mix", Some(float(-0.0)), Some(float(0.5)))],
        vec![eq("free", int(1))],
        vec![eq("absent", int(1))],
        vec![range("absent", Some(float(0.0)), None)],
        vec![everywhere()],
        vec![eq("flag", boolean(false))],
        vec![range("ts", Some(int(i64::MIN)), Some(int(i64::MAX)))],
        vec![eq(
            "geo",
            MetadataValue::GeoPoint {
                latitude: 1.0,
                longitude: -89.0,
            },
        )],
    ]
}

fn same(stored: &MetadataValue, wanted: &MetadataValue) -> bool {
    use MetadataValue::{Boolean, Float, GeoPoint, Integer, String};
    match (stored, wanted) {
        (Integer(a), Integer(b)) => a == b,
        (Float(a), Float(b)) => a == b,
        (String(a), String(b)) => a == b,
        (Boolean(a), Boolean(b)) => a == b,
        (
            GeoPoint {
                latitude: a,
                longitude: b,
            },
            GeoPoint {
                latitude: c,
                longitude: d,
            },
        ) => a == c && b == d,
        _ => false,
    }
}

fn numeric(stored: &MetadataValue, bound: &MetadataValue) -> Option<Ordering> {
    match (stored, bound) {
        (MetadataValue::Integer(a), MetadataValue::Integer(b)) => Some(a.cmp(b)),
        (MetadataValue::Float(a), MetadataValue::Float(b)) => a.partial_cmp(b),
        _ => None,
    }
}

/// Independent predicate semantics: typed equality, inclusive same-type
/// numeric ranges, and only globe-covering radii.
fn matches(metadata: &Metadata, conditions: &[Condition]) -> bool {
    conditions.iter().all(|condition| match condition {
        Condition::Eq { field, value } => metadata.get(field).is_some_and(|v| same(v, value)),
        Condition::Range { field, min, max } => metadata.get(field).is_some_and(|value| {
            min.as_ref()
                .is_none_or(|min| numeric(value, min).is_some_and(Ordering::is_ge))
                && max
                    .as_ref()
                    .is_none_or(|max| numeric(value, max).is_some_and(Ordering::is_le))
        }),
        Condition::GeoRadius {
            field,
            radius_meters,
            ..
        } => {
            assert!(*radius_meters >= 2.1e7);
            matches!(metadata.get(field), Some(MetadataValue::GeoPoint { .. }))
        }
    })
}

/// Squared-L2 top-k by (distance, ID) over visible matching records.
fn brute(
    model: &Model,
    query: &[f32],
    k: usize,
    conditions: &[Condition],
    now: u64,
) -> Vec<(u64, u64)> {
    let mut hits: Vec<(f64, u64)> = model
        .iter()
        .filter(|(_, (_, metadata, expiry))| {
            expiry.is_none_or(|expiry| expiry > now) && matches(metadata, conditions)
        })
        .map(|(&id, (vector, _, _))| {
            let distance = vector
                .iter()
                .zip(query)
                .map(|(&a, &b)| {
                    let difference = f64::from(a) - f64::from(b);
                    difference * difference
                })
                .sum::<f64>();
            (distance, id)
        })
        .collect();
    hits.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    hits.truncate(k);
    hits.into_iter()
        .map(|(distance, id)| (id, distance.to_bits()))
        .collect()
}

fn bits(hits: &[SearchHit]) -> Vec<(u64, u64)> {
    hits.iter()
        .map(|hit| (hit.id, hit.distance.to_bits()))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Checks every query at the wall clock against the model and, when given,
/// the undeclared twin. Summary-only queries scan at most half the records,
/// except `wide_after_moves()` when the summaries may have been derived
/// over moved values (`moved`) rather than before the moves.
fn check(db: &Database, twin: Option<&Database>, model: &Model, moved: bool) {
    let now = now_ms();
    let records = model.len() as u64;
    let wide = if moved { wide_after_moves() } else { vec![] };
    let cases: Vec<(Vec<Condition>, bool)> = summary_only()
        .into_iter()
        .map(|conditions| {
            let narrow = !wide.contains(&conditions);
            (conditions, narrow)
        })
        .chain(
            unchanged()
                .into_iter()
                .map(|conditions| (conditions, false)),
        )
        .collect();
    for (conditions, narrow) in &cases {
        for query in QUERIES {
            for k in [0, 7, 10_000] {
                for metric in METRICS {
                    let context = format!("{conditions:?} k={k} {metric:?} {query:?}");
                    let (hits, stats) = db
                        .collection("default")
                        .unwrap()
                        .search_with_stats(&query, k, conditions, metric)
                        .unwrap();
                    if let Some(twin) = twin {
                        let (expected, plain) = twin
                            .collection("default")
                            .unwrap()
                            .search_with_stats(&query, k, conditions, metric)
                            .unwrap();
                        assert_eq!(bits(&hits), bits(&expected), "{context}");
                        if k > 0 {
                            assert_eq!(plain.records_scanned, records, "{context}");
                        }
                    }
                    if metric == DistanceMetric::SquaredL2 {
                        let expected = brute(model, &query, k, conditions, now);
                        assert_eq!(bits(&hits), expected, "{context}");
                    }
                    if *narrow && k > 0 {
                        assert_eq!(stats.index_candidates, 0, "{context}");
                        assert!(
                            stats.records_scanned * 2 <= records,
                            "{context}: scanned {} of {records}",
                            stats.records_scanned
                        );
                    }
                }
            }
        }
    }
}

/// Checks every query at explicit clocks against the twin and the model.
fn check_at(db: &Database, twin: &Database, model: &Model, clocks: &[u64]) {
    for conditions in summary_only().into_iter().chain(unchanged()) {
        for &clock in clocks {
            for k in [1, 10_000] {
                for metric in METRICS {
                    let query = QUERIES[0];
                    let context = format!("{conditions:?} k={k} {metric:?} clock={clock}");
                    let hits = db
                        .search_filtered_with_metric_at(&query, k, &conditions, metric, clock)
                        .unwrap();
                    let expected = twin
                        .search_filtered_with_metric_at(&query, k, &conditions, metric, clock)
                        .unwrap();
                    assert_eq!(bits(&hits), bits(&expected), "{context}");
                    if metric == DistanceMetric::SquaredL2 {
                        let expected = brute(model, &query, k, &conditions, clock);
                        assert_eq!(bits(&hits), expected, "{context}");
                    }
                }
            }
        }
    }
}

enum Op {
    Put(u64, Metadata, Option<u64>),
    Meta(u64, Metadata),
    Delete(u64),
}

fn apply_model(model: &mut Model, op: &Op) {
    match op {
        Op::Put(id, metadata, expiry) => {
            model.insert(*id, (vector(*id), metadata.clone(), *expiry));
        }
        Op::Meta(id, metadata) => model.get_mut(id).unwrap().1 = metadata.clone(),
        Op::Delete(id) => {
            model.remove(id);
        }
    }
}

fn apply(db: &mut Database, op: &Op) {
    match op {
        Op::Put(id, metadata, expiry) => {
            db.put_with_expiry(*id, &vector(*id), b"", metadata, *expiry)
                .unwrap();
        }
        Op::Meta(id, metadata) => {
            db.update_metadata(*id, metadata).unwrap();
        }
        Op::Delete(id) => assert!(db.delete(*id).unwrap()),
    }
}

fn base() -> Model {
    (0..ROWS)
        .map(|id| (id, (vector(id), row(id), None)))
        .collect()
}

fn load(db: &mut Database) {
    let operations: Vec<Mutation> = (0..ROWS)
        .map(|id| Mutation::PutWithMetadata {
            id,
            vector: vector(id),
            payload: vec![],
            metadata: row(id),
        })
        .collect();
    for chunk in operations.chunks(2048) {
        db.write_batch(chunk).unwrap();
    }
}

/// Moves values across blocks, deletes and reinserts, edits TTLs and types,
/// and adds rows outside the original ID range.
fn mutations() -> Vec<Op> {
    vec![
        // A metadata edit into another block's range: block 0 no longer
        // describes ID 10, which the `ts` 2390..=2410 query must still find.
        Op::Meta(10, with(10, "ts", Some(int(2400)))),
        Op::Delete(4000),
        Op::Put(4000, with(4000, "ts", Some(int(-2495))), None),
        Op::Delete(4001),
        Op::Put(20, row(20), Some(T)),
        Op::Put(30, with(30, "ts", Some(int(1000))), Some(T + 100)),
        Op::Put(100_000, with(2500, "ts", Some(int(0))), None),
        Op::Put(6000, with(1, "ts", Some(int(i64::MIN))), Some(T)),
        Op::Put(50, with(50, "ts", None), None),
        Op::Meta(60, with(60, "ts", Some(float(1.0)))),
        Op::Meta(70, with(70, "tag", Some(text("région-6")))),
        Op::Put(80, row(80), Some(T)),
        Op::Put(80, row(80), None),
        Op::Delete(90),
        Op::Put(90, row(90), None),
        Op::Put(
            4100,
            with(4100, "flag", Some(boolean(true))),
            Some(u64::MAX),
        ),
        Op::Meta(2500, with(2500, "temp", Some(float(0.0)))),
    ]
}

fn create(path: &Path, key: Option<&EncryptionKey>) -> Database {
    Database::create_with_options(
        path,
        2,
        CreateOptions {
            encryption: key.cloned(),
            ..CreateOptions::default()
        },
    )
    .unwrap()
}

fn declare(db: &mut Database) {
    db.set_collection_settings("default", declared()).unwrap();
}

#[test]
fn pruned_exact_filters_equal_unindexed_scans_and_brute_force() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = create(&directory.path().join("indexed.vlite"), None);
    let mut twin = create(&directory.path().join("plain.vlite"), None);
    declare(&mut db);
    load(&mut db);
    load(&mut twin);
    check(&db, Some(&twin), &base(), false);
}

#[test]
fn visibility_edits_and_query_clocks_never_lose_a_row() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = create(&directory.path().join("indexed.vlite"), None);
    let mut twin = create(&directory.path().join("plain.vlite"), None);
    declare(&mut db);
    load(&mut db);
    load(&mut twin);
    let mut model = base();
    // Derive the summaries first, so every edit lands on a live generation.
    check(&db, Some(&twin), &model, false);
    for op in mutations() {
        apply(&mut db, &op);
        apply(&mut twin, &op);
        apply_model(&mut model, &op);
    }
    check(&db, Some(&twin), &model, false);
    check_at(&db, &twin, &model, &[0, T - 1, T, T + 1, T + 100, u64::MAX]);

    // Rewrite every original row (a merge): `ts` now descends with the ID.
    let reversed = |id: u64| with(id, "ts", Some(int(2500 - id as i64)));
    let operations: Vec<Mutation> = (0..ROWS)
        .map(|id| Mutation::PutWithMetadata {
            id,
            vector: vector(id),
            payload: vec![],
            metadata: reversed(id),
        })
        .collect();
    for chunk in operations.chunks(2048) {
        db.write_batch(chunk).unwrap();
        twin.write_batch(chunk).unwrap();
    }
    for id in 0..ROWS {
        model.insert(id, (vector(id), reversed(id), None));
    }
    check(&db, Some(&twin), &model, true);
    check_at(&db, &twin, &model, &[0, T, u64::MAX]);
}

#[test]
fn declaration_changes_rebuild_or_drop_summaries() {
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    load(&mut db);
    let model = base();
    let narrow = [range("ts", Some(int(-2400)), Some(int(-2350)))];
    let scanned = |db: &Database| {
        let (hits, stats) = db
            .collection("default")
            .unwrap()
            .search_with_stats(&QUERIES[0], 10_000, &narrow, DistanceMetric::SquaredL2)
            .unwrap();
        assert_eq!(
            bits(&hits),
            brute(&model, &QUERIES[0], 10_000, &narrow, now_ms())
        );
        stats.records_scanned
    };
    // No declared index: today's full scan.
    assert_eq!(scanned(&db), ROWS);
    declare(&mut db);
    assert!(scanned(&db) * 2 <= ROWS);
    db.set_collection_settings("default", CollectionSettings::default())
        .unwrap();
    assert_eq!(scanned(&db), ROWS);
    // A declaration that does not cover the filtered field never prunes.
    let mut other = CollectionSettings::default();
    other.metadata_indexes = vec![spec("temp", MetadataIndexKind::Eq)];
    db.set_collection_settings("default", other).unwrap();
    assert_eq!(scanned(&db), ROWS);
    declare(&mut db);
    assert!(scanned(&db) * 2 <= ROWS);
}

#[test]
fn layouts_reopen_replay_checkpoint_convert_compact_and_encrypt() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let key = EncryptionKey::from_bytes([0x5e; 32]);
    let ops = mutations();
    let mut mutated = base();
    for op in &ops {
        apply_model(&mut mutated, op);
    }
    let mutate = |db: &mut Database| {
        for op in &ops {
            apply(db, op);
        }
    };

    // Single file, plain and encrypted: derive, mutate, reopen (replay),
    // then convert to a directory store.
    for (name, key) in [("plain.vlite", None), ("sealed.vlite", Some(&key))] {
        let path = root.join(name);
        let mut db = create(&path, key);
        declare(&mut db);
        load(&mut db);
        check(&db, None, &base(), false);
        mutate(&mut db);
        check(&db, None, &mutated, false);
        db.close().unwrap();
        let db = match key {
            Some(key) => Database::open_encrypted(&path, key).unwrap(),
            None => Database::open(&path).unwrap(),
        };
        check(&db, None, &mutated, true);
        db.close().unwrap();

        let converted = root.join(format!("{name}.dir"));
        let db = match key {
            Some(key) => {
                maintenance::convert_to_directory_with_key(&path, &converted, key).unwrap();
                Database::open_encrypted(&converted, key).unwrap()
            }
            None => {
                maintenance::convert_to_directory(&path, &converted).unwrap();
                Database::open(&converted).unwrap()
            }
        };
        check(&db, None, &mutated, true);
        db.close().unwrap();
    }

    // Compaction drops expired records; the rest prune as before.
    let compacted = root.join("compacted.vlite");
    maintenance::compact(root.join("plain.vlite"), &compacted).unwrap();
    let now = now_ms();
    let mut live = mutated.clone();
    live.retain(|_, (_, _, expiry)| expiry.is_none_or(|expiry| expiry > now));
    let db = Database::open(&compacted).unwrap();
    check(&db, None, &live, true);
    db.close().unwrap();

    // Directory stores, plain and encrypted: a checkpoint image, then log
    // segments replayed over it on reopen.
    for (name, key) in [("store", None), ("sealed-store", Some(&key))] {
        let path = root.join(name);
        let mut db = Database::create_directory(
            &path,
            2,
            CreateOptions {
                encryption: key.cloned(),
                ..CreateOptions::default()
            },
        )
        .unwrap();
        db.set_checkpoint_threshold(None);
        declare(&mut db);
        load(&mut db);
        assert!(db.checkpoint().unwrap().is_some());
        check(&db, None, &base(), false);
        mutate(&mut db);
        check(&db, None, &mutated, false);
        db.close().unwrap();
        let db = match key {
            Some(key) => Database::open_encrypted(&path, key).unwrap(),
            None => Database::open(&path).unwrap(),
        };
        check(&db, None, &mutated, true);
        db.close().unwrap();
    }

    // In memory, mutated before the first query.
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    declare(&mut db);
    load(&mut db);
    mutate(&mut db);
    check(&db, None, &mutated, true);
}

#[test]
fn auto_exact_scan_prunes_under_the_same_plan() {
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    let mut twin = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    declare(&mut db);
    load(&mut db);
    load(&mut twin);
    let model = base();
    let config = HnswConfig {
        m: 8,
        ef_construction: 32,
        seed: 1,
    };
    let auto = |db: &Database, conditions: &[Condition]| {
        db.search_auto(
            &QUERIES[0],
            7,
            conditions,
            DistanceMetric::SquaredL2,
            config,
            AnnOptions::default(),
            64,
        )
        .unwrap()
    };
    for conditions in summary_only() {
        let planned = auto(&db, &conditions);
        let expected = auto(&twin, &conditions);
        assert_eq!(planned.plan.strategy, PlanStrategy::ExactScan);
        assert_eq!(planned.plan, expected.plan, "{conditions:?}");
        assert_eq!(bits(&planned.hits), bits(&expected.hits), "{conditions:?}");
        assert_eq!(
            bits(&planned.hits),
            brute(&model, &QUERIES[0], 7, &conditions, now_ms())
        );
        assert_eq!(planned.stats.index_candidates, 0);
        assert!(
            planned.stats.records_scanned * 2 <= ROWS,
            "{conditions:?}: scanned {}",
            planned.stats.records_scanned
        );
    }
}

#[test]
fn invalid_filters_are_rejected_as_before() {
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    let mut twin = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    declare(&mut db);
    load(&mut db);
    load(&mut twin);
    let invalid = [
        vec![range("ts", None, None)],
        vec![range("ts", Some(int(0)), Some(float(1.0)))],
        vec![range("ts", Some(int(2)), Some(int(1)))],
        vec![range("tag", Some(text("a")), None)],
        vec![range("temp", Some(float(f64::NAN)), None)],
        vec![eq("temp", float(f64::INFINITY))],
        vec![eq("", int(0))],
        vec![range("ts", Some(int(0)), None); 65],
    ];
    for conditions in invalid {
        let error = db
            .search_filtered_with_metric(&QUERIES[0], 3, &conditions, DistanceMetric::SquaredL2)
            .unwrap_err();
        let expected = twin
            .search_filtered_with_metric(&QUERIES[0], 3, &conditions, DistanceMetric::SquaredL2)
            .unwrap_err();
        assert_eq!(format!("{error:?}"), format!("{expected:?}"));
    }
}
