//! P5 step 2: block summaries, visibility bitmaps, conservative pruning and
//! the derived summary block codec.
//!
//! The independent oracle fixture (`tests/fixtures/filter_visibility`,
//! checked by `tests/filter_visibility_oracle.py`) fixes the pruning status
//! of every segment and the surviving candidates; the golden block below
//! was written by hand from the codec specification in the P5 design, with
//! its digest computed separately, before the codec existed.

use super::codec::{self, Bound, KIND_SUMMARY, Row};
use super::*;
use crate::compact::tests::{FLOOR, block, frame};
use crate::compact::{CRITICAL, KIND_END, KIND_TABLE, envelope, skip_report};
use crate::format::{self, FLAG_COMPACT, FLAG_HISTORY, PREFIX_SIZE};
use crate::{Error, Record};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;

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

fn geo(latitude: f64, longitude: f64) -> MetadataValue {
    MetadataValue::GeoPoint {
        latitude,
        longitude,
    }
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

fn radius(field: &str, radius_meters: f64) -> Condition {
    Condition::GeoRadius {
        field: field.into(),
        latitude: 0.0,
        longitude: 0.0,
        radius_meters,
    }
}

fn metadata(fields: &[(&str, MetadataValue)]) -> Metadata {
    fields
        .iter()
        .map(|(field, value)| ((*field).to_owned(), value.clone()))
        .collect()
}

fn record(fields: &[(&str, MetadataValue)]) -> Record {
    Record {
        vector: vec![0.0],
        payload: vec![],
        metadata: metadata(fields),
    }
}

fn covered(fields: &[&str]) -> Arc<[String]> {
    let mut names: Vec<String> = fields.iter().map(|&field| field.to_owned()).collect();
    names.sort();
    names.dedup();
    names.into()
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Keep => "keep",
        Status::Prune => "prune",
        Status::Unknown => "unknown",
    }
}

// ------------------------------------------------------------ oracle fixture

const GOLDEN_FIXTURE: &str = include_str!("../../../tests/fixtures/filter_visibility/golden.json");

/// One fixture row.
struct Fixed {
    id: u64,
    revision: u64,
    deleted: bool,
    expires: Option<u64>,
    metadata: Metadata,
}

fn fixture_value(json: &Value) -> MetadataValue {
    let payload = &json["value"];
    match json["type"].as_str().unwrap() {
        "integer" => int(payload.as_str().unwrap().parse().unwrap()),
        "float" => float(payload.as_f64().unwrap()),
        "boolean" => boolean(payload.as_bool().unwrap()),
        "string" => text(payload.as_str().unwrap()),
        "geopoint" => geo(
            payload["latitude"].as_f64().unwrap(),
            payload["longitude"].as_f64().unwrap(),
        ),
        other => panic!("unsupported fixture type {other}"),
    }
}

fn fixture_condition(json: &Value) -> Condition {
    let field = json["field"].as_str().unwrap().to_owned();
    let bound = |json: &Value| (!json.is_null()).then(|| fixture_value(json));
    match json["op"].as_str().unwrap() {
        "eq" => Condition::Eq {
            field,
            value: fixture_value(&json["value"]),
        },
        "range" => Condition::Range {
            field,
            min: bound(&json["min"]),
            max: bound(&json["max"]),
        },
        "georadius" => Condition::GeoRadius {
            field,
            latitude: json["latitude"].as_f64().unwrap(),
            longitude: json["longitude"].as_f64().unwrap(),
            radius_meters: json["radius_meters"].as_f64().unwrap(),
        },
        other => panic!("unsupported fixture condition {other}"),
    }
}

fn fixture_conditions(json: &Value) -> Vec<Condition> {
    json.as_array()
        .unwrap()
        .iter()
        .map(fixture_condition)
        .collect()
}

fn fixture_rows(json: &Value) -> Vec<Fixed> {
    json.as_array()
        .unwrap()
        .iter()
        .map(|row| Fixed {
            id: row["id"].as_u64().unwrap(),
            revision: row["revision"].as_u64().unwrap(),
            deleted: row["deleted"].as_bool().unwrap(),
            expires: row["expires_at"].as_u64(),
            metadata: row["metadata"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(field, value)| (field.clone(), fixture_value(value)))
                .collect(),
        })
        .collect()
}

/// Type order of `FieldSummary::counts`.
const TYPES: [&str; 5] = ["string", "integer", "float", "boolean", "geopoint"];

/// The derived summary of a complete-coverage segment is exactly the
/// fixture's claim: per type, the inclusive interval or the dictionary,
/// after signed-zero normalization, and no other type.
fn assert_described(summary: &BlockSummary, claim: &Value, context: &str) {
    let claimed = claim["fields"].as_object().unwrap();
    for (name, field) in summary.names.iter().zip(&summary.fields) {
        let entries = claimed
            .get(name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut expected = FieldSummary {
            counts: field.counts,
            ..FieldSummary::default()
        };
        for entry in &entries {
            let values = || entry["values"].as_array().unwrap().iter();
            let integer = |json: &Value| json.as_str().unwrap().parse::<i64>().unwrap();
            let real = |json: &Value| zero(json.as_f64().unwrap());
            match entry["type"].as_str().unwrap() {
                "integer" => expected.ints = Some((integer(&entry["min"]), integer(&entry["max"]))),
                "float" => expected.floats = Some((real(&entry["min"]), real(&entry["max"]))),
                "boolean" => {
                    for value in values() {
                        expected.bools |= 1 << u8::from(value.as_bool().unwrap());
                    }
                }
                "string" => {
                    expected.strings = Dictionary::Values(
                        values().map(|v| v.as_str().unwrap().to_owned()).collect(),
                    );
                }
                "geopoint" => {
                    expected.points = Dictionary::Values(
                        values()
                            .map(|v| {
                                Point::new(
                                    v["latitude"].as_f64().unwrap(),
                                    v["longitude"].as_f64().unwrap(),
                                )
                            })
                            .collect(),
                    );
                }
                other => panic!("unsupported summary type {other}"),
            }
        }
        assert_eq!(*field, expected, "{context}: field {name}");
        for (tag, count) in TYPES.iter().zip(field.counts) {
            let listed = entries.iter().any(|entry| entry["type"] == *tag);
            assert_eq!(count > 0, listed, "{context}: field {name} type {tag}");
        }
    }
}

fn statuses(summaries: &[(String, BlockSummary)], conditions: &[Condition]) -> Value {
    summaries
        .iter()
        .map(|(name, summary)| {
            json!({"segment": name, "status": status_name(summary.status(conditions))})
        })
        .collect()
}

fn oracle_case(case: &Value) {
    let name = case["name"].as_str().unwrap();
    let segments: Vec<(String, String, Vec<Fixed>, &Value)> = case["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|segment| {
            (
                segment["name"].as_str().unwrap().to_owned(),
                segment["summary"]["coverage"].as_str().unwrap().to_owned(),
                fixture_rows(&segment["rows"]),
                &segment["summary"],
            )
        })
        .collect();
    let overlay = fixture_rows(&case["overlay"]);
    let queries = case["queries"].as_array().unwrap();
    let probes = case["pruning_probes"].as_array().unwrap();

    // The latest revision of every ID decides its visibility.
    let mut latest: BTreeMap<u64, &Fixed> = BTreeMap::new();
    for row in segments
        .iter()
        .flat_map(|(_, _, rows, _)| rows)
        .chain(&overlay)
    {
        if latest
            .get(&row.id)
            .is_none_or(|current| row.revision > current.revision)
        {
            latest.insert(row.id, row);
        }
    }

    // A complete summary covers every stored and queried field; a stale or
    // unknown one covers none, so it can never prune.
    let mut fields: Vec<&str> = segments
        .iter()
        .flat_map(|(_, _, rows, _)| rows)
        .chain(&overlay)
        .flat_map(|row| row.metadata.keys().map(String::as_str))
        .collect();
    let all_conditions: Vec<Condition> = queries
        .iter()
        .chain(probes)
        .flat_map(|query| fixture_conditions(&query["conditions"]))
        .collect();
    fields.extend(all_conditions.iter().map(field_of));
    let complete = covered(&fields);
    let none = covered(&[]);

    let mut view = View::default();
    let mut summaries = Vec::new();
    for (segment, coverage, rows, claim) in &segments {
        let context = format!("{name}/{segment}");
        let names = if coverage == "complete" {
            &complete
        } else {
            &none
        };
        let summary = BlockSummary::build(names, rows.iter().map(|row| (row.id, &row.metadata)));
        if coverage == "complete" {
            assert_described(&summary, claim, &context);
        }
        let blocks = if rows.is_empty() {
            vec![]
        } else {
            vec![summary.clone()]
        };
        let mut generation = Segment::new(rows.iter().map(|row| row.id).collect(), blocks);
        for (ordinal, row) in rows.iter().enumerate() {
            if row.deleted || latest[&row.id].revision != row.revision {
                assert!(generation.supersede_row(ordinal), "{context}");
            }
        }
        view.segments.push(generation);
        summaries.push((segment.clone(), summary));
    }
    view.overlay = latest
        .values()
        .filter(|row| {
            !row.deleted
                && overlay
                    .iter()
                    .any(|item| item.id == row.id && item.revision == row.revision)
        })
        .map(|row| row.id)
        .collect();

    for query in queries {
        let context = format!("{name}/{}", query["name"].as_str().unwrap());
        let conditions = fixture_conditions(&query["conditions"]);
        let clock = query["clock"].as_u64().unwrap();
        let expected = &query["expected"];
        assert_eq!(
            statuses(&summaries, &conditions),
            expected["pruning"],
            "{context}"
        );
        let visible: BTreeMap<u64, &Fixed> = latest
            .iter()
            .filter(|(_, row)| !row.deleted && row.expires.is_none_or(|expiry| expiry > clock))
            .map(|(&id, &row)| (id, row))
            .collect();
        let visible_ids: Vec<u64> = visible.keys().copied().collect();
        assert_eq!(json!(visible_ids), expected["visible_ids"], "{context}");
        let (candidates, _) = view.candidates(&conditions, usize::MAX).unwrap();
        let candidates: Vec<u64> = candidates
            .into_iter()
            .filter(|id| visible.contains_key(id))
            .collect();
        assert_eq!(json!(candidates), expected["candidate_ids"], "{context}");
        let matching: Vec<u64> = visible
            .iter()
            .filter(|(_, row)| crate::metadata::matches(&row.metadata, &conditions))
            .map(|(&id, _)| id)
            .collect();
        assert_eq!(json!(matching), expected["matching_ids"], "{context}");
        let rechecked: Vec<u64> = candidates
            .iter()
            .copied()
            .filter(|id| crate::metadata::matches(&visible[id].metadata, &conditions))
            .collect();
        assert_eq!(rechecked, matching, "{context}");
    }
    for probe in probes {
        let context = format!("{name}/{}", probe["name"].as_str().unwrap());
        let conditions = fixture_conditions(&probe["conditions"]);
        assert_eq!(
            statuses(&summaries, &conditions),
            probe["expected"],
            "{context}"
        );
    }
}

