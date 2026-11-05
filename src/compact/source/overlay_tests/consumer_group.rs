//! Grouped search and facets over compact records.
//!
//! Both APIs sample the wall clock themselves, so every deadline in this file is far in the past (`PAST`) or far in
//! the future (`FAR`). Expectations are the resident oracle at the same state plus the IDs and counts worked out by
//! hand in each test from the rows it writes.

use super::super::dispatch_tests::{compact_dispatch_fixture, gates};
use super::assert_compact_resident_pair;
use super::consumer_cursor::{decode_count, decode_count_in, poison_cell};
use crate::{
    CollectionMut, CollectionRef, Condition, Database, DistanceMetric, Error, FacetCount, Metadata,
    MetadataValue, SearchGroup,
};

const MAX: u64 = u64::MAX;
const PAST: u64 = 1;
/// 2100-01-01, the deadline of the fixture's expiring row (ID 3).
const FAR: u64 = 4_102_444_800_000;
const ORIGIN: [f32; 2] = [0.0, 0.0];
const METRIC: DistanceMetric = DistanceMetric::SquaredL2;
const POISONED: &str = "compact record cell lock poisoned";
/// The `id` metadata of the base row `u64::MAX`.
const MAX_TEXT: &str = "18446744073709551615";

type Groups = Vec<(MetadataValue, Vec<u64>)>;
type Counts = Vec<(MetadataValue, u64)>;

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

fn int(value: i64) -> MetadataValue {
    MetadataValue::Integer(value)
}

fn float(value: f64) -> MetadataValue {
    MetadataValue::Float(value)
}

fn flag(value: bool) -> MetadataValue {
    MetadataValue::Boolean(value)
}

fn geo(latitude: f64, longitude: f64) -> MetadataValue {
    MetadataValue::GeoPoint {
        latitude,
        longitude,
    }
}

fn flagged(value: MetadataValue, rank: i64) -> Metadata {
    Metadata::from([("flag".into(), value), ("rank".into(), int(rank))])
}

fn fixture() -> (Database, Database) {
    let (candidate, oracle, _) = compact_dispatch_fixture(false);
    (candidate, oracle)
}

