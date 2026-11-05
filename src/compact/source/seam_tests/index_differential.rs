//! Lane B4 slice 3: the declared-index scan differential.
//!
//! One image, two openings: the candidate serves the collection through the compact source, the oracle through the
//! resident records. Both receive the same operations in the same order (puts, deletes, metadata updates and changes
//! of the declared indexes), and after every operation the same battery of exact searches runs on both at explicit
//! clocks. The answers must be equal: the hits, their order and every scan statistic.
//!
//! A third answer comes from a small model of the rows and of the declared-index rule. It is written from the
//! contract, not from the code under test, so that two paths that share a bug cannot agree on a wrong answer:
//!
//! - a row is visible at a clock when it has no deadline or the clock is before it (a deadline hides the row exactly
//!   at the clock);
//! - a filter keeps a row when every condition holds (`Eq` is equality, `Range` is inclusive on integers);
//! - the hits are the nearest rows by squared L2 distance, ties by ascending ID, at most `k`;
//! - a declared index serves a condition when it was declared for its kind (`Eq` or `EqRange` serve `Eq`, `Range`
//!   or `EqRange` serve `Range`); the postings of the most selective served condition narrow the scan when they hold
//!   at most half of the rows, and every other served condition within that bound is intersected with them;
//!   `index_candidates` counts the narrowed set;
//! - the scan examines the narrowed set, or every row when there is none, and scores the visible rows that match.
//!
//! The battery only filters on fields whose declared kind serves the condition's kind, or on fields that are not
//! declared, so the resident block summaries (P5), which only the resident arm consults, never prune here and every
//! statistic is equal. One difference is allowed and documented, and the last test pins it: above one block of rows,
//! a filter that the summaries of a declared field can prune but no posting serves makes the resident scan examine
//! only the blocks the summaries keep (`records_scanned`), while the compact scan, which has no summaries, examines
//! every row. The hits, their order, the distance evaluations and the index candidates stay equal.

use crate::compact::source::dispatch_tests::gates;
use crate::{
    CollectionSettings, CollectionSpec, Condition, CreateOptions, Database, DistanceMetric,
    Metadata, MetadataIndexKind, MetadataIndexSpec, MetadataValue, SearchHit, SearchStats,
};
use std::collections::{BTreeMap, BTreeSet};

/// A deadline far in the future (2100-01-01): no operation sees a row as expired by the wall clock.
const FAR: u64 = 4_102_444_800_000;
const TAGS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
const NAME: &str = "swept";
const QUERY: [f32; 2] = [2.0, 2.0];

/// The declared indexes of each variant: `(tag, n)`.
const VARIANTS: [(Option<MetadataIndexKind>, Option<MetadataIndexKind>); 5] = [
    (Some(MetadataIndexKind::Eq), Some(MetadataIndexKind::Range)),
    (Some(MetadataIndexKind::Eq), None),
    (None, Some(MetadataIndexKind::EqRange)),
    (None, None),
    (
        Some(MetadataIndexKind::EqRange),
        Some(MetadataIndexKind::Range),
    ),
];

#[derive(Clone, Debug)]
struct Row {
    vector: [f32; 2],
    tag: &'static str,
    n: i64,
    deadline: Option<u64>,
}

fn metadata(tag: &str, n: i64) -> Metadata {
    Metadata::from([
        ("tag".to_owned(), MetadataValue::String(tag.to_owned())),
        ("n".to_owned(), MetadataValue::Integer(n)),
    ])
}

fn settings(variant: usize) -> CollectionSettings {
    let (tag, n) = VARIANTS[variant];
    let mut specs = Vec::new();
    for (field, kind) in [("tag", tag), ("n", n)] {
        if let Some(kind) = kind {
            specs.push(MetadataIndexSpec {
                field: field.into(),
                kind,
            });
        }
    }
    CollectionSettings {
        metadata_indexes: specs,
        ..CollectionSettings::default()
    }
}

/// A compact image of one collection holding `rows`, declared as `variant`.
fn image(rows: &BTreeMap<u64, Row>, variant: usize) -> Vec<u8> {
    let mut original = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    original
        .create_collection(
            NAME,
            CollectionSpec {
                dimensions: 2,
                settings: settings(variant),
            },
        )
        .unwrap();
    {
        let mut collection = original.collection_mut(NAME).unwrap();
        for (&id, row) in rows {
            collection
                .put_with_expiry(
                    id,
                    &row.vector,
                    b"",
                    &metadata(row.tag, row.n),
                    row.deadline,
                )
                .unwrap();
        }
    }
    let mut loaded = crate::format::load_with(
        &mut std::io::Cursor::new(original.to_bytes().unwrap()),
        None,
    )
    .unwrap();
    loaded.flags |= crate::format::FLAG_PRECISION;
    for collection in loaded.collections.values_mut() {
        collection.precision = Some(crate::compact::catalog::Precision::Float32);
    }
    let mut bytes = Vec::new();
    crate::maintenance::write_v5_image(
        &mut bytes,
        &loaded,
        loaded.lineage.unwrap(),
        None,
        None,
        None,
        loaded.flags,
        loaded.sequence,
    )
    .unwrap();
    bytes
}