/// All six oracle cases: typed boundaries, supersets, latest overlay and
/// TTL, geo as unknown, mutable-only and the empty snapshot.
#[test]
fn oracle_fixture_cases_prune_and_recheck_as_the_oracle_does() {
    let document: Value = serde_json::from_str(GOLDEN_FIXTURE).unwrap();
    assert_eq!(document["schema"], "vlite.filter-visibility-golden");
    assert_eq!(document["version"], 1);
    let cases = document["cases"].as_array().unwrap();
    let names: Vec<&str> = cases
        .iter()
        .map(|case| case["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "typed-boundaries",
            "summary-supersets",
            "latest-overlay-and-ttl",
            "geo-pruning-only",
            "mutable-only",
            "empty-snapshot",
        ]
    );
    for case in cases {
        oracle_case(case);
    }
}

// ------------------------------------------------------------ summaries

#[test]
fn typed_statuses_follow_filter_semantics() {
    use Status::{Keep, Prune, Unknown};
    let names = covered(&["b", "g", "mix", "n", "s"]);
    let rows = [
        metadata(&[
            ("n", int(i64::MIN)),
            ("s", text("café")),
            ("b", boolean(true)),
            ("g", geo(-0.0, 10.0)),
        ]),
        metadata(&[("n", int(i64::MAX)), ("mix", float(-0.0))]),
        metadata(&[("n", float(-2.5)), ("mix", float(7.25))]),
        metadata(&[("s", text("zebra")), ("free", int(1))]),
    ];
    let block = BlockSummary::build(&names, (5..).zip(&rows));
    assert_eq!((block.first, block.last, block.rows), (5, 8, 4));
    let cases = [
        (vec![eq("n", int(i64::MIN))], Keep),
        (vec![eq("n", int(i64::MAX))], Keep),
        // An interval is a superset: the hole between the extremes is kept.
        (vec![eq("n", int(0))], Keep),
        (vec![range("n", None, Some(int(i64::MIN)))], Keep),
        (vec![range("n", Some(int(i64::MAX)), None)], Keep),
        (vec![eq("n", float(-2.5))], Keep),
        (vec![eq("n", float(-2.4))], Prune),
        (vec![range("n", Some(float(-2.5)), Some(float(-2.5)))], Keep),
        (vec![range("n", Some(float(-2.4999)), None)], Prune),
        (vec![range("n", None, Some(float(-2.500_000_1)))], Prune),
        (vec![eq("n", text("1"))], Prune),
        (vec![eq("n", boolean(true))], Prune),
        (vec![eq("mix", float(0.0))], Keep),
        (vec![eq("mix", float(-0.0))], Keep),
        (
            vec![range("mix", Some(float(-0.0)), Some(float(-0.0)))],
            Keep,
        ),
        (vec![range("mix", Some(float(7.25)), None)], Keep),
        (vec![range("mix", Some(float(7.26)), None)], Prune),
        (vec![range("mix", None, Some(float(-1e-300)))], Prune),
        // An integer bound never matches a float.
        (vec![range("mix", Some(int(0)), None)], Prune),
        (vec![eq("s", text("café"))], Keep),
        (vec![eq("s", text("cafe"))], Prune),
        // Decomposed: other bytes, so another value.
        (vec![eq("s", text("cafe\u{301}"))], Prune),
        (vec![eq("b", boolean(true))], Keep),
        (vec![eq("b", boolean(false))], Prune),
        (vec![eq("g", geo(0.0, 10.0))], Keep),
        (vec![eq("g", geo(-0.0, 10.0))], Keep),
        (vec![eq("g", geo(0.0, -10.0))], Prune),
        // A radius never prunes, even a field without points.
        (vec![radius("g", 0.0)], Unknown),
        (vec![radius("n", 1.0)], Unknown),
        // No coverage, no answer.
        (vec![eq("free", int(1))], Unknown),
        (vec![eq("free", int(2))], Unknown),
        // Shapes validation rejects never prune either.
        (vec![range("n", None, None)], Unknown),
        (vec![range("n", Some(int(0)), Some(float(1.0)))], Unknown),
        // A conjunction prunes when any conjunct does.
        (vec![eq("free", int(2)), eq("s", text("nope"))], Prune),
        (vec![eq("free", int(2)), eq("s", text("zebra"))], Unknown),
        (vec![radius("g", 5.0), eq("b", boolean(false))], Prune),
        (vec![], Keep),
    ];
    for (conditions, expected) in cases {
        assert_eq!(block.status(&conditions), expected, "{conditions:?}");
        if expected == Prune {
            assert!(
                rows.iter()
                    .all(|row| !crate::metadata::matches(row, &conditions)),
                "{conditions:?}"
            );
        }
    }
    let empty = BlockSummary::build(&names, std::iter::empty());
    assert_eq!((empty.first, empty.last, empty.rows), (0, 0, 0));
    assert_eq!(empty.status(&[]), Prune);
    assert_eq!(empty.status(&[radius("g", 1.0)]), Prune);
}

#[test]
fn dictionaries_overflow_to_possible_membership() {
    let names = covered(&["g", "s"]);
    let many: Vec<Metadata> = (0..=MAX_DICTIONARY_VALUES)
        .map(|index| {
            metadata(&[
                ("s", text(&format!("v{index:02}"))),
                ("g", geo(index as f64, 0.0)),
            ])
        })
        .collect();
    let block = BlockSummary::build(&names, (0..).zip(&many));
    assert_eq!(block.fields[0].points, Dictionary::Overflow);
    assert_eq!(block.fields[1].strings, Dictionary::Overflow);
    assert_eq!(block.status(&[eq("s", text("absent"))]), Status::Keep);
    assert_eq!(block.status(&[eq("g", geo(-1.0, 0.0))]), Status::Keep);

    let exact = BlockSummary::build(&names, (0..).zip(&many[..MAX_DICTIONARY_VALUES]));
    assert!(
        matches!(&exact.fields[1].strings, Dictionary::Values(values) if values.len() == MAX_DICTIONARY_VALUES)
    );
    assert_eq!(exact.status(&[eq("s", text("absent"))]), Status::Prune);
    assert_eq!(exact.status(&[eq("s", text("v31"))]), Status::Keep);
    assert_eq!(exact.status(&[eq("g", geo(-1.0, 0.0))]), Status::Prune);

    // At most 1024 value bytes: 512 + 512 stay exact, 512 + 513 do not.
    let long = |values: &[String]| {
        let rows: Vec<Metadata> = values
            .iter()
            .map(|value| metadata(&[("s", text(value))]))
            .collect();
        BlockSummary::build(&names, (0..).zip(&rows)).fields[1]
            .strings
            .clone()
    };
    let wide = "é".repeat(256);
    assert!(matches!(
        long(&[wide.clone(), "x".repeat(512)]),
        Dictionary::Values(values) if values.len() == 2
    ));
    assert_eq!(long(&[wide, "y".repeat(513)]), Dictionary::Overflow);
}