/// The same edit on the compact candidate and on the resident oracle.
fn both(
    candidate: &mut Database,
    oracle: &mut Database,
    name: &str,
    edit: impl Fn(&mut CollectionMut<'_>),
) {
    for db in [candidate, oracle] {
        let mut collection = db.collection_mut(name).unwrap();
        edit(&mut collection);
    }
}

fn summary(groups: &[SearchGroup]) -> Groups {
    groups
        .iter()
        .map(|group| {
            (
                group.value.clone(),
                group.hits.iter().map(|hit| hit.id).collect(),
            )
        })
        .collect()
}

fn counts(facets: &[FacetCount]) -> Counts {
    facets
        .iter()
        .map(|facet| (facet.value.clone(), facet.count))
        .collect()
}

/// What a limit leaves of the unlimited groups: the first `groups` groups with their first `per_group` hits; a zero
/// limit leaves nothing.
fn limited(full: &Groups, groups: usize, per_group: usize) -> Groups {
    if per_group == 0 {
        return Vec::new();
    }
    full.iter()
        .take(groups)
        .map(|(value, ids)| (value.clone(), ids.iter().copied().take(per_group).collect()))
        .collect()
}

fn grouped(db: &Database, name: &str, field: &str, conditions: &[Condition]) -> Vec<SearchGroup> {
    db.collection(name)
        .unwrap()
        .search_grouped(&ORIGIN, 100, 100, field, conditions, METRIC)
        .unwrap()
}

fn faceted(db: &Database, name: &str, field: &str, conditions: &[Condition]) -> Vec<FacetCount> {
    db.collection(name)
        .unwrap()
        .facets(field, conditions)
        .unwrap()
}

fn assert_grouped(
    candidate: &Database,
    oracle: &Database,
    name: &str,
    field: &str,
    conditions: &[Condition],
    want: &Groups,
) -> Vec<SearchGroup> {
    let actual = grouped(candidate, name, field, conditions);
    assert_eq!(
        actual,
        grouped(oracle, name, field, conditions),
        "{name}: resident parity of the groups of {field}"
    );
    assert_eq!(
        summary(&actual),
        *want,
        "{name}: groups of {field} under {conditions:?}"
    );
    actual
}

fn assert_facets(
    candidate: &Database,
    oracle: &Database,
    name: &str,
    field: &str,
    conditions: &[Condition],
    want: &Counts,
) -> Vec<FacetCount> {
    let actual = faceted(candidate, name, field, conditions);
    assert_eq!(
        actual,
        faceted(oracle, name, field, conditions),
        "{name}: resident parity of the facets of {field}"
    );
    assert_eq!(
        counts(&actual),
        *want,
        "{name}: facets of {field} under {conditions:?}"
    );
    actual
}

/// Rows written over the fixture's base rows 0, 3, 10, 20 and `u64::MAX`. The base rows 0, 3, 10 and `MAX` hold the
/// vector `[0.1, -0.0]` and the metadata `flag: true`, `id` (their decimal ID), `integer: i64::MIN`, `number: 0.125` and
/// `place: (0, 1)`; row 3 expires at `FAR`; row 20 holds `[0.25, 0.5]` and no metadata. The vectors below are exact in
/// float16 and float32, so the squared distances from the origin are exact too:
///
/// ```text
///  ID  distance  flag
///   0     0.01   true        (base; the second phase rewrites its metadata)
/// MAX     0.01   true        (base)
///   4   0.0625   -           only `rank`
///   5     0.25   "1"
///  20   0.3125   -           (base, no metadata)
///   6      0.5   false
///   1      1.0   1           integer
///   9      1.0   1.0         float
///   7      2.0   1           integer
///   2      4.0   1.0         float
///  13      5.0   2           integer
///  14      5.0   2.5         float
///  16     6.25   0.0         float
///  17     6.25   -0.0        float, the same group as 0.0
///   8      9.0   true
///  15     16.0   "true"      string, not the boolean
///  19     16.0   -           only `place` = (-0.0, 1)
///  18     18.0   true        deadline FAR
/// ```
///
/// Hidden: 3 (rewritten with a past deadline, so the base row never shows), 11 (past deadline), 10 and 12 (deleted).
/// Every row has `rank` = its ID, except the base rows and row 19.
fn edit_flag_rows(collection: &mut CollectionMut<'_>) {
    for (id, vector, value) in [
        (1, [0.0, 1.0], int(1)),
        (2, [2.0, 0.0], float(1.0)),
        (5, [0.0, 0.5], text("1")),
        (6, [0.5, 0.5], flag(false)),
        (7, [1.0, 1.0], int(1)),
        (8, [0.0, 3.0], flag(true)),
        (9, [1.0, 0.0], float(1.0)),
        (13, [1.0, 2.0], int(2)),
        (14, [2.0, 1.0], float(2.5)),
        (15, [0.0, 4.0], text("true")),
        (16, [0.0, 2.5], float(0.0)),
        (17, [2.5, 0.0], float(-0.0)),
    ] {
        collection
            .put_with_metadata(id, &vector, b"row", &flagged(value, id as i64))
            .unwrap();
    }
    collection
        .put_with_metadata(
            4,
            &[0.25, 0.0],
            b"no flag",
            &Metadata::from([("rank".into(), int(4))]),
        )
        .unwrap();
    collection
        .put_with_metadata(
            19,
            &[4.0, 0.0],
            b"geo only",
            &Metadata::from([("place".into(), geo(-0.0, 1.0))]),
        )
        .unwrap();
    collection
        .put_with_expiry(
            3,
            &[0.0, 0.0],
            b"expired",
            &flagged(flag(true), 3),
            Some(PAST),
        )
        .unwrap();
    collection
        .put_with_expiry(
            11,
            &[0.0, 0.0],
            b"expired",
            &flagged(flag(false), 11),
            Some(PAST),
        )
        .unwrap();
    collection
        .put_with_metadata(12, &[0.0, 0.0], b"deleted", &flagged(text("1"), 12))
        .unwrap();
    assert!(collection.delete(12).unwrap());
    assert!(collection.delete(10).unwrap());
    collection
        .put_with_expiry(
            18,
            &[3.0, 3.0],
            b"far deadline",
            &flagged(flag(true), 18),
            Some(FAR),
        )
        .unwrap();
}

/// The second phase: the metadata update that moves row 0 from the group `true` to the group "1".
fn move_row_zero(collection: &mut CollectionMut<'_>) {
    collection
        .update_metadata(0, &flagged(text("1"), 0))
        .unwrap();
}

/// Groups by `flag` before row 0 moves, in the order of their nearest hit.
fn phase_a() -> Groups {
    vec![
        (flag(true), vec![0, MAX, 8, 18]),
        (text("1"), vec![5]),
        (flag(false), vec![6]),
        (int(1), vec![1, 7]),
        (float(1.0), vec![9, 2]),
        (int(2), vec![13]),
        (float(2.5), vec![14]),
        (float(0.0), vec![16, 17]),
        (text("true"), vec![15]),
    ]
}

fn phase_b() -> Groups {
    vec![
        (text("1"), vec![0, 5]),
        (flag(true), vec![MAX, 8, 18]),
        (flag(false), vec![6]),
        (int(1), vec![1, 7]),
        (float(1.0), vec![9, 2]),
        (int(2), vec![13]),
        (float(2.5), vec![14]),
        (float(0.0), vec![16, 17]),
        (text("true"), vec![15]),
    ]
}

/// Facet counts of `flag`, sorted by type (strings, integers, floats, booleans) and then by value.
fn facets_a() -> Counts {
    vec![
        (text("1"), 1),
        (text("true"), 1),
        (int(1), 2),
        (int(2), 1),
        (float(0.0), 2),
        (float(1.0), 2),
        (float(2.5), 1),
        (flag(false), 1),
        (flag(true), 4),
    ]
}

fn facets_b() -> Counts {
    vec![
        (text("1"), 2),
        (text("true"), 1),
        (int(1), 2),
        (int(2), 1),
        (float(0.0), 2),
        (float(1.0), 2),
        (float(2.5), 1),
        (flag(false), 1),
        (flag(true), 3),
    ]
}

/// The other fields: what the base rows 0 and `MAX` and the geo-only row 19 contribute, as groups and as counts.
fn other_fields(moved: bool) -> Vec<(&'static str, Groups, Counts)> {
    let (id_groups, id_counts, base_ids, base_count, place_ids) = if moved {
        (
            vec![(text(MAX_TEXT), vec![MAX])],
            vec![(text(MAX_TEXT), 1)],
            vec![MAX],
            1,
            vec![MAX, 19],
        )
    } else {
        (
            vec![(text("0"), vec![0]), (text(MAX_TEXT), vec![MAX])],
            vec![(text("0"), 1), (text(MAX_TEXT), 1)],
            vec![0, MAX],
            2,
            vec![0, MAX, 19],
        )
    };
    vec![
        ("id", id_groups, id_counts),
        (
            "integer",
            vec![(int(i64::MIN), base_ids.clone())],
            vec![(int(i64::MIN), base_count)],
        ),
        (
            "number",
            vec![(float(0.125), base_ids)],
            vec![(float(0.125), base_count)],
        ),
        (
            "place",
            vec![(geo(0.0, 1.0), place_ids.clone())],
            vec![(geo(0.0, 1.0), place_ids.len() as u64)],
        ),
        ("absent", Vec::new(), Vec::new()),
    ]
}

/// Squared distances of the rows that carry a `flag` and have an exact vector.
const DISTANCES: [(u64, f64); 13] = [
    (5, 0.25),
    (6, 0.5),
    (1, 1.0),
    (9, 1.0),
    (7, 2.0),
    (2, 4.0),
    (13, 5.0),
    (14, 5.0),
    (16, 6.25),
    (17, 6.25),
    (8, 9.0),
    (15, 16.0),
    (18, 18.0),
];

fn assert_group_shape(groups: &[SearchGroup], what: &str) {
    let nearest: Vec<f64> = groups.iter().map(|group| group.hits[0].distance).collect();
    assert!(
        nearest.windows(2).all(|pair| pair[0] <= pair[1]),
        "{what}: groups follow their nearest hit {nearest:?}"
    );
    for group in groups {
        assert!(
            group
                .hits
                .windows(2)
                .all(|pair| { (pair[0].distance, pair[0].id) < (pair[1].distance, pair[1].id) }),
            "{what}: hits are nearest first and ties go by ID"
        );
        for hit in &group.hits {
            match DISTANCES.iter().find(|(id, _)| *id == hit.id) {
                Some(&(_, distance)) => {
                    assert_eq!(hit.distance, distance, "{what}: distance of {}", hit.id)
                }
                // Rows 0 and `MAX` hold [0.1, -0.0], which float16 and float32 round differently.
                None => assert!(
                    (hit.distance - 0.01).abs() < 1e-3,
                    "{what}: distance of {}",
                    hit.id
                ),
            }
            assert!(
                ![3, 10, 11, 12].contains(&hit.id),
                "{what}: row {} is expired or deleted",
                hit.id
            );
        }
    }
}

/// The zeros are one group (or one count) whose value is that of the first row, which holds +0.0.
fn assert_zero_value(value: &MetadataValue, what: &str) {
    let MetadataValue::Float(value) = value else {
        panic!("{what}: a float group");
    };
    assert!(
        value.is_sign_positive(),
        "{what}: the value of the nearest or first row"
    );
}

fn zero_group(groups: &[SearchGroup]) -> &MetadataValue {
    &groups
        .iter()
        .find(|group| matches!(group.value, MetadataValue::Float(value) if value == 0.0))
        .expect("the group of the zeros")
        .value
}

fn assert_limits(candidate: &Database, oracle: &Database, name: &str, full: &Groups) {
    let actual = candidate.collection(name).unwrap();
    let expected = oracle.collection(name).unwrap();
    for groups in [0, 1, 2, 3, 8, 9, 10, 100] {
        for per_group in [0, 1, 2, 3, 100] {
            let got = actual
                .search_grouped(&ORIGIN, groups, per_group, "flag", &[], METRIC)
                .unwrap();
            assert_eq!(
                got,
                expected
                    .search_grouped(&ORIGIN, groups, per_group, "flag", &[], METRIC)
                    .unwrap(),
                "{name}: resident parity at {groups} groups of {per_group}"
            );
            assert_eq!(
                summary(&got),
                limited(full, groups, per_group),
                "{name}: {groups} groups of {per_group}"
            );
        }
    }
}

#[test]
fn compact_grouped_search_follows_the_nearest_hit_and_the_group_limits() {
    let _gates = gates();
    let (mut candidate, mut oracle) = fixture();
    for name in ["half", "full"] {
        assert_compact_resident_pair(&candidate, &oracle, name);
        both(&mut candidate, &mut oracle, name, edit_flag_rows);

        let groups = assert_grouped(&candidate, &oracle, name, "flag", &[], &phase_a());
        assert_group_shape(&groups, &format!("{name} before the update"));
        assert_zero_value(zero_group(&groups), name);
        assert_limits(&candidate, &oracle, name, &phase_a());
        for (field, want, _) in other_fields(false) {
            assert_grouped(&candidate, &oracle, name, field, &[], &want);
        }

        both(&mut candidate, &mut oracle, name, move_row_zero);
        let groups = assert_grouped(&candidate, &oracle, name, "flag", &[], &phase_b());
        assert_group_shape(&groups, &format!("{name} after the update"));
        assert_zero_value(zero_group(&groups), name);
        assert_limits(&candidate, &oracle, name, &phase_b());
        for (field, want, _) in other_fields(true) {
            assert_grouped(&candidate, &oracle, name, field, &[], &want);
        }
    }
}

#[test]
fn compact_facets_count_typed_values_over_overlay_rows() {
    let _gates = gates();
    let (mut candidate, mut oracle) = fixture();
    // Every visible row with a `rank` counts once; 3 and 11 are expired, 10 and 12 deleted.
    let ranks = |moved: bool| -> Counts {
        let mut ranks = vec![1, 2, 4, 5, 6, 7, 8, 9, 13, 14, 15, 16, 17, 18];
        if moved {
            ranks.insert(0, 0);
        }
        ranks.into_iter().map(|rank| (int(rank), 1)).collect()
    };
    for name in ["half", "full"] {
        assert_compact_resident_pair(&candidate, &oracle, name);
        both(&mut candidate, &mut oracle, name, edit_flag_rows);

        let facets = assert_facets(&candidate, &oracle, name, "flag", &[], &facets_a());
        let zero = facets.iter().find(|facet| {
            facet.count == 2 && matches!(facet.value, MetadataValue::Float(value) if value == 0.0)
        });
        assert_zero_value(&zero.expect("the count of the zeros").value, name);
        assert_facets(&candidate, &oracle, name, "rank", &[], &ranks(false));
        for (field, _, want) in other_fields(false) {
            assert_facets(&candidate, &oracle, name, field, &[], &want);
        }

        both(&mut candidate, &mut oracle, name, move_row_zero);
        assert_facets(&candidate, &oracle, name, "flag", &[], &facets_b());
        assert_facets(&candidate, &oracle, name, "rank", &[], &ranks(true));
        for (field, _, want) in other_fields(true) {
            assert_facets(&candidate, &oracle, name, field, &[], &want);
        }
    }
}

#[test]
fn compact_grouped_search_and_facets_apply_the_and_filter_to_overlay_rows() {
    let _gates = gates();
    let (mut candidate, mut oracle) = fixture();
    let in_range = Condition::Range {
        field: "rank".into(),
        min: Some(int(5)),
        max: Some(int(9)),
    };
    let eq = |value: MetadataValue| Condition::Eq {
        field: "flag".into(),
        value,
    };
    // (conditions, field, groups, counts), worked out from the table of `edit_flag_rows` after row 0 moved:
    // `rank` in 5..=9 keeps the rows 5, 6, 7, 8 and 9; the comparison of `Eq` is strict about the type.
    let cases: Vec<(Vec<Condition>, &str, Groups, Counts)> = vec![
        (
            vec![in_range.clone()],
            "flag",
            vec![
                (text("1"), vec![5]),
                (flag(false), vec![6]),
                (float(1.0), vec![9]),
                (int(1), vec![7]),
                (flag(true), vec![8]),
            ],
            vec![
                (text("1"), 1),
                (int(1), 1),
                (float(1.0), 1),
                (flag(false), 1),
                (flag(true), 1),
            ],
        ),
        (
            vec![in_range.clone(), eq(flag(true))],
            "flag",
            vec![(flag(true), vec![8])],
            vec![(flag(true), 1)],
        ),
        (
            vec![eq(int(1))],
            "flag",
            vec![(int(1), vec![1, 7])],
            vec![(int(1), 2)],
        ),
        (
            vec![eq(int(1))],
            "rank",
            vec![(int(1), vec![1]), (int(7), vec![7])],
            vec![(int(1), 1), (int(7), 1)],
        ),
        (
            vec![eq(flag(true))],
            "rank",
            vec![(int(8), vec![8]), (int(18), vec![18])],
            vec![(int(8), 1), (int(18), 1)],
        ),
        (
            vec![eq(float(1.0))],
            "rank",
            vec![(int(9), vec![9]), (int(2), vec![2])],
            vec![(int(2), 1), (int(9), 1)],
        ),
        (
            vec![eq(text("1"))],
            "rank",
            vec![(int(0), vec![0]), (int(5), vec![5])],
            vec![(int(0), 1), (int(5), 1)],
        ),
        (vec![eq(text("none"))], "flag", Vec::new(), Vec::new()),
        (vec![in_range], "absent", Vec::new(), Vec::new()),
    ];
    for name in ["half", "full"] {
        both(&mut candidate, &mut oracle, name, edit_flag_rows);
        both(&mut candidate, &mut oracle, name, move_row_zero);
        for (conditions, field, groups, want) in &cases {
            assert_grouped(&candidate, &oracle, name, field, conditions, groups);
            assert_facets(&candidate, &oracle, name, field, conditions, want);
        }
    }
}

#[test]
fn compact_grouped_search_reads_every_row_and_honors_the_metric() {
    let _gates = gates();
    let (mut candidate, mut oracle) = fixture();
    let near: Groups = vec![(flag(true), vec![0, 3, 10, MAX]), (flag(false), vec![5])];
    let far_first: Groups = vec![(flag(false), vec![5]), (flag(true), vec![0, 3, 10, MAX])];
    for name in ["half", "full"] {
        // One more row, far away: no depth short of every row reaches it. Rows 0, 3, 10 and `MAX` tie at 0.01 and
        // go by ID; row 20 has no `flag`.
        both(&mut candidate, &mut oracle, name, |collection| {
            collection
                .put_with_metadata(
                    5,
                    &[3.0, 4.0],
                    b"far",
                    &Metadata::from([("flag".into(), flag(false))]),
                )
                .unwrap();
        });
        let actual = candidate.collection(name).unwrap();
        let expected = oracle.collection(name).unwrap();
        // The query [1, 0] puts row 5 at 20 (squared L2), at 0.4 (cosine) and at -3 (inner product): nearest only for
        // the inner product; row 20 (no `flag`) is at 0.8125, 0.553 and -0.25.
        let cases: [([f32; 2], DistanceMetric, usize, usize, Groups); 6] = [
            (ORIGIN, METRIC, 10, 10, near.clone()),
            (
                ORIGIN,
                METRIC,
                10,
                2,
                vec![(flag(true), vec![0, 3]), (flag(false), vec![5])],
            ),
            (
                ORIGIN,
                METRIC,
                1,
                10,
                vec![(flag(true), vec![0, 3, 10, MAX])],
            ),
            ([1.0, 0.0], METRIC, 10, 10, near.clone()),
            (
                [1.0, 0.0],
                DistanceMetric::InnerProduct,
                10,
                10,
                far_first.clone(),
            ),
            ([1.0, 0.0], DistanceMetric::Cosine, 10, 10, near.clone()),
        ];
        for (query, metric, groups, per_group, want) in cases {
            let got = actual
                .search_grouped(&query, groups, per_group, "flag", &[], metric)
                .unwrap();
            assert_eq!(
                got,
                expected
                    .search_grouped(&query, groups, per_group, "flag", &[], metric)
                    .unwrap(),
                "{name}: resident parity for {query:?} under {metric:?}"
            );
            assert_eq!(
                summary(&got),
                want,
                "{name}: {groups} groups of {per_group} for {query:?} under {metric:?}"
            );
        }
    }
}

fn failure<T: std::fmt::Debug>(result: crate::Result<T>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value:?}"),
        Err(error) => format!("{error:?}"),
    }
}