/// The same image, served by the compact source and by the resident records.
fn pair(rows: &BTreeMap<u64, Row>, variant: usize) -> (Database, Database) {
    let bytes = image(rows, variant);
    let oracle = {
        let _source = crate::compact::source::Activation::set(false);
        Database::open_from_bytes(bytes.clone(), None).unwrap()
    };
    let candidate = Database::open_from_bytes(bytes, None).unwrap();
    let id = candidate.collection(NAME).unwrap().id();
    assert!(candidate.state(id).records.is_compact());
    let id = oracle.collection(NAME).unwrap().id();
    assert!(!oracle.state(id).records.is_compact());
    (candidate, oracle)
}

fn search(
    db: &Database,
    k: usize,
    conditions: &[Condition],
    now_ms: u64,
) -> (Vec<SearchHit>, SearchStats) {
    let id = db.collection(NAME).unwrap().id();
    let mut stats = SearchStats::default();
    let hits = db
        .search_in_at(
            id,
            &QUERY,
            k,
            conditions,
            DistanceMetric::SquaredL2,
            now_ms,
            &mut stats,
        )
        .unwrap();
    (hits, stats)
}

fn eq(tag: &str) -> Condition {
    Condition::Eq {
        field: "tag".into(),
        value: MetadataValue::String(tag.into()),
    }
}

fn range(min: Option<i64>, max: Option<i64>) -> Condition {
    Condition::Range {
        field: "n".into(),
        min: min.map(MetadataValue::Integer),
        max: max.map(MetadataValue::Integer),
    }
}

/// What the contract says about a row and a condition, without the code under test.
fn holds(row: &Row, condition: &Condition) -> bool {
    match condition {
        Condition::Eq { field, value } => {
            field == "tag" && matches!(value, MetadataValue::String(tag) if tag == row.tag)
        }
        Condition::Range { field, min, max } => {
            let bound = |value: &Option<MetadataValue>| match value {
                Some(MetadataValue::Integer(bound)) => Some(*bound),
                _ => None,
            };
            field == "n"
                && bound(min).is_none_or(|min| row.n >= min)
                && bound(max).is_none_or(|max| row.n <= max)
        }
        Condition::GeoRadius { .. } => false,
    }
}

/// The rows and the declared indexes, as the operations leave them.
struct Model {
    rows: BTreeMap<u64, Row>,
    variant: usize,
}

impl Model {
    /// Whether the declared indexes serve `condition`.
    fn serves(&self, condition: &Condition) -> bool {
        let (tag, n) = VARIANTS[self.variant];
        match condition {
            Condition::Eq { .. } => matches!(
                tag,
                Some(MetadataIndexKind::Eq | MetadataIndexKind::EqRange)
            ),
            Condition::Range { .. } => matches!(
                n,
                Some(MetadataIndexKind::Range | MetadataIndexKind::EqRange)
            ),
            Condition::GeoRadius { .. } => false,
        }
    }

    /// The scan the contract describes: the hits, and the three statistics an exact scan reports.
    fn expected(
        &self,
        conditions: &[Condition],
        k: usize,
        now_ms: u64,
    ) -> (Vec<SearchHit>, SearchStats) {
        let mut stats = SearchStats::default();
        let cap = self.rows.len() / 2;
        let served: Vec<(&Condition, BTreeSet<u64>)> = conditions
            .iter()
            .filter(|condition| self.serves(condition))
            .map(|condition| {
                let ids = self
                    .rows
                    .iter()
                    .filter(|(_, row)| holds(row, condition))
                    .map(|(&id, _)| id)
                    .collect();
                (condition, ids)
            })
            .collect();
        let narrowest = served.iter().map(|(_, ids)| ids.len()).min();
        let candidates: Option<BTreeSet<u64>> = narrowest.filter(|&count| count <= cap).map(|_| {
            let mut within = served
                .iter()
                .filter(|(_, ids)| ids.len() <= cap)
                .map(|(_, ids)| ids);
            let first = within.next().unwrap().clone();
            within.fold(first, |acc, ids| acc.intersection(ids).copied().collect())
        });
        if let Some(ids) = &candidates {
            stats.index_candidates = ids.len() as u64;
        }
        let examined = candidates.as_ref().map_or(self.rows.len(), BTreeSet::len);
        if k.min(self.rows.len()).min(examined) == 0 {
            return (Vec::new(), stats);
        }
        let mut found = Vec::new();
        for (&id, row) in &self.rows {
            if candidates.as_ref().is_some_and(|ids| !ids.contains(&id)) {
                continue;
            }
            stats.records_scanned += 1;
            if row.deadline.is_some_and(|deadline| deadline <= now_ms) {
                continue;
            }
            if !conditions.iter().all(|condition| holds(row, condition)) {
                continue;
            }
            stats.distance_evaluations += 1;
            let distance: f64 = row
                .vector
                .iter()
                .zip(QUERY)
                .map(|(a, b)| f64::from(a - b) * f64::from(a - b))
                .sum();
            found.push(SearchHit { id, distance });
        }
        found.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
        found.truncate(k);
        (found, stats)
    }
}