/// xorshift64: deterministic and dependency-free.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[test]
fn bitmaps_agree_with_a_set() {
    let mut random = Random(0x2545_F491_4F6C_DD1D);
    for universe in [1usize, 63, 64, 65, 1000, 4097] {
        let mut bitmap = Bitmap::new(universe);
        let mut set = BTreeSet::new();
        for _ in 0..universe * 2 {
            let bit = random.below(universe as u64) as usize;
            assert_eq!(bitmap.insert(bit), set.insert(bit), "{universe} {bit}");
        }
        for bit in 0..universe {
            assert_eq!(bitmap.contains(bit), set.contains(&bit), "{universe} {bit}");
        }
    }
}

#[test]
fn candidates_need_coverage_a_pruned_block_and_room_under_the_cap() {
    let records: RecordMap = (0..4500u64)
        .map(|id| {
            let block = (id / BLOCK_ROWS as u64) as i64;
            (id, record(&[("n", int(block)), ("free", int(id as i64))]))
        })
        .collect();
    let summaries = Summaries::new(["n".to_owned(), "s".to_owned()]);
    let all = usize::MAX;
    // Filters that cannot prune never derive.
    assert_eq!(
        summaries.candidates(&[eq("free", int(1))], &records, all),
        None
    );
    assert_eq!(
        summaries.candidates(&[radius("n", 1.0)], &records, all),
        None
    );
    assert_eq!(summaries.candidates(&[], &records, all), None);
    assert!(summaries.state.get().is_none());

    let ids = |conditions: &[Condition], cap| summaries.candidates(conditions, &records, cap);
    assert_eq!(ids(&[eq("n", int(2))], all), Some((2048..3072).collect()));
    assert!(summaries.state.get().is_some());
    assert_eq!(
        ids(&[range("n", Some(int(4)), None)], all),
        Some((4096..4500).collect())
    );
    assert_eq!(
        ids(&[eq("n", int(1)), eq("free", int(1500))], all),
        Some((1024..2048).collect())
    );
    // Nothing pruned: the full scan is no worse.
    let everything = range("n", Some(int(0)), None);
    assert_eq!(ids(std::slice::from_ref(&everything), all), None);
    assert_eq!(ids(&[everything, radius("n", 1.0)], all), None);
    assert_eq!(
        ids(&[eq("n", int(0)), radius("n", 1.0)], all),
        Some((0..1024).collect())
    );
    // Every block pruned: nothing to scan.
    assert_eq!(ids(&[eq("n", int(9))], 0), Some(BTreeSet::new()));
    assert_eq!(ids(&[eq("s", text("x"))], 0), Some(BTreeSet::new()));
    // Over the cap: the caller scans everything.
    let tail = [range("n", Some(int(3)), None)];
    assert_eq!(ids(&tail, 1427), None);
    assert_eq!(ids(&tail, 1428).map(|ids| ids.len()), Some(1428));
    // Nothing declared: no summaries.
    assert_eq!(
        Summaries::default().candidates(&[eq("n", int(2))], &records, all),
        None
    );
}

/// Applies a committed change as `Collection::apply_committed` does:
/// summaries observe the previous record first.
fn apply(summaries: &mut Summaries, records: &mut RecordMap, id: u64, value: Option<Metadata>) {
    let previous = records.get(&id).map(|record| record.metadata.clone());
    summaries.track(id, previous.as_ref(), value.as_ref());
    match value {
        Some(metadata) => {
            records.insert(
                id,
                Record {
                    vector: vec![0.0],
                    payload: vec![],
                    metadata,
                },
            );
        }
        None => {
            records.remove(&id);
        }
    }
}

/// Candidates, checked to contain every matching record.
fn superset(
    summaries: &Summaries,
    records: &RecordMap,
    conditions: &[Condition],
) -> Option<BTreeSet<u64>> {
    let ids = summaries.candidates(conditions, records, usize::MAX);
    if let Some(ids) = &ids {
        for (&id, record) in records {
            if crate::metadata::matches(&record.metadata, conditions) {
                assert!(ids.contains(&id), "{conditions:?} lost {id}");
            }
        }
    }
    ids
}

fn ready(summaries: &Summaries) -> &View {
    match summaries.state.get() {
        Some(State::Ready(view)) => view,
        other => panic!("expected ready summaries, got {other:?}"),
    }
}

fn churn(summaries: &Summaries) -> (usize, usize) {
    let view = ready(summaries);
    let superseded = view
        .segments
        .iter()
        .map(|segment| segment.superseded_rows)
        .sum();
    (superseded, view.overlay.len())
}

#[test]
fn tracking_supersedes_rows_and_overlays_latest_revisions() {
    let mut records: RecordMap = (0..3000u64)
        .map(|id| (id, record(&[("n", int(id as i64))])))
        .collect();
    records.insert(2999, record(&[("n", float(0.0))]));
    let mut summaries = Summaries::new(["n".to_owned()]);
    let n = |value| Some(metadata(&[("n", value)]));

    // Before the first covered query, nothing is derived or tracked.
    apply(&mut summaries, &mut records, 5, n(int(2500)));
    assert!(summaries.state.get().is_none());
    let ids = superset(&summaries, &records, &[eq("n", int(2500))]).unwrap();
    assert_eq!(ids, (0..1024).chain(2048..3000).collect());
    assert_eq!(churn(&summaries), (0, 0));

    // An edit out of its block's range: the row is superseded, the latest
    // revision overlaid.
    apply(&mut summaries, &mut records, 10, n(int(5000)));
    assert_eq!(churn(&summaries), (1, 1));
    let five_thousand = [eq("n", int(5000))];
    assert_eq!(
        superset(&summaries, &records, &five_thousand),
        Some([10].into())
    );

    // Delete, then reinsert with another value. Block 0 now spans 0..=2500.
    apply(&mut summaries, &mut records, 2048, None);
    assert_eq!(churn(&summaries), (2, 1));
    let ids = superset(
        &summaries,
        &records,
        &[range("n", Some(int(2600)), Some(int(2600)))],
    );
    assert_eq!(ids, Some((2049..3000).chain([10]).collect()));
    apply(&mut summaries, &mut records, 2048, n(int(7)));
    assert_eq!(churn(&summaries), (2, 2));
    let ids = superset(&summaries, &records, &[eq("n", int(7))]).unwrap();
    assert!(ids.contains(&2048) && ids.contains(&7) && !ids.contains(&1024));

    // An edit of uncovered fields only, and signed zero, change nothing.
    apply(
        &mut summaries,
        &mut records,
        20,
        Some(metadata(&[("n", int(20)), ("other", int(1))])),
    );
    apply(&mut summaries, &mut records, 2999, n(float(-0.0)));
    assert_eq!(churn(&summaries), (2, 2));
    let ids = superset(&summaries, &records, &[eq("n", float(-0.0))]).unwrap();
    assert_eq!(ids, (2049..3000).chain([10, 2048]).collect());
    assert!(
        superset(&summaries, &records, &[eq("n", int(20))])
            .unwrap()
            .contains(&20)
    );

    // Deleting an overlaid row removes it from the overlay.
    apply(&mut summaries, &mut records, 10, None);
    assert_eq!(churn(&summaries), (2, 1));
    assert_eq!(
        superset(&summaries, &records, &five_thousand),
        Some([2048].into())
    );

    // New IDs are overlaid until churn passes max(4096, rows / 4); then the
    // summaries are dropped and derived again by the next covered query.
    apply(&mut summaries, &mut records, 10_000, n(int(1)));
    assert!(
        superset(&summaries, &records, &[eq("n", int(1))])
            .unwrap()
            .contains(&10_000)
    );
    let mut added = 0;
    while summaries.state.get().is_some() {
        apply(&mut summaries, &mut records, 20_000 + added, n(int(-1)));
        added += 1;
        assert!(added <= MIN_REBUILD_CHURN as u64);
    }
    // Two superseded rows and two overlaid IDs were already counted.
    assert_eq!(added as usize + 4, MIN_REBUILD_CHURN + 1);
    let ids = superset(&summaries, &records, &[eq("n", int(5000))]).unwrap();
    assert!(ids.is_empty());
    assert_eq!(churn(&summaries), (0, 0));
    let view = ready(&summaries);
    assert_eq!(view.segments[0].ids.len(), records.len());
}