#[test]
fn compact_grouped_search_and_facets_validate_before_materializing_rows() {
    let _gates = gates();
    let (candidate, oracle) = fixture();
    assert_eq!(decode_count(&candidate, "full"), 0);
    let actual = candidate.collection("full").unwrap();
    let expected = oracle.collection("full").unwrap();
    let long = "x".repeat(257);
    let no_bounds = [Condition::Range {
        field: "rank".into(),
        min: None,
        max: None,
    }];
    let field_error = "InvalidQuery(\"group field must be 1..=256 bytes\")";
    let facet_error = "InvalidQuery(\"facet field must be 1..=256 bytes\")";
    let filter_error = "InvalidFilter(\"range requires min or max\")";
    let dimension_error = "DimensionMismatch { expected: 2, actual: 1 }";

    // The field is checked before the query, the query before the filter.
    let check_grouped =
        |query: &[f32], groups: usize, field: &str, conditions: &[Condition], want: &str| {
            let got = failure(actual.search_grouped(query, groups, 5, field, conditions, METRIC));
            assert_eq!(got, want, "grouped search of {query:?} by {field:?}");
            assert_eq!(
                got,
                failure(expected.search_grouped(query, groups, 5, field, conditions, METRIC)),
                "resident parity"
            );
        };
    check_grouped(&ORIGIN, 5, "", &[], field_error);
    check_grouped(&ORIGIN, 5, &long, &[], field_error);
    check_grouped(&[1.0], 5, "", &[], field_error);
    check_grouped(&[1.0], 5, "flag", &[], dimension_error);
    check_grouped(&[1.0], 0, "flag", &[], dimension_error);
    check_grouped(&ORIGIN, 5, "flag", &no_bounds, filter_error);
    check_grouped(&[1.0], 5, "flag", &no_bounds, dimension_error);
    check_grouped(&ORIGIN, 5, "", &no_bounds, field_error);

    let check_facets = |field: &str, conditions: &[Condition], want: &str| {
        let got = failure(actual.facets(field, conditions));
        assert_eq!(got, want, "facets of {field:?}");
        assert_eq!(
            got,
            failure(expected.facets(field, conditions)),
            "resident parity"
        );
    };
    check_facets("", &[], facet_error);
    check_facets(&long, &[], facet_error);
    check_facets("", &no_bounds, facet_error);
    check_facets("flag", &no_bounds, filter_error);
    assert_eq!(
        decode_count(&candidate, "full"),
        0,
        "arguments are validated before any row is decoded"
    );

    // 256 bytes is the longest valid field; no row has it.
    let exact = "x".repeat(256);
    assert!(
        actual
            .search_grouped(&ORIGIN, 5, 5, &exact, &[], METRIC)
            .unwrap()
            .is_empty()
    );
    assert!(actual.facets(&exact, &[]).unwrap().is_empty());
    assert!(expected.facets(&exact, &[]).unwrap().is_empty());
    assert!(
        decode_count(&candidate, "full") > 0,
        "a valid facet call reads every row"
    );

    // A zero limit needs no row either: the hits are found, but none is read.
    let (candidate, oracle) = fixture();
    let near: Groups = vec![(flag(true), vec![0, 3, 10, MAX])];
    for (groups, per_group) in [(0, 5), (5, 0), (0, 0)] {
        for db in [&candidate, &oracle] {
            let got = db
                .collection("full")
                .unwrap()
                .search_grouped(&ORIGIN, groups, per_group, "flag", &[], METRIC)
                .unwrap();
            assert!(got.is_empty(), "{groups} groups of {per_group}");
        }
    }
    assert_eq!(
        decode_count(&candidate, "full"),
        0,
        "a zero limit reads no row"
    );
    let got = grouped(&candidate, "full", "flag", &[]);
    assert_eq!(summary(&got), near);
    assert!(
        decode_count(&candidate, "full") > 0,
        "the hits of a real search are read"
    );
}

