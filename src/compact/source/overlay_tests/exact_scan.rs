//! Lane E4: the bounded exact scan over a compact collection.
//!
//! Every expectation comes from the contract, not from the code under test:
//!
//! - a row is visible at a clock when it has no deadline or the clock is before it;
//! - a filter keeps a row when every condition holds;
//! - squared L2 is the sum of squared differences, inner product the negated dot product and cosine
//!   `1 - dot / (|row| * |query|)` (a row of norm zero has no cosine distance), each folded left to right over the
//!   dimensions in `f64` from the widened `f32` components; the hits are the `k` nearest, ties by ascending ID;
//! - `records_scanned` counts the candidate rows the scan examines before its visibility check and
//!   `distance_evaluations` the visible rows that match, counted when the scan commits to reading the vector;
//! - the vector workspace holds at most 4 MiB of decoded blocks and a block is decoded whole: a block larger than the
//!   budget is an explicit `InvalidQuery`, never a partial answer, and nothing else limits the scan.
//!
//! The model below is the independent answer. The resident oracle is a second opinion where a test uses it.

use super::gates;
use crate::compact::catalog::Precision;
use crate::compact::catalog::tests::source_key;
use crate::compact::source::ReadWorkspace;
use crate::records::CollectionRecords;
use crate::{
    CollectionSpec, Condition, CreateOptions, Database, DistanceMetric, Error, Metadata,
    MetadataValue, SearchHit, SearchStats,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

/// A deadline far in the future (2100-01-01): the wall clock never sees a row as expired.
const FAR: u64 = 4_102_444_800_000;
const NAME: &str = "scan";
const METRICS: [DistanceMetric; 3] = [
    DistanceMetric::SquaredL2,
    DistanceMetric::Cosine,
    DistanceMetric::InnerProduct,
];
const PRECISIONS: [Precision; 2] = [Precision::Float32, Precision::Float16];

#[derive(Clone, Debug)]
struct Row {
    vector: Vec<f32>,
    payload: usize,
    tag: &'static str,
    deadline: Option<u64>,
}

fn row(vector: &[f32], tag: &'static str) -> Row {
    Row {
        vector: vector.to_vec(),
        payload: 0,
        tag,
        deadline: None,
    }
}

fn expiring(mut row: Row, deadline: u64) -> Row {
    row.deadline = Some(deadline);
    row
}

fn metadata(tag: &str) -> Metadata {
    Metadata::from([("tag".to_owned(), MetadataValue::String(tag.to_owned()))])
}

fn tagged(tag: &str) -> Condition {
    Condition::Eq {
        field: "tag".into(),
        value: MetadataValue::String(tag.into()),
    }
}

#[derive(Clone, Debug)]
enum Op {
    Put(u64, Row),
    Delete(u64),
    Retag(u64, &'static str),
}

fn apply(db: &mut Database, op: &Op) {
    let mut collection = db.collection_mut(NAME).unwrap();
    match op {
        Op::Put(id, row) => {
            collection
                .put_with_expiry(
                    *id,
                    &row.vector,
                    &vec![0; row.payload],
                    &metadata(row.tag),
                    row.deadline,
                )
                .unwrap();
        }
        Op::Delete(id) => assert!(collection.delete(*id).unwrap()),
        Op::Retag(id, tag) => {
            collection.update_metadata(*id, &metadata(tag)).unwrap();
        }
    }
}

fn apply_model(rows: &mut BTreeMap<u64, Row>, op: &Op) {
    match op {
        Op::Put(id, row) => {
            rows.insert(*id, row.clone());
        }
        Op::Delete(id) => {
            rows.remove(id).expect("the model holds the deleted row");
        }
        Op::Retag(id, tag) => rows.get_mut(id).expect("the model holds the row").tag = tag,
    }
}

/// A compact image of one collection holding `rows`, with every collection at `precision`.
fn image(
    dimensions: usize,
    rows: &BTreeMap<u64, Row>,
    precision: Precision,
    encrypted: bool,
) -> Vec<u8> {
    let key = encrypted.then(source_key);
    let mut original = Database::create_in_memory(
        2,
        CreateOptions {
            encryption: key.clone(),
            ..CreateOptions::default()
        },
    )
    .unwrap();
    original
        .create_collection(
            NAME,
            CollectionSpec {
                dimensions,
                settings: crate::CollectionSettings::default(),
            },
        )
        .unwrap();
    for (&id, row) in rows {
        apply(&mut original, &Op::Put(id, row.clone()));
    }
    let mut loaded = crate::format::load_with(
        &mut std::io::Cursor::new(original.to_bytes().unwrap()),
        key.as_ref(),
    )
    .unwrap();
    loaded.flags |= crate::format::FLAG_PRECISION;
    for collection in loaded.collections.values_mut() {
        collection.precision = Some(precision);
    }
    let mut bytes = Vec::new();
    crate::maintenance::write_v5_image(
        &mut bytes,
        &loaded,
        loaded.lineage.unwrap(),
        None,
        None,
        loaded.cipher.as_ref(),
        loaded.flags & (crate::format::FLAG_GRAPH | crate::format::FLAG_PRECISION),
        loaded.sequence,
    )
    .unwrap();
    bytes
}

/// The same image served by the compact source (candidate) and by resident records (oracle).
fn open(bytes: &[u8], encrypted: bool) -> (Database, Database) {
    let key = encrypted.then(source_key);
    let oracle = {
        let _source = crate::compact::source::Activation::set(false);
        Database::open_from_bytes(bytes.to_vec(), key.as_ref()).unwrap()
    };
    let candidate = Database::open_from_bytes(bytes.to_vec(), key.as_ref()).unwrap();
    let id = candidate.collection(NAME).unwrap().id();
    assert!(candidate.state(id).records.is_compact());
    let id = oracle.collection(NAME).unwrap().id();
    assert!(!oracle.state(id).records.is_compact());
    (candidate, oracle)
}

fn view(db: &Database) -> crate::compact::source::CompactView {
    let id = db.collection(NAME).unwrap().id();
    let CollectionRecords::Compact { view, .. } = &db.state(id).records else {
        panic!("the collection is compact");
    };
    view.clone()
}

/// `(decoded record cells, initialized record cells)` of the generation behind the collection.
fn cells(db: &Database) -> (usize, usize) {
    let view = view(db);
    let generation = &view.generation;
    let initialized = generation
        .collections
        .values()
        .flat_map(|source| source.cells.iter())
        .filter(|cell| cell.record.get().is_some())
        .count();
    (generation.decode_count.load(Ordering::Relaxed), initialized)
}

/// The rows of each physical block of the collection, in order.
fn block_rows(db: &Database) -> Vec<usize> {
    let view = view(db);
    view.generation.collections[&view.collection]
        .blocks
        .iter()
        .map(|block| block.source.rows.len())
        .collect()
}

// ------------------------------------------------------------------------------------------------ the model

fn fold(terms: impl Iterator<Item = f64>) -> f64 {
    let mut sum = 0.0;
    for term in terms {
        sum += term;
    }
    sum
}

fn norm(vector: &[f32]) -> f64 {
    fold(vector.iter().map(|&x| f64::from(x) * f64::from(x))).sqrt()
}

fn model_distance(metric: DistanceMetric, row: &[f32], query: &[f32]) -> Option<f64> {
    let dot = || {
        fold(
            row.iter()
                .zip(query)
                .map(|(&r, &q)| f64::from(r) * f64::from(q)),
        )
    };
    match metric {
        DistanceMetric::SquaredL2 => Some(fold(row.iter().zip(query).map(|(&r, &q)| {
            let difference = f64::from(r) - f64::from(q);
            difference * difference
        }))),
        DistanceMetric::InnerProduct => {
            let dot = dot();
            Some(if dot == 0.0 { 0.0 } else { -dot })
        }
        DistanceMetric::Cosine => {
            let (row_norm, query_norm) = (norm(row), norm(query));
            if row_norm == 0.0 || query_norm == 0.0 {
                None
            } else {
                Some(1.0 - (dot() / (row_norm * query_norm)).clamp(-1.0, 1.0))
            }
        }
    }
}

#[derive(Debug, PartialEq)]
struct Answer {
    /// `(id, distance bits)` in answer order.
    hits: Vec<(u64, u64)>,
    scanned: u64,
    evaluations: u64,
}

/// The contract's answer over `rows` at the clock `now`, optionally restricted to a tag.
fn model(
    rows: &BTreeMap<u64, Row>,
    query: &[f32],
    metric: DistanceMetric,
    k: usize,
    now: u64,
    tag: Option<&str>,
) -> Answer {
    let (mut scored, mut scanned, mut evaluations) = (Vec::new(), 0, 0);
    for (&id, row) in rows {
        scanned += 1;
        if row.deadline.is_some_and(|deadline| deadline <= now)
            || tag.is_some_and(|tag| row.tag != tag)
        {
            continue;
        }
        evaluations += 1;
        if let Some(distance) = model_distance(metric, &row.vector, query) {
            scored.push((distance, id));
        }
    }
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    scored.truncate(k);
    Answer {
        hits: scored
            .into_iter()
            .map(|(distance, id)| (id, distance.to_bits()))
            .collect(),
        scanned,
        evaluations,
    }
}

fn bits(hits: &[SearchHit]) -> Vec<(u64, u64)> {
    hits.iter()
        .map(|hit| (hit.id, hit.distance.to_bits()))
        .collect()
}

fn answer(hits: &[SearchHit], stats: &SearchStats) -> Answer {
    Answer {
        hits: bits(hits),
        scanned: stats.records_scanned,
        evaluations: stats.distance_evaluations,
    }
}

fn ids(hits: &[(u64, u64)]) -> Vec<u64> {
    hits.iter().map(|&(id, _)| id).collect()
}

// ------------------------------------------------------------------------------------------- the consumers

#[derive(Clone, Copy, Debug)]
enum Path {
    Helper,
    CollectionState,
    DatabaseSearch,
    Snapshot,
    Handle,
}

const PATHS: [Path; 5] = [
    Path::Helper,
    Path::CollectionState,
    Path::DatabaseSearch,
    Path::Snapshot,
    Path::Handle,
];

/// One exact search through `path`. The handle path reports no statistics.
fn run(
    db: &Database,
    path: Path,
    query: &[f32],
    metric: DistanceMetric,
    k: usize,
    conditions: &[Condition],
    now: u64,
) -> (Vec<SearchHit>, Option<SearchStats>) {
    let id = db.collection(NAME).unwrap().id();
    let state = db.state(id);
    let query_norm = if metric == DistanceMetric::Cosine {
        norm(query)
    } else {
        0.0
    };
    let mut stats = SearchStats::default();
    let hits = match path {
        Path::Helper => crate::exact_search_compact(
            &state.records,
            &state.expiries,
            query,
            query_norm,
            k,
            conditions,
            metric,
            None,
            now,
            &mut stats,
        )
        .unwrap(),
        Path::CollectionState => state
            .exact_search_candidates(
                query, query_norm, k, conditions, metric, None, now, &mut stats,
            )
            .unwrap(),
        Path::DatabaseSearch => db
            .search_in_at(id, query, k, conditions, metric, now, &mut stats)
            .unwrap(),
        Path::Snapshot => db
            .collection(NAME)
            .unwrap()
            .read_snapshot()
            .unwrap()
            .search_with_stats_at(query, k, conditions, metric, now, &mut stats)
            .unwrap(),
        Path::Handle => {
            let hits = db
                .collection(NAME)
                .unwrap()
                .search_filtered_with_metric_at(query, k, conditions, metric, now)
                .unwrap();
            return (hits, None);
        }
    };
    (hits, Some(stats))
}

// --------------------------------------------------------------------------------------------------- G2

/// Rows 10..=50 sit at `[id, 0]`. The overlay then: an overlay-only first ID (5), a replacement of a base ID (20), a
/// tombstone (30), an overlay row with a deadline over a base row (40), an overlay-only last ID (60). The scan has
/// to see the effective rows in ID order, each once, with the newest entry deciding every ID.
fn ordered_scenario() -> (BTreeMap<u64, Row>, Vec<Op>) {
    let rows = (1..=5)
        .map(|n| (n * 10, row(&[(n * 10) as f32, 0.0], "base")))
        .collect();
    let ops = vec![
        Op::Put(5, row(&[5.0, 0.0], "overlay")),
        Op::Put(20, row(&[21.0, 0.0], "replaced")),
        Op::Delete(30),
        Op::Put(40, expiring(row(&[40.0, 0.0], "expiring"), FAR)),
        Op::Put(60, row(&[60.0, 0.0], "overlay")),
    ];
    (rows, ops)
}

#[test]
fn compact_exact_scan_consumes_ordered_ids_across_base_and_overlay() {
    let _gates = gates();
    let (base, ops) = ordered_scenario();
    let mut effective = base.clone();
    for op in &ops {
        apply_model(&mut effective, op);
    }
    assert_eq!(
        effective.keys().copied().collect::<Vec<_>>(),
        vec![5, 10, 20, 40, 50, 60],
        "the model of the fixture"
    );
    for precision in PRECISIONS {
        for encrypted in [false, true] {
            let bytes = image(2, &base, precision, encrypted);
            let (mut candidate, mut oracle) = open(&bytes, encrypted);
            for op in &ops {
                apply(&mut candidate, op);
                apply(&mut oracle, op);
            }
            // The literal answer: squared distances to [21, 0] are 5: 256, 10: 121, 20: 0, 40: 361, 50: 841, 60: 1521,
            // the row at 40 expires at FAR, the tombstoned 30 is gone.
            let (hits, stats) = run(
                &candidate,
                Path::DatabaseSearch,
                &[21.0, 0.0],
                DistanceMetric::SquaredL2,
                10,
                &[],
                FAR - 1,
            );
            assert_eq!(
                hits.iter()
                    .map(|hit| (hit.id, hit.distance))
                    .collect::<Vec<_>>(),
                vec![
                    (20, 0.0),
                    (10, 121.0),
                    (5, 256.0),
                    (40, 361.0),
                    (50, 841.0),
                    (60, 1521.0)
                ],
                "{precision:?} encrypted {encrypted}"
            );
            let stats = stats.unwrap();
            assert_eq!((stats.records_scanned, stats.distance_evaluations), (6, 6));
            for now in [FAR - 1, FAR] {
                // Queries on each end of the ID order and in the middle: the nearest row is the first overlay-only ID,
                // the first base ID, a replaced ID and the last overlay-only ID.
                for query in [
                    [5.0, 0.0],
                    [10.0, 0.0],
                    [21.0, 0.0],
                    [60.0, 0.0],
                    [47.0, 0.0],
                ] {
                    for metric in [DistanceMetric::SquaredL2, DistanceMetric::InnerProduct] {
                        for k in [1, 2, 10] {
                            let expected = model(&effective, &query, metric, k, now, None);
                            for path in PATHS {
                                let (hits, stats) =
                                    run(&candidate, path, &query, metric, k, &[], now);
                                let label = format!(
                                    "{precision:?} {encrypted} {path:?} {metric:?} {query:?} k={k} at {now}"
                                );
                                assert_eq!(bits(&hits), expected.hits, "{label}");
                                if let Some(stats) = stats {
                                    assert_eq!(answer(&hits, &stats), expected, "{label}");
                                }
                                let (resident, _) = run_resident(&oracle, &query, metric, k, now);
                                assert_eq!(bits(&resident), expected.hits, "{label}: oracle");
                            }
                        }
                    }
                }
            }
            // No ID is listed twice, and the expired overlay row is scanned and hidden at its deadline.
            let (hits, stats) = run(
                &candidate,
                Path::Helper,
                &[21.0, 0.0],
                DistanceMetric::SquaredL2,
                10,
                &[],
                FAR,
            );
            assert_eq!(ids(&bits(&hits)), vec![20, 10, 5, 50, 60]);
            let stats = stats.unwrap();
            assert_eq!((stats.records_scanned, stats.distance_evaluations), (6, 5));
        }
    }
}

fn run_resident(
    oracle: &Database,
    query: &[f32],
    metric: DistanceMetric,
    k: usize,
    now: u64,
) -> (Vec<SearchHit>, SearchStats) {
    let id = oracle.collection(NAME).unwrap().id();
    let mut stats = SearchStats::default();
    let hits = oracle
        .search_in_at(id, query, k, &[], metric, now, &mut stats)
        .unwrap();
    (hits, stats)
}

// --------------------------------------------------------------------------------------------------- G3

/// Rows with exactly representable components in either source precision. The query `[3, 4, 0, 0]` has norm 5, so
/// several rows have exactly known cosine distances (same direction, opposite, orthogonal), one is a tie by ID and
/// two have norm zero.
fn dataset() -> BTreeMap<u64, Row> {
    let mut rows = BTreeMap::new();
    for (id, vector, tag) in [
        (1, [3.0, 4.0, 0.0, 0.0], "a"),
        (2, [6.0, 8.0, 0.0, 0.0], "a"),
        (3, [0.0, 0.0, 3.0, 4.0], "b"),
        (4, [-3.0, -4.0, 0.0, 0.0], "b"),
        (5, [4.0, 3.0, 0.0, 0.0], "a"),
        (6, [0.0, 0.0, 0.0, 0.0], "a"),
        (8, [1.0, 1.0, 1.0, 1.0], "a"),
        (9, [2.0, 0.0, 0.0, 0.0], "a"),
        (10, [0.0, 2.0, 0.0, 0.0], "b"),
    ] {
        rows.insert(id, row(&vector, tag));
    }
    rows.insert(7, expiring(row(&[3.0, 4.0, 0.0, 0.0], "b"), FAR));
    for id in 11..=40u64 {
        rows.insert(
            id,
            row(
                &[(id % 3) as f32, (id % 2) as f32, 0.0, 1.0],
                if id % 2 == 0 { "a" } else { "b" },
            ),
        );
    }
    rows
}

/// Overlay changes: a replacement, a tombstone, an overlay row with a deadline, a metadata-only change, a deadline
/// over a base row and a row of norm zero.
fn dataset_ops() -> Vec<Op> {
    vec![
        Op::Put(20, row(&[3.0, 4.0, 0.0, 0.0], "a")),
        Op::Delete(5),
        Op::Put(100, expiring(row(&[3.0, 4.0, 0.0, 0.0], "b"), FAR)),
        Op::Retag(9, "b"),
        Op::Put(2, expiring(row(&[6.0, 8.0, 0.0, 0.0], "a"), FAR)),
        Op::Put(101, row(&[0.0, 0.0, 0.0, 0.0], "a")),
    ]
}

fn fresh(bytes: &[u8], encrypted: bool, ops: &[Op]) -> Database {
    let key = encrypted.then(source_key);
    let mut candidate = Database::open_from_bytes(bytes.to_vec(), key.as_ref()).unwrap();
    for op in ops {
        apply(&mut candidate, op);
    }
    candidate
}

#[test]
fn compact_exact_scan_scores_without_materializing_record_cells() {
    let _gates = gates();
    let base = dataset();
    let ops = dataset_ops();
    let mut effective = base.clone();
    for op in &ops {
        apply_model(&mut effective, op);
    }
    let query = [3.0, 4.0, 0.0, 0.0];
    let filters: [(&[Condition], Option<&str>); 2] = [(&[], None), (&[tagged("a")], Some("a"))];
    for precision in PRECISIONS {
        for encrypted in [false, true] {
            let bytes = image(4, &base, precision, encrypted);
            for (changed, ops) in [(false, &[][..]), (true, &ops[..])] {
                let rows = if changed { &effective } else { &base };
                for metric in METRICS {
                    for now in [FAR - 1, FAR] {
                        for (conditions, tag) in filters {
                            for k in [1, 8] {
                                let expected = model(rows, &query, metric, k, now, tag);
                                for path in PATHS {
                                    // A fresh database per measurement: an earlier read cannot mask a later one.
                                    let db = fresh(&bytes, encrypted, ops);
                                    let before = cells(&db);
                                    if !changed {
                                        assert_eq!(
                                            before,
                                            (0, 0),
                                            "an untouched generation has no cells"
                                        );
                                    }
                                    let (hits, stats) =
                                        run(&db, path, &query, metric, k, conditions, now);
                                    let label = format!(
                                        "{precision:?} encrypted {encrypted} changed {changed} {path:?} {metric:?} k={k} at {now} filter {tag:?}"
                                    );
                                    assert_eq!(
                                        cells(&db),
                                        before,
                                        "{label}: scoring materialized cells"
                                    );
                                    assert_eq!(bits(&hits), expected.hits, "{label}");
                                    if let Some(stats) = stats {
                                        assert_eq!(answer(&hits, &stats), expected, "{label}");
                                        assert_eq!(stats.index_candidates, 0, "{label}");
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // The exact answers that need no arithmetic: the nearest rows of the query [3, 4, 0, 0].
            let db = fresh(&bytes, encrypted, &ops);
            let pin = |metric, now, k| {
                let (hits, _) = run(&db, Path::DatabaseSearch, &query, metric, k, &[], now);
                hits.into_iter()
                    .map(|hit| (hit.id, hit.distance))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                pin(DistanceMetric::SquaredL2, FAR - 1, 4),
                vec![(1, 0.0), (7, 0.0), (20, 0.0), (100, 0.0)],
                "four rows at the query, ties by ID"
            );
            assert_eq!(
                pin(DistanceMetric::SquaredL2, FAR, 2),
                vec![(1, 0.0), (20, 0.0)]
            );
            assert_eq!(
                pin(DistanceMetric::Cosine, FAR - 1, 3),
                vec![(1, 0.0), (2, 0.0), (7, 0.0)]
            );
            assert_eq!(
                pin(DistanceMetric::Cosine, FAR, 2),
                vec![(1, 0.0), (20, 0.0)]
            );
            assert_eq!(
                pin(DistanceMetric::InnerProduct, FAR - 1, 1),
                vec![(2, -50.0)]
            );
            assert_eq!(
                pin(DistanceMetric::InnerProduct, FAR, 2),
                vec![(1, -25.0), (20, -25.0)]
            );
            // Rows of norm zero have no cosine distance but are evaluated (rows 6 and 101).
            let (hits, stats) = run(
                &db,
                Path::Helper,
                &query,
                DistanceMetric::Cosine,
                1000,
                &[],
                FAR - 1,
            );
            let stats = stats.unwrap();
            assert!(!ids(&bits(&hits)).iter().any(|id| [6, 101].contains(id)));
            assert_eq!(stats.distance_evaluations, stats.records_scanned);
            assert_eq!(hits.len() as u64, stats.distance_evaluations - 2);

            // The accounting of one scan's own workspace shows no cell either.
            let db = fresh(&bytes, encrypted, &ops);
            let before = cells(&db);
            let id = db.collection(NAME).unwrap().id();
            let state = db.state(id);
            let mut workspace = ReadWorkspace::new(4 << 20).unwrap();
            let cell_bytes = state
                .records
                .source_accounting(&mut workspace)
                .unwrap()
                .unwrap()
                .cells_live;
            crate::exact_search_compact_with_workspace(
                &state.records,
                &state.expiries,
                &query,
                0.0,
                8,
                &[],
                DistanceMetric::SquaredL2,
                None,
                FAR - 1,
                &mut SearchStats::default(),
                &mut workspace,
            )
            .unwrap();
            let accounting = state
                .records
                .source_accounting(&mut workspace)
                .unwrap()
                .unwrap();
            assert_eq!(accounting.cells_live, cell_bytes);
            assert!(accounting.vectors_live > 0);
            assert_eq!(cells(&db), before);
        }
    }
}

// --------------------------------------------------------------------------------------------------- G4

const HEAD_ROWS: u64 = 2;
const BODY_ROWS: u64 = 8;
const TAIL_ROWS: u64 = 3;
/// Decoded bytes of each block of the staged collection (two dimensions, four bytes a component).
const HEAD: usize = HEAD_ROWS as usize * 8;
const BODY: usize = BODY_ROWS as usize * 8;
const TAIL: usize = TAIL_ROWS as usize * 8;

/// A block closes when its payload and metadata reach 1 MiB, so two rows with 600 KiB payloads fill the first block
/// and eight rows with 128 KiB payloads the second; three small rows are the third. The blocks decode to 16, 64 and
/// 24 bytes: the biggest one is in the middle, after progress and before more rows.
fn staged() -> BTreeMap<u64, Row> {
    let mut rows = BTreeMap::new();
    for id in 0..HEAD_ROWS + BODY_ROWS + TAIL_ROWS {
        let mut staged = row(
            &[(id % 64) as f32, 0.0],
            if id < HEAD_ROWS { "head" } else { "tail" },
        );
        if id < HEAD_ROWS {
            staged.payload = 600 * 1024;
        } else if id < HEAD_ROWS + BODY_ROWS {
            staged.payload = 128 * 1024;
        }
        rows.insert(id, staged);
    }
    rows
}

struct Scan {
    result: Result<Vec<SearchHit>, Error>,
    stats: SearchStats,
    accounting: crate::compact::source::SeamAccounting,
}

fn scan(
    db: &Database,
    workspace: &mut ReadWorkspace,
    conditions: &[Condition],
    candidates: Option<&BTreeSet<u64>>,
    k: usize,
) -> Scan {
    let id = db.collection(NAME).unwrap().id();
    let state = db.state(id);
    let mut stats = SearchStats::default();
    let result = crate::exact_search_compact_with_workspace(
        &state.records,
        &state.expiries,
        &[3.0, 0.0],
        0.0,
        k,
        conditions,
        DistanceMetric::SquaredL2,
        candidates,
        0,
        &mut stats,
        workspace,
    );
    let accounting = state.records.source_accounting(workspace).unwrap().unwrap();
    Scan {
        result,
        stats,
        accounting,
    }
}

fn expect_error(scan: &Scan) {
    assert!(
        matches!(
            &scan.result,
            Err(Error::InvalidQuery(
                "compact scratch budget is smaller than one block"
            ))
        ),
        "{:?}",
        scan.result
    );
}

#[test]
fn compact_exact_scan_workspace_boundaries_preserve_explicit_error_accounting() {
    let _gates = gates();
    let base = staged();
    let total = HEAD_ROWS + BODY_ROWS + TAIL_ROWS;
    let expected = model(&base, &[3.0, 0.0], DistanceMetric::SquaredL2, 5, 0, None);
    assert_eq!(expected.scanned, total);
    // Encrypted frames are authenticated again on every row read, and that is slow in a debug build: the encrypted run
    // keeps the scans that cross the boundaries and leaves the full-scan variants to the plain runs.
    for (precision, encrypted) in [
        (Precision::Float32, false),
        (Precision::Float16, false),
        (Precision::Float32, true),
    ] {
        {
            let bytes = image(2, &base, precision, encrypted);
            let (db, _) = open(&bytes, encrypted);
            let label = format!("{precision:?} encrypted {encrypted}");
            assert_eq!(
                block_rows(&db),
                vec![HEAD_ROWS as usize, BODY_ROWS as usize, TAIL_ROWS as usize],
                "{label}: the blocks of the fixture"
            );

            // Exact fit of the biggest block: the scan succeeds and evicts the smaller blocks as it goes.
            for (budget, evictions, live, peak) in [
                (BODY, 2, TAIL, BODY),
                (BODY + HEAD + TAIL - 1, 1, BODY + TAIL, BODY + TAIL),
                (
                    BODY + HEAD + TAIL,
                    0,
                    BODY + HEAD + TAIL,
                    BODY + HEAD + TAIL,
                ),
            ]
            .into_iter()
            .take(if encrypted { 1 } else { 3 })
            {
                let mut workspace = ReadWorkspace::new(budget).unwrap();
                let done = scan(&db, &mut workspace, &[], None, 5);
                let hits = done
                    .result
                    .as_ref()
                    .unwrap_or_else(|e| panic!("{label} budget {budget}: {e:?}"));
                assert_eq!(bits(hits), expected.hits, "{label} budget {budget}");
                assert_eq!(done.stats.records_scanned, total);
                assert_eq!(done.stats.distance_evaluations, total);
                let a = done.accounting;
                assert_eq!(
                    (a.misses, a.hits),
                    (3, total - 3),
                    "{label} budget {budget}"
                );
                assert_eq!(a.evictions, evictions, "{label} budget {budget}");
                assert_eq!(
                    (a.vectors_live, a.vectors_peak),
                    (live, peak),
                    "{label} budget {budget}"
                );
                assert_eq!(a.cells_live, 0);
                assert_eq!(a.plaintext_live > 0, encrypted);
            }

            // Just short of the biggest block: the first block is scanned, the refusal comes at the first row of the
            // second block, after that row was counted as examined and as evaluated, with no hits and the earlier
            // cache state untouched by the refused read.
            let mut workspace = ReadWorkspace::new(BODY - 1).unwrap();
            let refused = scan(&db, &mut workspace, &[], None, 5);
            expect_error(&refused);
            assert_eq!(refused.stats.records_scanned, HEAD_ROWS + 1, "{label}");
            assert_eq!(refused.stats.distance_evaluations, HEAD_ROWS + 1, "{label}");
            let a = refused.accounting;
            assert_eq!(
                (a.misses, a.hits, a.evictions),
                (1, HEAD_ROWS - 1, 0),
                "{label}"
            );
            assert_eq!((a.vectors_live, a.vectors_peak), (HEAD, HEAD), "{label}");
            // The same workspace refuses again the same way and counts only the attempted row.
            let again = scan(&db, &mut workspace, &[], None, 5);
            expect_error(&again);
            assert_eq!(again.stats.records_scanned, HEAD_ROWS + 1);
            assert_eq!(again.stats.distance_evaluations, HEAD_ROWS + 1);
            let a = again.accounting;
            assert_eq!(
                (a.misses, a.hits, a.evictions),
                (1, 2 * (HEAD_ROWS - 1) + 1, 0),
                "{label}"
            );
            assert_eq!((a.vectors_live, a.vectors_peak), (HEAD, HEAD), "{label}");

            // Nothing fits in a zero budget: the first row counts and the cache stays empty.
            let mut workspace = ReadWorkspace::new(0).unwrap();
            let refused = scan(&db, &mut workspace, &[], None, 5);
            expect_error(&refused);
            assert_eq!(
                (
                    refused.stats.records_scanned,
                    refused.stats.distance_evaluations
                ),
                (1, 1)
            );
            let a = refused.accounting;
            assert_eq!(
                (
                    a.misses,
                    a.hits,
                    a.evictions,
                    a.vectors_live,
                    a.vectors_peak
                ),
                (0, 0, 0, 0, 0)
            );

            // A candidate set that stays inside the first block never reaches the oversized one.
            let inside: BTreeSet<u64> = (0..HEAD_ROWS).collect();
            let mut workspace = ReadWorkspace::new(HEAD).unwrap();
            let cached = scan(&db, &mut workspace, &[], Some(&inside), 5);
            assert_eq!(cached.stats.records_scanned, HEAD_ROWS);
            assert_eq!(cached.result.unwrap().len(), HEAD_ROWS as usize);

            // A warmed workspace: the first block is cached, so the next scan hits it before the refusal.
            let mut workspace = ReadWorkspace::new(BODY - 1).unwrap();
            scan(&db, &mut workspace, &[], Some(&inside), 5)
                .result
                .unwrap();
            let refused = scan(&db, &mut workspace, &[], None, 5);
            expect_error(&refused);
            assert_eq!(refused.stats.records_scanned, HEAD_ROWS + 1);
            let a = refused.accounting;
            assert_eq!(
                (a.misses, a.hits, a.evictions),
                (1, 2 * HEAD_ROWS - 1, 0),
                "{label}"
            );
            if encrypted {
                continue;
            }
            // A block whose vectors no row needs is never decoded: only the head rows match the filter.
            let mut workspace = ReadWorkspace::new(HEAD).unwrap();
            let head = scan(&db, &mut workspace, &[tagged("head")], None, 10);
            let hits = head.result.as_ref().unwrap();
            let rows: BTreeMap<u64, Row> = base
                .iter()
                .filter(|&(&id, _)| id < HEAD_ROWS)
                .map(|(&id, row)| (id, row.clone()))
                .collect();
            assert_eq!(
                bits(hits),
                model(&rows, &[3.0, 0.0], DistanceMetric::SquaredL2, 10, 0, None).hits
            );
            assert_eq!(
                (head.stats.records_scanned, head.stats.distance_evaluations),
                (total, HEAD_ROWS)
            );
            assert_eq!(
                (head.accounting.misses, head.accounting.vectors_live),
                (1, HEAD)
            );

            // Ordinary eviction is not a refusal: with room for the biggest block, the warmed block is evicted.
            let mut workspace = ReadWorkspace::new(BODY).unwrap();
            scan(&db, &mut workspace, &[], Some(&inside), 5)
                .result
                .unwrap();
            let done = scan(&db, &mut workspace, &[], None, 5);
            assert_eq!(bits(done.result.as_ref().unwrap()), expected.hits);
            let a = done.accounting;
            assert_eq!((a.misses, a.evictions), (3, 2), "{label}");
            assert_eq!(
                a.hits,
                (HEAD_ROWS - 1) + HEAD_ROWS + (total - HEAD_ROWS - 2),
                "{label}"
            );
            assert_eq!(a.vectors_peak, BODY);

            // The wrapper's own workspace is the 4 MiB one: the same scan answers through it.
            let id = db.collection(NAME).unwrap().id();
            let state = db.state(id);
            let mut stats = SearchStats::default();
            let hits = crate::exact_search_compact(
                &state.records,
                &state.expiries,
                &[3.0, 0.0],
                0.0,
                5,
                &[],
                DistanceMetric::SquaredL2,
                None,
                0,
                &mut stats,
            )
            .unwrap();
            assert_eq!(bits(&hits), expected.hits);
            assert_eq!(stats.records_scanned, total);
        }
    }
}

/// 512 dimensions: a full block decodes to 1 MiB (float32, 512 rows) or 2 MiB (float16, 1,024 rows).
#[test]
fn compact_exact_scan_workspace_ceiling_and_the_largest_blocks() {
    let _gates = gates();
    let ceiling = 4 << 20;
    assert!(ReadWorkspace::new(ceiling).is_ok());
    assert!(matches!(
        ReadWorkspace::new(ceiling + 1),
        Err(Error::InvalidQuery("compact workspace exceeds 4 MiB"))
    ));
    let component = |id: u64, dimension: usize| ((id as usize + dimension) % 4) as f32;
    let rows: BTreeMap<u64, Row> = (0..1024u64)
        .map(|id| {
            let vector: Vec<f32> = (0..512).map(|d| component(id, d)).collect();
            (id, row(&vector, "wide"))
        })
        .collect();
    let query: Vec<f32> = (0..512).map(|d| component(7, d) * 0.5).collect();
    for (precision, block, rows_per_block) in [
        (Precision::Float32, 1usize << 20, 512usize),
        (Precision::Float16, 2 << 20, 1024),
    ] {
        // Plain only: an encrypted frame is authenticated again on every row read, which costs a whole 1-2 MiB block
        // per row here; the encrypted boundaries are pinned on the small staged blocks above.
        for encrypted in [false] {
            let bytes = image(512, &rows, precision, encrypted);
            let (db, _) = open(&bytes, encrypted);
            let label = format!("{precision:?} encrypted {encrypted}");
            let layout: Vec<usize> = block_rows(&db);
            assert_eq!(
                layout,
                vec![rows_per_block; 1024 / rows_per_block],
                "{label}"
            );
            assert_eq!(rows_per_block * 512 * 4, block);
            let expected = model(&rows, &query, DistanceMetric::SquaredL2, 3, 0, None);
            let id = db.collection(NAME).unwrap().id();
            let state = db.state(id);
            for (budget, fits) in [(block, true), (block - 1, false), (ceiling, true)] {
                let mut workspace = ReadWorkspace::new(budget).unwrap();
                let mut stats = SearchStats::default();
                let result = crate::exact_search_compact_with_workspace(
                    &state.records,
                    &state.expiries,
                    &query,
                    0.0,
                    3,
                    &[],
                    DistanceMetric::SquaredL2,
                    None,
                    0,
                    &mut stats,
                    &mut workspace,
                );
                let accounting = state
                    .records
                    .source_accounting(&mut workspace)
                    .unwrap()
                    .unwrap();
                if fits {
                    assert_eq!(
                        bits(&result.unwrap()),
                        expected.hits,
                        "{label} budget {budget}"
                    );
                    assert_eq!(stats.records_scanned, 1024);
                    assert!(accounting.vectors_peak <= budget);
                    // With room for every block the cache holds all 1,024 rows (2 MiB); otherwise only one block at a time.
                    let held = if budget == ceiling {
                        1024 * 512 * 4
                    } else {
                        block
                    };
                    assert_eq!(accounting.vectors_peak, held);
                } else {
                    assert!(
                        matches!(result, Err(Error::InvalidQuery(reason)) if reason == "compact scratch budget is smaller than one block")
                    );
                    assert_eq!(
                        (stats.records_scanned, stats.distance_evaluations),
                        (1, 1),
                        "{label}"
                    );
                    assert_eq!((accounting.vectors_live, accounting.misses), (0, 0));
                }
            }
            // The wrapper creates the real 4 MiB workspace.
            let mut stats = SearchStats::default();
            let hits = crate::exact_search_compact(
                &state.records,
                &state.expiries,
                &query,
                0.0,
                3,
                &[],
                DistanceMetric::SquaredL2,
                None,
                0,
                &mut stats,
            )
            .unwrap();
            assert_eq!(bits(&hits), expected.hits, "{label}");
        }
    }
}