fn random_metadata(random: &mut Random, id: u64) -> Metadata {
    let mut fields = Metadata::new();
    if random.below(8) != 0 {
        let a = (id / 256) as i64 + random.below(3) as i64 - 1;
        fields.insert("a".into(), int(a));
    }
    fields.insert(
        "b".into(),
        text(&format!("k{}", id / 512 + random.below(2))),
    );
    let c = match random.below(5) {
        0 => Some(float(random.below(9) as f64 / 4.0 - 1.0)),
        1 => Some(float(-0.0)),
        2 => Some(boolean(random.below(2) == 0)),
        3 => Some(geo(random.below(3) as f64, -0.0)),
        _ => None,
    };
    if let Some(c) = c {
        fields.insert("c".into(), c);
    }
    fields.insert("d".into(), int(random.below(4) as i64));
    fields
}

fn random_condition(random: &mut Random) -> Condition {
    let low = random.below(14) as i64 - 1;
    match random.below(9) {
        0 => eq("a", int(low)),
        1 => range("a", Some(int(low)), Some(int(low + random.below(2) as i64))),
        2 => range("a", None, Some(int(low))),
        3 => eq("b", text(&format!("k{}", random.below(8)))),
        4 => eq("c", float(random.below(9) as f64 / 4.0 - 1.0)),
        5 => range("c", Some(float(0.0)), Some(float(0.0))),
        6 => eq("c", geo(random.below(3) as f64, 0.0)),
        7 => radius("c", 1.0),
        _ => eq("d", int(random.below(4) as i64)),
    }
}

#[test]
fn random_edits_keep_candidates_a_superset_of_matches() {
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let mut records: RecordMap = (0..3000)
        .map(|id| {
            let metadata = random_metadata(&mut random, id);
            (
                id,
                Record {
                    vector: vec![0.0],
                    payload: vec![],
                    metadata,
                },
            )
        })
        .collect();
    let mut summaries = Summaries::new(["a", "b", "c"].map(String::from));
    let mut narrowed = 0;
    for step in 0..6000 {
        if step % 3 == 0 {
            let mut conditions = vec![random_condition(&mut random)];
            if random.below(3) == 0 {
                conditions.push(random_condition(&mut random));
            }
            narrowed += usize::from(superset(&summaries, &records, &conditions).is_some());
        } else {
            let id = random.below(3600);
            let value = (random.below(6) != 0).then(|| random_metadata(&mut random, id));
            apply(&mut summaries, &mut records, id, value);
        }
    }
    assert!(narrowed > 100, "{narrowed}");
}

#[test]
fn over_budget_summaries_scan_until_churn_allows_a_retry() {
    let rows = 9000u64;
    let mut records: RecordMap = (0..rows)
        .map(|id| (id, record(&[("n", int(id as i64))])))
        .collect();
    let mut summaries = Summaries::new(["n".to_owned()]);
    let one = [eq("n", int(4097))];
    // Room for the names, IDs, bitmap and lists, but not for one block.
    summaries.budget = summaries.fixed_bytes(rows as usize);
    assert_eq!(summaries.candidates(&one, &records, usize::MAX), None);
    assert!(matches!(
        summaries.state.get(),
        Some(State::OverBudget {
            rows: 9000,
            pending: 0
        })
    ));
    summaries.budget = 1;
    for id in 0..MIN_REBUILD_CHURN as u64 {
        apply(&mut summaries, &mut records, id, None);
    }
    assert!(summaries.state.get().is_some());
    apply(&mut summaries, &mut records, 4096, None);
    assert!(summaries.state.get().is_none());
    assert_eq!(summaries.candidates(&one, &records, usize::MAX), None);
    assert!(matches!(
        summaries.state.get(),
        Some(State::OverBudget { rows: 4903, .. })
    ));

    summaries.budget = BUDGET_BYTES;
    summaries.state = OnceLock::new();
    let ids = summaries.candidates(&one, &records, usize::MAX).unwrap();
    assert_eq!(ids, (4097..4097 + BLOCK_ROWS as u64).collect());
}

// ------------------------------------------------------- memory accounting

/// The lib test binary's allocator: `System`, also counting the net heap
/// bytes (requested sizes, as `Vec` and `String` capacities are) that the
/// current thread allocates while `counted` runs. Other threads, and this
/// one outside `counted`, are not counted.
struct Counting;

thread_local! {
    static LIVE: Cell<Option<isize>> = const { Cell::new(None) };
}

fn count(delta: isize) {
    let _ = LIVE.try_with(|live| {
        if let Some(bytes) = live.get() {
            live.set(Some(bytes + delta));
        }
    });
}

// Every call forwards to `System` with the caller's layout and only adds
// bookkeeping on this thread, so `System`'s contract carries over.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            count(layout.size() as isize);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            count(layout.size() as isize);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        count(-(layout.size() as isize));
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, size) };
        if !moved.is_null() {
            count(size as isize - layout.size() as isize);
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Runs `body` and returns its value with the heap bytes it left allocated
/// on this thread; `live` reads the running count inside `body`.
fn counted<T>(body: impl FnOnce() -> T) -> (T, usize) {
    LIVE.with(|live| assert_eq!(live.replace(Some(0)), None, "nested count"));
    let value = body();
    let bytes = LIVE.with(Cell::take).expect("counting");
    (
        value,
        usize::try_from(bytes).expect("freed more than counted"),
    )
}

fn live() -> usize {
    usize::try_from(LIVE.with(Cell::get).expect("counting")).unwrap()
}

/// A logger keeping the `vlite` records of the threads inside `logged`
/// only, so tests running in parallel never see each other's records. It is
/// installed once for the lib test binary, where nothing else installs one.
mod capture {
    use log::{Level, LevelFilter, Log, Metadata, Record};
    use std::cell::RefCell;
    use std::sync::Once;

    pub type Logged = Vec<(Level, String, String)>;

    thread_local! {
        static RECORDS: RefCell<Option<Logged>> = const { RefCell::new(None) };
    }

    struct Capture;

    impl Log for Capture {
        fn enabled(&self, metadata: &Metadata) -> bool {
            metadata.target().starts_with("vlite")
        }

        fn log(&self, record: &Record) {
            if self.enabled(record.metadata()) {
                RECORDS.with(|records| {
                    if let Some(records) = records.borrow_mut().as_mut() {
                        records.push((
                            record.level(),
                            record.target().to_owned(),
                            record.args().to_string(),
                        ));
                    }
                });
            }
        }

        fn flush(&self) {}
    }

    /// Runs `body` and returns it with what it logged on this thread.
    pub fn logged<T>(body: impl FnOnce() -> T) -> (T, Logged) {
        static INSTALL: Once = Once::new();
        static LOGGER: Capture = Capture;
        INSTALL.call_once(|| {
            log::set_logger(&LOGGER).expect("the only logger in the lib test binary");
            log::set_max_level(LevelFilter::Trace);
        });
        RECORDS.with(|records| *records.borrow_mut() = Some(Vec::new()));
        let value = body();
        let records = RECORDS.with(|records| records.borrow_mut().take());
        (value, records.unwrap_or_default())
    }
}

/// The one warning of summaries over `budget`.
fn budget_warning(rows: usize, bytes: usize, budget: usize) -> capture::Logged {
    vec![(
        log::Level::Warn,
        "vlite::filter".to_owned(),
        format!(
            r#"{{"event":"filter_summary_budget","outcome":"exact_scan","rows":{rows},"min_bytes":{bytes},"budget":{budget}}}"#
        ),
    )]
}

/// New summaries over `names` with the heap bytes they hold (the names).
fn summaries_over(names: &[String]) -> (Summaries, usize) {
    counted(|| Summaries::new(names.to_vec()))
}

/// Derives `rows`, checking that the bytes charged are exactly the heap
/// the summaries hold: the names plus what derivation left allocated.
fn derive_exactly<'a>(
    names: &[String],
    rows: impl ExactSizeIterator<Item = (u64, &'a Metadata)>,
) -> (Summaries, usize) {
    let (summaries, held) = summaries_over(names);
    let count = rows.len();
    let (state, derived) = counted(|| summaries.derive(count, rows));
    let State::Ready(view) = &state else {
        panic!("expected ready summaries, got {state:?}");
    };
    assert_eq!(view.bytes, held + derived, "{names:?} over {count} rows");
    let bytes = view.bytes;
    summaries.state.set(state).unwrap();
    (summaries, bytes)
}

/// `rows` numbered from ID 0.
fn numbered(rows: &[Metadata]) -> impl ExactSizeIterator<Item = (u64, &Metadata)> {
    rows.iter().enumerate().map(|(id, row)| (id as u64, row))
}

/// The review's layout: 16 covered string fields `f00`..`f15`; every
/// 1024-row block opens with 17 rows that set each field to one of 17
/// one-byte values (`A`..`Q` in block 0, `a`..`q` after) and continues
/// with empty rows, so each dictionary keeps 17 strings in room for 32.
struct Dictionaries {
    names: Vec<String>,
    upper: Vec<Metadata>,
    lower: Vec<Metadata>,
    empty: Metadata,
}

impl Dictionaries {
    fn new() -> Self {
        let names: Vec<String> = (0..16).map(|field| format!("f{field:02}")).collect();
        let opening = |first: u8| -> Vec<Metadata> {
            (0..17)
                .map(|offset| {
                    let value = char::from(first + offset).to_string();
                    names
                        .iter()
                        .map(|name| (name.clone(), text(&value)))
                        .collect()
                })
                .collect()
        };
        Self {
            upper: opening(b'A'),
            lower: opening(b'a'),
            names,
            empty: Metadata::new(),
        }
    }

    fn rows(&self, blocks: usize) -> impl ExactSizeIterator<Item = (u64, &Metadata)> + '_ {
        (0..blocks * BLOCK_ROWS).map(move |row| {
            let opening = if row < BLOCK_ROWS {
                &self.upper
            } else {
                &self.lower
            };
            let index = row % BLOCK_ROWS;
            (row as u64, opening.get(index).unwrap_or(&self.empty))
        })
    }
}