#[test]
fn compact_grouped_search_and_facets_check_health_before_anything_else() {
    let _gates = gates();
    let (mut candidate, mut oracle) = fixture();
    let id = candidate.collection("full").unwrap().id();
    let oracle_id = oracle.collection("full").unwrap().id();
    candidate.poisoned = true;
    oracle.poisoned = true;
    let no_bounds = [Condition::Range {
        field: "rank".into(),
        min: None,
        max: None,
    }];
    for (db, id) in [(&candidate, id), (&oracle, oracle_id)] {
        let view = CollectionRef { db, id };
        assert!(matches!(
            view.search_grouped(&[1.0], 0, 0, "", &no_bounds, METRIC),
            Err(Error::Poisoned)
        ));
        assert!(matches!(
            view.search_grouped(&ORIGIN, 5, 5, "flag", &[], METRIC),
            Err(Error::Poisoned)
        ));
        assert!(matches!(view.facets("", &no_bounds), Err(Error::Poisoned)));
        assert!(matches!(view.facets("flag", &[]), Err(Error::Poisoned)));
    }
    assert_eq!(decode_count_in(&candidate, id), 0);
}

#[test]
fn compact_grouped_search_and_facets_return_base_read_errors() {
    let _gates = gates();
    // The base rows 0, 3, 10 and `MAX` hold flag true; row 20 holds no metadata.
    let near: Groups = vec![(flag(true), vec![0, 3, 10, MAX])];
    for name in ["half", "full"] {
        let (healthy, oracle) = fixture();
        let groups = grouped(&healthy, name, "flag", &[]);
        assert_eq!(summary(&groups), near);
        assert_eq!(groups, grouped(&oracle, name, "flag", &[]));
        let facets = faceted(&healthy, name, "flag", &[]);
        assert_eq!(counts(&facets), [(flag(true), 4)]);
        assert_eq!(facets, faceted(&oracle, name, "flag", &[]));

        // Base row 20 cannot be read. It is a hit of the search (without the field) and a row of the facet scan.
        let (broken, _) = fixture();
        poison_cell(&broken, name, 20);
        let view = broken.collection(name).unwrap();
        let before = decode_count(&broken, name);
        for (groups, per_group) in [(0, 5), (5, 0)] {
            let got = view
                .search_grouped(&ORIGIN, groups, per_group, "flag", &[], METRIC)
                .unwrap();
            assert!(got.is_empty(), "{name}: {groups} groups of {per_group}");
        }
        assert_eq!(
            decode_count(&broken, name),
            before,
            "{name}: a zero limit reads no row"
        );
        let result = view.search_grouped(&ORIGIN, 10, 10, "flag", &[], METRIC);
        assert!(
            matches!(&result, Err(Error::InvalidQuery(reason)) if *reason == POISONED),
            "{name}: grouped search must return the read error, not drop the row: {result:?}"
        );
        let result = view.facets("flag", &[]);
        assert!(
            matches!(&result, Err(Error::InvalidQuery(reason)) if *reason == POISONED),
            "{name}: facets must return the read error: {result:?}"
        );
    }
}