#[derive(Clone, Debug)]
enum Op {
    Put(u64, Row),
    Delete(u64),
    Update(u64, &'static str, i64),
    Settings(usize),
}

impl Op {
    fn model(&self, model: &mut Model) {
        match self {
            Op::Put(id, row) => {
                model.rows.insert(*id, row.clone());
            }
            Op::Delete(id) => {
                model.rows.remove(id);
            }
            Op::Update(id, tag, n) => {
                if let Some(row) = model.rows.get_mut(id) {
                    row.tag = tag;
                    row.n = *n;
                }
            }
            Op::Settings(variant) => model.variant = *variant,
        }
    }

    /// The outcome on one database, as text: the answers and the errors must be equal on both.
    fn apply(&self, db: &mut Database) -> String {
        match self {
            Op::Put(id, row) => format!(
                "{:?}",
                db.collection_mut(NAME)
                    .unwrap()
                    .put_with_expiry(
                        *id,
                        &row.vector,
                        b"",
                        &metadata(row.tag, row.n),
                        row.deadline
                    )
                    .map(|_| ())
            ),
            Op::Delete(id) => format!("{:?}", db.collection_mut(NAME).unwrap().delete(*id)),
            Op::Update(id, tag, n) => format!(
                "{:?}",
                db.collection_mut(NAME)
                    .unwrap()
                    .update_metadata(*id, &metadata(tag, *n))
                    .map(|_| ())
            ),
            Op::Settings(variant) => {
                format!("{:?}", db.set_collection_settings(NAME, settings(*variant)))
            }
        }
    }
}

/// A fixed-seed generator: the sweep replays the same operations every time.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

fn row(rng: &mut Rng) -> Row {
    Row {
        vector: [rng.below(5) as f32, rng.below(5) as f32],
        tag: TAGS[rng.below(4) as usize],
        n: rng.below(10) as i64,
        deadline: match rng.below(4) {
            0 => None,
            choice => Some(FAR + 100 * choice),
        },
    }
}

fn battery() -> Vec<Vec<Condition>> {
    let mut all = vec![vec![]];
    all.extend(TAGS.iter().chain(&["nowhere"]).map(|tag| vec![eq(tag)]));
    for (min, max) in [
        (Some(0), Some(2)),
        (Some(3), Some(6)),
        (Some(7), Some(9)),
        (Some(10), Some(20)),
        (Some(5), None),
        (None, Some(4)),
        (Some(4), Some(4)),
    ] {
        all.push(vec![range(min, max)]);
    }
    all.push(vec![eq("alpha"), range(Some(3), Some(6))]);
    all.push(vec![eq("beta"), eq("gamma")]);
    all.push(vec![range(Some(2), Some(8)), range(Some(4), Some(6))]);
    all
}

#[test]
fn compact_declared_index_scans_match_the_resident_oracle_and_the_contract_across_changes() {
    let _gates = gates();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut model = Model {
        rows: (1..=16).map(|id| (id, row(&mut rng))).collect(),
        variant: 0,
    };
    let (mut candidate, mut oracle) = pair(&model.rows, model.variant);
    let battery = battery();
    let clocks = [FAR - 1, FAR + 100, FAR + 250, FAR + 1000];
    let (mut narrowed, mut full, mut empty, mut ties) = (0usize, 0usize, 0usize, 0usize);
    for step in 0..90 {
        let op = match rng.below(20) {
            0..=8 => Op::Put(1 + rng.below(30), row(&mut rng)),
            9..=12 => Op::Delete(1 + rng.below(30)),
            13..=15 => Op::Update(
                1 + rng.below(30),
                TAGS[rng.below(4) as usize],
                rng.below(10) as i64,
            ),
            _ => Op::Settings(rng.below(VARIANTS.len() as u64) as usize),
        };
        let (a, b) = (op.apply(&mut candidate), op.apply(&mut oracle));
        assert_eq!(a, b, "step {step}: {op:?}");
        op.model(&mut model);
        for conditions in &battery {
            for now_ms in clocks {
                for k in [1, 3, 50] {
                    let context = format!(
                        "step {step} after {op:?}, variant {}, {} rows, filter {conditions:?}, clock {now_ms}, k {k}",
                        model.variant,
                        model.rows.len()
                    );
                    let actual = search(&candidate, k, conditions, now_ms);
                    assert_eq!(
                        actual,
                        search(&oracle, k, conditions, now_ms),
                        "{context}: oracle"
                    );
                    let expected = model.expected(conditions, k, now_ms);
                    assert_eq!(actual, expected, "{context}: contract");
                    narrowed += usize::from(actual.1.index_candidates > 0);
                    full +=
                        usize::from(actual.1.index_candidates == 0 && actual.1.records_scanned > 0);
                    empty += usize::from(actual.0.is_empty());
                    ties += actual
                        .0
                        .windows(2)
                        .filter(|pair| pair[0].distance == pair[1].distance)
                        .count();
                }
            }
        }
    }
    // The sweep reaches the cases it exists for: narrowed and full scans, empty answers and ties.
    assert!(
        narrowed > 1000 && full > 1000 && empty > 1000 && ties > 1000,
        "{narrowed} {full} {empty} {ties}"
    );
}

/// The one allowed difference between the two arms (see the module documentation). Above one block of 1024 rows the
/// resident arm may prune with block summaries when no posting serves the filter (here `n` is declared with the
/// equality kind only, so a range on it is not served). The compact arm has no summaries: it examines every row.
/// Hits, their order, the evaluations and the candidates stay equal. The expected numbers follow from the block
/// layout (ID order, 1024 rows per block, a block is kept when its range reaches the filter, and the kept blocks must
/// hold at most half of the rows), not from reading the scan back.
#[test]
fn compact_scan_examines_every_row_where_the_resident_block_summaries_prune() {
    let _gates = gates();
    const ROWS: u64 = 2500;
    let rows: BTreeMap<u64, Row> = (1..=ROWS)
        .map(|id| {
            let row = Row {
                vector: [(id % 7) as f32, (id % 5) as f32],
                tag: TAGS[(id % 4) as usize],
                n: id as i64,
                deadline: None,
            };
            (id, row)
        })
        .collect();
    let (mut candidate, mut oracle) = pair(&rows, 1);
    // `n` is declared by a settings change on both: the equality kind, so no range posting serves it.
    let declared = CollectionSettings {
        metadata_indexes: vec![MetadataIndexSpec {
            field: "n".into(),
            kind: MetadataIndexKind::Eq,
        }],
        ..CollectionSettings::default()
    };
    for db in [&mut candidate, &mut oracle] {
        db.set_collection_settings(NAME, declared.clone()).unwrap();
    }
    // Blocks of 1024 rows in ID order: 1..=1024, 1025..=2048, 2049..=2500. A range is pruned to the blocks it
    // reaches when they hold at most half of the rows (1250); otherwise every row is examined.
    let cases: [(&str, i64, i64, u64, u64); 6] = [
        ("inside the first block", 10, 20, 1024, 11),
        ("inside the second block", 1100, 1200, 1024, 101),
        ("inside the last block", 2200, 2300, 452, 101),
        (
            "two blocks: 1476 rows are above the cap",
            2000,
            2100,
            2500,
            101,
        ),
        ("three blocks", 1, 2500, 2500, 2500),
        ("one row", 1500, 1500, 1024, 1),
    ];
    for (case, min, max, resident_scanned, evaluations) in cases {
        let filter = [range(Some(min), Some(max))];
        let (hits, stats) = search(&candidate, 5, &filter, 1);
        let (oracle_hits, oracle_stats) = search(&oracle, 5, &filter, 1);
        assert_eq!(hits, oracle_hits, "{case}: hits and order");
        assert_eq!(
            stats.distance_evaluations, evaluations,
            "{case}: evaluations"
        );
        assert_eq!(
            stats.distance_evaluations, oracle_stats.distance_evaluations,
            "{case}"
        );
        assert_eq!(
            stats.index_candidates, oracle_stats.index_candidates,
            "{case}"
        );
        assert_eq!(
            stats.index_candidates, 0,
            "{case}: no posting serves a range on an equality index"
        );
        assert_eq!(
            stats.records_scanned, ROWS,
            "{case}: the compact arm examines every row"
        );
        assert_eq!(
            oracle_stats.records_scanned, resident_scanned,
            "{case}: the blocks the summaries keep"
        );
    }
}