/// The bytes charged for summaries are the heap they hold, measured by the
/// allocator: names, IDs, bitmap, block and segment lists, field lists and
/// dictionaries at their capacities, strings at theirs.
#[test]
fn charged_bytes_are_the_measured_summary_heap() {
    let layout = Dictionaries::new();
    for blocks in [1, 2, 3] {
        derive_exactly(&layout.names, layout.rows(blocks));
    }
    // A partial last block, and no rows at all.
    derive_exactly(&layout.names, layout.rows(3).take(2 * BLOCK_ROWS + 5));
    derive_exactly(&layout.names, layout.rows(0));

    // Integers, floats, signed zero, booleans, points and strings.
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let mixed: Vec<Metadata> = (0..2600)
        .map(|id| random_metadata(&mut random, id))
        .collect();
    let names = ["a", "b", "c", "d", "uncovered"].map(String::from);
    derive_exactly(&names, numbered(&mixed));

    // Dictionaries that overflow by count, then by bytes, and fields that
    // no row has.
    let mut overflowing: Vec<Metadata> = (0..=MAX_DICTIONARY_VALUES)
        .map(|index| {
            metadata(&[
                ("s", text(&format!("v{index:02}"))),
                ("g", geo(index as f64, 0.0)),
            ])
        })
        .collect();
    overflowing.resize(BLOCK_ROWS, Metadata::new());
    for length in [600, 500, 7] {
        overflowing.push(metadata(&[("s", text(&"é".repeat(length)))]));
    }
    let names = ["g", "s", "t"].map(String::from);
    derive_exactly(&names, numbered(&overflowing));
    derive_exactly(&[], numbered(&overflowing));
}

/// Review regression: dictionaries partly filled (17 one-byte strings in
/// room for 32) over 16 fields. At the real 64 MiB budget, the review's
/// 3900 blocks (66,705,600 bytes counting initialized strings only, at
/// least 89,169,600 held) and one block more than fits log one warning and
/// fall back to the exact scan; the summaries that fit are derived and
/// prune.
#[test]
fn near_budget_dictionaries_fall_back_to_an_exact_scan() {
    let layout = Dictionaries::new();
    let only_upper = [eq("f00", text("A"))];
    let absent = [eq("f07", text("z"))];
    let records = RecordMap::new();
    // Derives `blocks` at the real budget, expecting the fallback; returns
    // the bytes the warning reports.
    let fall_back = |blocks: usize| {
        let summaries = Summaries::new(layout.names.clone());
        assert_eq!(summaries.budget, 64 << 20);
        let rows = blocks * BLOCK_ROWS;
        let (state, logged) = capture::logged(|| summaries.derive(rows, layout.rows(blocks)));
        let State::OverBudget {
            rows: over,
            pending,
        } = &state
        else {
            panic!("{blocks} blocks: summaries derived within the budget");
        };
        assert_eq!((*over, *pending), (rows, 0));
        let [(_, _, message)] = &logged[..] else {
            panic!("{blocks} blocks: {logged:?}");
        };
        let min_bytes = serde_json::from_str::<Value>(message).unwrap()["min_bytes"]
            .as_u64()
            .unwrap() as usize;
        assert_eq!(logged, budget_warning(rows, min_bytes, BUDGET_BYTES));
        summaries.state.set(state).unwrap();
        assert_eq!(
            summaries.candidates(&only_upper, &records, usize::MAX),
            None
        );
        assert_eq!(summaries.candidates(&absent, &records, usize::MAX), None);
        min_bytes
    };
    let review = fall_back(3900);

    let (_, one) = derive_exactly(&layout.names, layout.rows(1));
    let (_, two) = derive_exactly(&layout.names, layout.rows(2));
    let per_block = two - one;
    let fixed = one - per_block;
    if cfg!(target_pointer_width = "64") {
        // The review's figure: 89,169,600 bytes for 3900 blocks.
        assert_eq!(per_block, 22_864);
    }
    assert!(review > BUDGET_BYTES && review - BUDGET_BYTES <= per_block);

    // The most blocks whose summaries fit, within one block of 64 MiB.
    let fit = (BUDGET_BYTES - fixed) / per_block;
    if cfg!(target_pointer_width = "64") {
        assert_eq!((fixed, fit), (528, 2935));
    }
    let (summaries, bytes) = derive_exactly(&layout.names, layout.rows(fit));
    assert_eq!(bytes, fixed + fit * per_block);
    assert!(bytes <= BUDGET_BYTES && BUDGET_BYTES - bytes < per_block);
    assert_eq!(
        summaries.candidates(&only_upper, &records, usize::MAX),
        Some((0..BLOCK_ROWS as u64).collect())
    );
    assert_eq!(
        summaries.candidates(&absent, &records, usize::MAX),
        Some(BTreeSet::new())
    );
    drop(summaries);

    let over = fall_back(fit + 1);
    assert!(over > BUDGET_BYTES && over - BUDGET_BYTES <= per_block);
}

/// `overlay_bytes` bounds the heap of an overlay, measured by the
/// allocator after every insertion and removal: ascending, descending,
/// random, interleaved and draining.
#[test]
fn overlay_bytes_bound_the_overlay_heap() {
    let mut random = Random(0x2545_F491_4F6C_DD1D);
    let size = 20_000u64;
    let random_ids: Vec<(bool, u64)> = (0..size).map(|_| (true, random.below(4 * size))).collect();
    let interleaved: Vec<(bool, u64)> = (0..4 * size)
        .map(|_| (random.below(3) != 0, random.below(size)))
        .collect();
    let mut drained: Vec<(bool, u64)> = (0..size).map(|id| (true, id)).collect();
    let mut order: Vec<u64> = (0..size).collect();
    for index in (1..order.len()).rev() {
        order.swap(index, random.below(index as u64 + 1) as usize);
    }
    drained.extend(order.into_iter().map(|id| (false, id)));
    let patterns: [Vec<(bool, u64)>; 5] = [
        (0..size).map(|id| (true, id)).collect(),
        (0..size).rev().map(|id| (true, id)).collect(),
        random_ids,
        interleaved,
        drained,
    ];
    for (pattern, changes) in patterns.iter().enumerate() {
        let ((), _) = counted(|| {
            let mut overlay = BTreeSet::new();
            for (step, &(insert, id)) in changes.iter().enumerate() {
                if insert {
                    overlay.insert(id);
                } else {
                    overlay.remove(&id);
                }
                let (held, bound) = (live(), overlay_bytes(overlay.len()));
                assert!(
                    held <= bound,
                    "pattern {pattern} step {step}: {held} > {bound}"
                );
            }
        });
    }
}

/// Summaries count their overlay against the budget: an overlay that would
/// pass it logs the warning and falls back to the exact scan until churn
/// allows another try.
#[test]
fn overlay_growth_past_the_budget_falls_back_to_an_exact_scan() {
    let mut records: RecordMap = (0..3000u64)
        .map(|id| (id, record(&[("n", int(id as i64))])))
        .collect();
    let mut summaries = Summaries::new(["n".to_owned()]);
    let five = [eq("n", int(5))];
    assert_eq!(
        superset(&summaries, &records, &five),
        Some((0..BLOCK_ROWS as u64).collect())
    );
    let base = ready(&summaries).bytes;
    // Room for two overlaid IDs.
    summaries.budget = base + overlay_bytes(2);
    let n = |value| Some(metadata(&[("n", int(value))]));
    apply(&mut summaries, &mut records, 10, n(5000));
    apply(&mut summaries, &mut records, 3000, n(5));
    assert_eq!(churn(&summaries), (1, 2));
    assert_eq!(
        superset(&summaries, &records, &five),
        Some((0..BLOCK_ROWS as u64).chain([3000]).collect())
    );
    // Removing an overlaid ID frees room for another.
    apply(&mut summaries, &mut records, 3000, None);
    apply(&mut summaries, &mut records, 3000, n(5));
    assert_eq!(churn(&summaries), (1, 2));

    let ((), logged) = capture::logged(|| apply(&mut summaries, &mut records, 3001, n(6)));
    let bytes = base + overlay_bytes(3);
    assert_eq!(logged, budget_warning(3000, bytes, summaries.budget));
    assert!(matches!(
        summaries.state.get(),
        Some(State::OverBudget {
            rows: 3000,
            pending: 0
        })
    ));
    assert_eq!(summaries.candidates(&five, &records, usize::MAX), None);

    // Then churn as for summaries over budget at derivation.
    for change in 0..MIN_REBUILD_CHURN as i64 {
        apply(&mut summaries, &mut records, 7, n(-change - 1));
    }
    assert!(summaries.state.get().is_some());
    apply(&mut summaries, &mut records, 7, n(7));
    assert!(summaries.state.get().is_none());
    // Derived again over 3002 rows: the last block now spans 5..=2999.
    let ids = superset(&summaries, &records, &five).unwrap();
    assert_eq!(ids, (0..1024).chain(2048..3002).collect());
    assert_eq!(churn(&summaries), (0, 0));
}

// ------------------------------------------------------------ block codec

/// Golden kind-0x0400 version-1 body for owner 0, partition 1, written by
/// hand from the P5 design: header with digest 0xcf3c3075af3a606c, two
/// section entries, coverage (first 10, last 15, 3 rows, fields flag, n,
/// pos and word) and per-field summaries.
const GOLDEN_HEX: &str = concat!(
    // kind 0x0400, version 1, flags 0 (derived), 2 sections
    "0004 0100 0000 0200",
    // object: owner 0, partition 1
    "0000 0100 0000 0000",
    // source digest
    "6c60 3aaf 7530 3ccf",
    // first section offset, reserved
    "3000 0000 0000 0000",
    // section table: (0x30, 44), (0x5c, 145)
    "3000 0000 2c00 0000 5c00 0000 9100 0000",
    // coverage: first 10, last 15, 3 rows, 4 fields, reserved
    "0a00 0000 0000 0000 0f00 0000 0000 0000 0300 0000 0400 0000",
    // names
    "0400 666c 6167 0100 6e 0300 706f 73 0400 776f 7264",
    // flag: two booleans, both values
    "0000 0000 0000 0000 0000 0000 0200 0000 0000 0000 03",
    // n: two integers -2..i64::MAX, one float 0.0..0.0
    "0000 0000 0200 0000 0100 0000 0000 0000 0000 0000",
    "feff ffff ffff ffff ffff ffff ffff ff7f",
    "0000 0000 0000 0000 0000 0000 0000 0000",
    // pos: one point (0.0, 1.5)
    "0000 0000 0000 0000 0000 0000 0000 0000 0100 0000 0100",
    "0000 0000 0000 0000 0000 0000 0000 f83f",
    // word: two strings, "abc" and "café"
    "0200 0000 0000 0000 0000 0000 0000 0000 0000 0000 0200",
    "0300 6162 63 0500 6361 66c3 a9",
);

const GOLDEN_DIGEST: u64 = 0xcf3c_3075_af3a_606c;
const GOLDEN_ID: u64 = KIND_SUMMARY as u64 | 1 << 16;

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|byte| *byte != b' ').collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn golden_metadata() -> Vec<(u64, u64, Metadata)> {
    vec![
        (
            10,
            1,
            metadata(&[
                ("flag", boolean(true)),
                ("n", int(-2)),
                ("pos", geo(-0.0, 1.5)),
                ("word", text("café")),
            ]),
        ),
        (
            12,
            4,
            metadata(&[
                ("flag", boolean(false)),
                ("n", float(-0.0)),
                ("word", text("abc")),
            ]),
        ),
        (
            15,
            2,
            metadata(&[("extra", text("x")), ("n", int(i64::MAX))]),
        ),
    ]
}

fn golden_names() -> Arc<[String]> {
    covered(&["word", "pos", "n", "flag"])
}

fn rows_of(source: &[(u64, u64, Metadata)]) -> Vec<Row<'_>> {
    source
        .iter()
        .map(|(id, revision, metadata)| Row {
            id: *id,
            revision: *revision,
            metadata,
        })
        .collect()
}

fn built(names: &Arc<[String]>, rows: &[Row<'_>]) -> BlockSummary {
    BlockSummary::build(names, rows.iter().map(|row| (row.id, row.metadata)))
}

fn corrupt_reason<T: std::fmt::Debug>(result: crate::Result<T>) -> &'static str {
    match result {
        Err(Error::Corrupt { reason, .. }) => reason,
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn golden_summary_block_binds_and_encodes_byte_for_byte() {
    let body = hex(GOLDEN_HEX);
    assert_eq!(body.len(), 237);
    let names = golden_names();
    let source = golden_metadata();
    let rows = rows_of(&source);
    assert_eq!(codec::digest(&names, &rows), GOLDEN_DIGEST);
    let expected = built(&names, &rows);
    assert_eq!(codec::encode(0, 1, &expected, GOLDEN_DIGEST), body);
    match codec::bind(0, GOLDEN_ID, 3, &body, &names, &rows, 0).unwrap() {
        Bound::Fresh(summary) => {
            assert_eq!(summary, expected);
            assert!(Arc::ptr_eq(&summary.names, &names));
            assert_eq!(summary.status(&[eq("word", text("café"))]), Status::Keep);
            assert_eq!(summary.status(&[eq("pos", geo(0.0, 1.5))]), Status::Keep);
            assert_eq!(summary.status(&[eq("n", float(0.0))]), Status::Keep);
            assert_eq!(summary.status(&[eq("n", float(0.5))]), Status::Prune);
            assert_eq!(summary.status(&[eq("extra", text("x"))]), Status::Unknown);
        }
        other => panic!("expected a fresh summary, got {other:?}"),
    }
    let adopted = codec::adopt_or_rebuild(0, GOLDEN_ID, 3, &body, &names, &rows, 0).unwrap();
    assert_eq!(adopted, expected);
}

#[test]
fn summary_blocks_round_trip_at_every_size_class() {
    let names = covered(&["a", "b", "c", "d"]);
    for size in [1u64, 2, 31, 33, 200, 1024] {
        let source: Vec<(u64, u64, Metadata)> = (0..size)
            .map(|row| {
                let id = 1000 + row * 3;
                let mut fields = metadata(&[
                    ("a", int(row as i64 * 3 - 7)),
                    ("c", text(&format!("s{}", row % 40))),
                ]);
                if row % 2 == 0 {
                    fields.insert("b".into(), float(row as f64 / 8.0 - 2.0));
                }
                let d = match row % 3 {
                    0 => geo(row as f64 % 90.0, -(row as f64 % 180.0)),
                    1 => boolean(row % 4 == 1),
                    _ => int(i64::MIN + row as i64),
                };
                fields.insert("d".into(), d);
                (id, row + 1, fields)
            })
            .collect();
        let rows = rows_of(&source);
        let summary = built(&names, &rows);
        let digest = codec::digest(&names, &rows);
        let body = codec::encode(3, size, &summary, digest);
        let id = KIND_SUMMARY as u64 | size << 16;
        match codec::bind(3, id, size as u32, &body, &names, &rows, 0).unwrap() {
            Bound::Fresh(decoded) => assert_eq!(decoded, summary, "{size}"),
            other => panic!("{size}: expected a fresh summary, got {other:?}"),
        }
        let overflowed = size > MAX_DICTIONARY_VALUES as u64;
        assert_eq!(
            summary.fields[2].strings == Dictionary::Overflow,
            overflowed,
            "{size}"
        );
    }
}

/// A summary body for owner 0, partition 1 with `sections`, flags cleared
/// and the golden digest.
fn body(sections: &[&[u8]]) -> Vec<u8> {
    let mut body = envelope(KIND_SUMMARY, 0, 1, sections, None);
    body[4..6].copy_from_slice(&0u16.to_le_bytes());
    body[16..24].copy_from_slice(&GOLDEN_DIGEST.to_le_bytes());
    body
}

/// Little-endian byte builder.
#[derive(Clone, Default)]
struct Bytes(Vec<u8>);

impl Bytes {
    fn u8(mut self, value: u8) -> Self {
        self.0.push(value);
        self
    }

    fn u16(mut self, value: u16) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn f64(self, value: f64) -> Self {
        self.u64(value.to_bits())
    }

    fn raw(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    fn name(self, name: &[u8]) -> Self {
        self.u16(name.len() as u16).raw(name)
    }

    fn counts(self, counts: [u32; 5]) -> Self {
        counts.into_iter().fold(self, Self::u32)
    }
}

/// Coverage of one field `n` over 3 rows, IDs 10 to 15.
fn head() -> Bytes {
    Bytes::default()
        .u64(10)
        .u64(15)
        .u32(3)
        .u16(1)
        .u16(0)
        .name(b"n")
}

#[test]
fn malformed_summary_blocks_are_corrupt() {
    let golden = hex(GOLDEN_HEX);
    let names = golden_names();
    let source = golden_metadata();
    let rows = rows_of(&source);
    let bind = |collection: u16, id: u64, count: u32, body: &[u8]| {
        codec::bind(collection, id, count, body, &names, &rows, 7)
    };
    for end in 0..golden.len() {
        let reason = corrupt_reason(bind(0, GOLDEN_ID, 3, &golden[..end]));
        if end < 40 {
            assert_eq!(reason, "invalid compact block length", "{end}");
        }
    }
    let mut extended = golden.clone();
    extended.push(0);
    assert_eq!(
        corrupt_reason(bind(0, GOLDEN_ID, 3, &extended)),
        "invalid compact block sections"
    );
    // Frame identity: kind, owner and partition.
    assert_eq!(
        corrupt_reason(bind(0, GOLDEN_ID + 1, 3, &golden)),
        "invalid filter summary block"
    );
    assert_eq!(
        corrupt_reason(bind(1, GOLDEN_ID, 3, &golden)),
        "compact block header does not match its frame"
    );
    assert_eq!(
        corrupt_reason(bind(0, KIND_SUMMARY as u64 | 2 << 16, 3, &golden)),
        "compact block header does not match its frame"
    );
    // Header: critical flag (with a digest), version 0.
    for (at, value) in [(4, 1u16), (2, 0)] {
        let mut changed = golden.clone();
        changed[at..at + 2].copy_from_slice(&value.to_le_bytes());
        assert_eq!(
            corrupt_reason(bind(0, GOLDEN_ID, 3, &changed)),
            "invalid compact block header",
            "{at}"
        );
    }
    let mut critical = golden.clone();
    critical[4] = 1;
    critical[16..24].fill(0);
    assert_eq!(
        corrupt_reason(bind(0, GOLDEN_ID, 3, &critical)),
        "invalid filter summary block"
    );
    let (coverage, fields) = (&golden[0x30..0x5c], &golden[0x5c..]);
    let empty: &[u8] = &[];
    for sections in [vec![coverage], vec![coverage, fields, empty]] {
        assert_eq!(
            corrupt_reason(bind(0, GOLDEN_ID, 3, &body(&sections))),
            "invalid filter summary block",
            "{}",
            sections.len()
        );
    }
    assert!(matches!(
        bind(0, GOLDEN_ID, 3, &body(&[coverage, fields])),
        Ok(Bound::Fresh(_))
    ));
    // A frame count that disagrees with the coverage.
    for count in [2, 4] {
        assert_eq!(
            corrupt_reason(bind(0, GOLDEN_ID, count, &golden)),
            "invalid filter summary block"
        );
    }

    // Coverage section.
    let field = Bytes::default()
        .counts([0, 1, 0, 0, 0])
        .u64((-2i64) as u64)
        .u64(4);
    let coverage_cases: Vec<(&str, Bytes)> = vec![
        (
            "no rows",
            Bytes::default()
                .u64(10)
                .u64(15)
                .u32(0)
                .u16(1)
                .u16(0)
                .name(b"n"),
        ),
        (
            "too many rows",
            Bytes::default()
                .u64(10)
                .u64(5000)
                .u32(1025)
                .u16(1)
                .u16(0)
                .name(b"n"),
        ),
        (
            "no fields",
            Bytes::default().u64(10).u64(15).u32(3).u16(0).u16(0),
        ),
        (
            "17 fields",
            (0..17u8).fold(
                Bytes::default().u64(10).u64(15).u32(3).u16(17).u16(0),
                |bytes, index| bytes.name(&[b'a' + index]),
            ),
        ),
        (
            "reserved",
            Bytes::default()
                .u64(10)
                .u64(15)
                .u32(3)
                .u16(1)
                .u16(1)
                .name(b"n"),
        ),
        (
            "empty name",
            Bytes::default()
                .u64(10)
                .u64(15)
                .u32(3)
                .u16(1)
                .u16(0)
                .name(b""),
        ),
        (
            "long name",
            Bytes::default()
                .u64(10)
                .u64(15)
                .u32(3)
                .u16(1)
                .u16(0)
                .name(&[b'n'; 257]),
        ),
        (
            "invalid UTF-8",
            Bytes::default()
                .u64(10)
                .u64(15)
                .u32(3)
                .u16(1)
                .u16(0)
                .name(&[0xff]),
        ),
        (
            "first after last",
            Bytes::default()
                .u64(16)
                .u64(15)
                .u32(3)
                .u16(1)
                .u16(0)
                .name(b"n"),
        ),
        (
            "span below rows",
            Bytes::default()
                .u64(10)
                .u64(11)
                .u32(3)
                .u16(1)
                .u16(0)
                .name(b"n"),
        ),
        ("trailing", head().u8(0)),
        (
            "truncated name",
            Bytes::default().u64(10).u64(15).u32(3).u16(1).u16(0).u16(1),
        ),
        ("short", Bytes::default().u64(10).u64(15).u32(3)),
    ];
    for (label, coverage) in coverage_cases {
        let fields = if label == "17 fields" {
            vec![Bytes::default().counts([0; 5]).0; 17].concat()
        } else if label == "no fields" {
            vec![]
        } else {
            field.0.clone()
        };
        assert_eq!(
            corrupt_reason(bind(0, GOLDEN_ID, 3, &body(&[&coverage.0, &fields]))),
            "invalid filter summary block",
            "{label}"
        );
    }
    let two = |first: &[u8], second: &[u8]| {
        Bytes::default()
            .u64(10)
            .u64(15)
            .u32(3)
            .u16(2)
            .u16(0)
            .name(first)
            .name(second)
    };
    for (label, coverage) in [
        ("unsorted", two(b"n", b"m")),
        ("duplicate", two(b"n", b"n")),
    ] {
        let fields = [field.0.clone(), field.0.clone()].concat();
        assert_eq!(
            corrupt_reason(bind(0, GOLDEN_ID, 3, &body(&[&coverage.0, &fields]))),
            "invalid filter summary block",
            "{label}"
        );
    }

    // Field section, for the one field `n` over 3 rows.
    let counts = |counts| Bytes::default().counts(counts);
    let strings = counts([1, 0, 0, 0, 0]);
    let points = counts([0, 0, 0, 0, 1]);
    let field_cases: Vec<(&str, Bytes)> = vec![
        ("more values than rows", counts([1, 1, 1, 1, 0])),
        ("counts overflow", counts([u32::MAX, 0, 0, 0, 2])),
        ("no strings", strings.clone().u16(0)),
        (
            "strings above count",
            strings.clone().u16(2).name(b"a").name(b"b"),
        ),
        (
            "unsorted strings",
            counts([2, 0, 0, 0, 0]).u16(2).name(b"b").name(b"a"),
        ),
        (
            "duplicate strings",
            counts([2, 0, 0, 0, 0]).u16(2).name(b"a").name(b"a"),
        ),
        ("invalid string", strings.clone().u16(1).name(&[0xc3])),
        ("string bytes", strings.clone().u16(1).name(&[b'x'; 1025])),
        ("integers descend", counts([0, 2, 0, 0, 0]).u64(5).u64(4)),
        ("NaN float", counts([0, 0, 1, 0, 0]).f64(f64::NAN).f64(1.0)),
        (
            "infinite float",
            counts([0, 0, 1, 0, 0]).f64(0.5).f64(f64::INFINITY),
        ),
        ("negative zero", counts([0, 0, 1, 0, 0]).f64(-0.0).f64(1.0)),
        ("floats descend", counts([0, 0, 2, 0, 0]).f64(2.0).f64(1.0)),
        ("no booleans", counts([0, 0, 0, 1, 0]).u8(0)),
        ("unknown boolean", counts([0, 0, 0, 1, 0]).u8(4)),
        ("both from one", counts([0, 0, 0, 1, 0]).u8(3)),
        ("no points", points.clone().u16(0)),
        ("latitude", points.clone().u16(1).f64(90.5).f64(0.0)),
        ("longitude", points.clone().u16(1).f64(0.0).f64(-180.5)),
        ("NaN point", points.clone().u16(1).f64(f64::NAN).f64(0.0)),
        (
            "negative zero point",
            points.clone().u16(1).f64(1.0).f64(-0.0),
        ),
        (
            "unsorted points",
            counts([0, 0, 0, 0, 2])
                .u16(2)
                .f64(1.0)
                .f64(0.0)
                .f64(0.5)
                .f64(0.0),
        ),
        ("overflow below 33 points", points.clone().u16(0xFFFF)),
        ("truncated", counts([0, 1, 0, 0, 0]).u64(1)),
        ("trailing", counts([0, 0, 0, 0, 0]).u8(0)),
    ];
    for (label, field) in field_cases {
        assert_eq!(
            corrupt_reason(bind(0, GOLDEN_ID, 3, &body(&[&head().0, &field.0]))),
            "invalid filter summary block",
            "{label}"
        );
    }
    // The same builders accept a valid field, so each case fails for its
    // own reason. Without a digest match it is stale, never served.
    let valid = counts([1, 1, 0, 1, 0]).u16(0xFFFF).u64(1).u64(1).u8(2);
    assert!(matches!(
        bind(0, GOLDEN_ID, 3, &body(&[&head().0, &valid.0])),
        Ok(Bound::Stale)
    ));
}

#[test]
fn stale_summary_blocks_are_rebuilt_never_served() {
    let golden = hex(GOLDEN_HEX);
    let names = golden_names();
    let source = golden_metadata();
    let stale = |names: &Arc<[String]>, source: &[(u64, u64, Metadata)], body: &[u8]| {
        let rows = rows_of(source);
        let count = u32_at_frame(body);
        assert!(
            matches!(
                codec::bind(0, GOLDEN_ID, count, body, names, &rows, 0),
                Ok(Bound::Stale)
            ),
            "{source:?}"
        );
        let rebuilt = codec::adopt_or_rebuild(0, GOLDEN_ID, count, body, names, &rows, 0).unwrap();
        assert_eq!(rebuilt, built(names, &rows));
    };
    fn edited(
        source: &[(u64, u64, Metadata)],
        row: usize,
        edit: impl FnOnce(&mut (u64, u64, Metadata)),
    ) -> Vec<(u64, u64, Metadata)> {
        let mut source = source.to_vec();
        edit(&mut source[row]);
        source
    }
    stale(&names, &edited(&source, 1, |row| row.1 = 5), &golden);
    let covered_value = edited(&source, 0, |row| {
        row.2.insert("n".into(), int(-3));
    });
    stale(&names, &covered_value, &golden);
    let uncovered_value = edited(&source, 2, |row| {
        row.2.insert("extra".into(), text("y"));
    });
    stale(&names, &uncovered_value, &golden);
    let removed = edited(&source, 0, |row| row.2.retain(|field, _| field != "pos"));
    stale(&names, &removed, &golden);
    stale(&names, &source[..2], &golden);
    stale(
        &covered(&["flag", "n", "pos", "word", "zz"]),
        &source,
        &golden,
    );
    stale(&covered(&["flag", "n", "pos"]), &source, &golden);

    // A block that matches its digest but not the coverage it states.
    let rows = rows_of(&source);
    let digest = codec::digest(&names, &rows);
    let mut lying = built(&names, &rows);
    lying.first = 9;
    stale(&names, &source, &codec::encode(0, 1, &lying, digest));
    let mut lying = built(&names, &rows);
    lying.last = 16;
    stale(&names, &source, &codec::encode(0, 1, &lying, digest));
    let mut lying = built(&names, &rows[..2]);
    lying.last = 15;
    stale(&names, &source, &codec::encode(0, 1, &lying, digest));

    // A summary of other rows, with their own digest.
    let other: Vec<(u64, u64, Metadata)> = source
        .iter()
        .map(|(id, revision, _)| (*id, *revision, metadata(&[("n", int(100))])))
        .collect();
    let other_rows = rows_of(&other);
    let foreign = codec::encode(
        0,
        1,
        &built(&names, &other_rows),
        codec::digest(&names, &other_rows),
    );
    stale(&names, &source, &foreign);
}

/// The coverage row count of a summary body, used as its frame count.
fn u32_at_frame(body: &[u8]) -> u32 {
    let coverage = format::u32_at(body, 32) as usize;
    format::u32_at(body, coverage + 16)
}

#[test]
fn unsupported_versions_are_rebuilt_and_corrupt_blocks_fail() {
    let golden = hex(GOLDEN_HEX);
    let names = golden_names();
    let source = golden_metadata();
    let rows = rows_of(&source);
    for version in [2u16, u16::MAX] {
        let mut newer = golden.clone();
        newer[2..4].copy_from_slice(&version.to_le_bytes());
        assert!(matches!(
            codec::bind(0, GOLDEN_ID, 3, &newer, &names, &rows, 0),
            Ok(Bound::UnsupportedVersion)
        ));
        // A newer version's sections are not read by this build.
        let mut single = body(&[b"anything".as_slice()]);
        single[2..4].copy_from_slice(&version.to_le_bytes());
        assert!(matches!(
            codec::bind(0, GOLDEN_ID, 3, &single, &names, &rows, 0),
            Ok(Bound::UnsupportedVersion)
        ));
        let rebuilt = codec::adopt_or_rebuild(0, GOLDEN_ID, 3, &newer, &names, &rows, 0).unwrap();
        assert_eq!(rebuilt, built(&names, &rows));
    }
    let mut broken = golden.clone();
    let last = broken.len() - 1;
    broken[last] = 0xff;
    assert_eq!(
        corrupt_reason(codec::adopt_or_rebuild(
            0, GOLDEN_ID, 3, &broken, &names, &rows, 9
        )),
        "invalid filter summary block"
    );
    let Err(Error::Corrupt { offset, .. }) =
        codec::adopt_or_rebuild(0, GOLDEN_ID, 3, &broken, &names, &rows, 9)
    else {
        unreachable!()
    };
    assert_eq!(offset, 9);
}

/// A plaintext compact image whose table declares `entries` (kind,
/// version, flags) and holds `blocks` (frame ID, count, body) for owner 0.
fn image(entries: &[(u16, u16, u16)], blocks: &[(u64, u32, Vec<u8>)]) -> Vec<u8> {
    let mut table = Vec::new();
    for &(kind, version, flags) in entries {
        for field in [kind, version, flags, 0] {
            table.extend_from_slice(&field.to_le_bytes());
        }
    }
    let mut frames = vec![
        format::advance_frame(FLOOR),
        block(KIND_TABLE, 1, CRITICAL, 0, 0, entries.len() as u32, &table),
    ];
    for (id, count, body) in blocks {
        frames.push(frame(0, *id, *count, body));
    }
    let mut digest = Sha256::new();
    for frame in &frames {
        digest.update(&frame[..PREFIX_SIZE]);
    }
    let mut end = (frames.len() as u64).to_le_bytes().to_vec();
    end.extend_from_slice(&digest.finalize());
    frames.push(block(KIND_END, 1, CRITICAL, 0, 0, 1, &end));
    let flags = FLAG_HISTORY | FLAG_COMPACT;
    let mut bytes = format::header_v5_flags(2, 0x5EED_0000_0000_0005, 0, flags, None);
    for frame in frames {
        bytes.extend_from_slice(&frame);
    }
    bytes
}

/// Loaders do not dispatch kind 0x0400 yet: a declared, authentic summary
/// block of any version is skipped and reported as an unknown kind, and a
/// table that marks the derived kind critical refuses the image.
#[test]
fn loaders_skip_and_report_summary_blocks() {
    let golden = hex(GOLDEN_HEX);
    // Partitions of one owner and kind run 0, 1, 2, ...
    let mut zeroth = golden.clone();
    zeroth[8..16].copy_from_slice(&0u64.to_le_bytes());
    let mut newer = golden.clone();
    newer[2..4].copy_from_slice(&2u16.to_le_bytes());
    newer[8..16].copy_from_slice(&(2u64 << 16).to_le_bytes());
    let bytes = image(
        &[(KIND_SUMMARY, 1, 0), (KIND_SUMMARY, 2, 0)],
        &[
            (u64::from(KIND_SUMMARY), 3, zeroth),
            (GOLDEN_ID, 3, golden.clone()),
            (KIND_SUMMARY as u64 | 2 << 16, 3, newer),
        ],
    );
    let db = crate::Database::open_from_bytes(bytes, None).unwrap();
    assert_eq!(
        skip_report(db.compact_skipped(), "load").as_deref(),
        Some(concat!(
            r#"{"event":"compact_block_skip","outcome":"unknown_kind","reader":"load","rebuild":"none","blocks":3,"groups":["#,
            r#"{"collection":0,"kind":1024,"version":1,"blocks":2},{"collection":0,"kind":1024,"version":2,"blocks":1}"#,
            r#"],"more_groups":0}"#,
        ))
    );

    let critical = image(
        &[(KIND_SUMMARY, 1, CRITICAL)],
        &[(u64::from(KIND_SUMMARY), 3, hex(GOLDEN_HEX))],
    );
    assert_eq!(
        corrupt_reason(crate::Database::open_from_bytes(critical, None).map(|_| ())),
        "invalid capability table entry"
    );
}
