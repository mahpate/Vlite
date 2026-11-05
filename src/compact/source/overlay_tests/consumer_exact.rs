//! The declared-index candidate cap of the compact exact search (`Database::search_in_at`, compact arm).
//!
//! The oracle is the resident search over the same rows at the same clock: hits, their order and the scan statistics are
//! equal. The expected index candidates are worked out by hand from the rule "a declared posting narrows the scan when it
//! holds at most half of the rows", not read back from the code under test.

use super::{assert_compact_resident_pair, gates, indexed_dispatch_fixture};
use crate::{Condition, Database, DistanceMetric, Metadata, MetadataValue, SearchHit, SearchStats};

fn row(tag: Option<&str>, other: Option<&str>) -> Metadata {
    let mut metadata = Metadata::new();
    if let Some(tag) = tag {
        metadata.insert("tag".into(), MetadataValue::String(tag.into()));
    }
    if let Some(other) = other {
        metadata.insert("other".into(), MetadataValue::String(other.into()));
    }
    metadata
}

fn eq(field: &str, value: &str) -> Condition {
    Condition::Eq {
        field: field.into(),
        value: MetadataValue::String(value.into()),
    }
}

/// Row `id` sits at `[id, 1]`, so its squared distance to the query `[1, 1]` is `(id - 1)^2`.
fn put(db: &mut Database, id: u64, metadata: &Metadata, expiry: Option<u64>) {
    db.collection_mut("indexed")
        .unwrap()
        .put_with_expiry(id, &[id as f32, 1.0], b"", metadata, expiry)
        .unwrap();
}

fn search(
    db: &Database,
    k: usize,
    conditions: &[Condition],
    now_ms: u64,
) -> (Vec<SearchHit>, SearchStats) {
    let id = db.collection("indexed").unwrap().id();
    let mut stats = SearchStats::default();
    let hits = db
        .search_in_at(
            id,
            &[1.0, 1.0],
            k,
            conditions,
            DistanceMetric::SquaredL2,
            now_ms,
            &mut stats,
        )
        .unwrap();
    (hits, stats)
}

/// One query on both databases: equal results, the expected ids and the expected index candidates.
#[allow(clippy::too_many_arguments)]
fn check(
    candidate: &Database,
    oracle: &Database,
    case: &str,
    k: usize,
    conditions: &[Condition],
    now_ms: u64,
    ids: &[u64],
    index_candidates: u64,
) {
    let actual = search(candidate, k, conditions, now_ms);
    assert_eq!(actual, search(oracle, k, conditions, now_ms), "{case}");
    assert_eq!(
        actual.0.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        ids,
        "{case}: hits"
    );
    assert_eq!(
        actual.1.index_candidates, index_candidates,
        "{case}: index candidates"
    );
}

#[test]
fn compact_exact_search_uses_declared_postings_up_to_half_of_the_rows() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    const DEADLINE: u64 = 1_000;
    for db in [&mut candidate, &mut oracle] {
        for id in 3..=8 {
            put(db, id, &row(Some("alpha"), (id == 3).then_some("x")), None);
        }
        put(db, 9, &row(Some("beta"), Some("x")), Some(DEADLINE));
        put(db, 10, &row(Some("beta"), None), None);
        put(db, 11, &row(Some("gamma"), Some("x")), None);
        put(db, 12, &row(None, Some("x")), None);
    }
    // Twelve rows: a posting is used when it holds at most six ids.
    // alpha {1, 3..=8} has seven, beta {2, 9, 10} three, gamma {11} one; `other` is not declared.
    let before = DEADLINE - 1;
    let at = DEADLINE;
    let cases: [(&str, Vec<Condition>, [(u64, &[u64]); 2], u64); 7] = [
        (
            "gamma",
            vec![eq("tag", "gamma")],
            [(before, &[11]), (at, &[11])],
            1,
        ),
        (
            "beta, one row expires at the deadline",
            vec![eq("tag", "beta")],
            [(before, &[2, 9, 10]), (at, &[2, 10])],
            3,
        ),
        (
            "alpha is wider than half the rows",
            vec![eq("tag", "alpha")],
            [
                (before, &[1, 3, 4, 5, 6, 7, 8]),
                (at, &[1, 3, 4, 5, 6, 7, 8]),
            ],
            0,
        ),
        (
            "a value without postings",
            vec![eq("tag", "delta")],
            [(before, &[]), (at, &[])],
            0,
        ),
        (
            "an undeclared field is a full scan",
            vec![eq("other", "x")],
            [(before, &[3, 9, 11, 12]), (at, &[3, 11, 12])],
            0,
        ),
        (
            "declared and undeclared conditions narrow by the declared one",
            vec![eq("tag", "beta"), eq("other", "x")],
            [(before, &[9]), (at, &[])],
            3,
        ),
        (
            "a wide posting and an undeclared condition",
            vec![eq("tag", "alpha"), eq("other", "x")],
            [(before, &[3]), (at, &[3])],
            0,
        ),
    ];
    for (case, conditions, clocks, index_candidates) in &cases {
        for &(now_ms, ids) in clocks {
            check(
                &candidate,
                &oracle,
                &format!("{case} at {now_ms}"),
                10,
                conditions,
                now_ms,
                ids,
                *index_candidates,
            );
        }
    }
    check(
        &candidate,
        &oracle,
        "beta with k = 1",
        1,
        &[eq("tag", "beta")],
        before,
        &[2],
        3,
    );

    // A metadata-only overlay write moves row 3 out of alpha, which then holds exactly six ids: still within the cap.
    for db in [&mut candidate, &mut oracle] {
        db.collection_mut("indexed")
            .unwrap()
            .update_metadata(3, &row(Some("gamma"), Some("x")))
            .unwrap();
    }
    check(
        &candidate,
        &oracle,
        "alpha at exactly half of the rows",
        10,
        &[eq("tag", "alpha")],
        before,
        &[1, 4, 5, 6, 7, 8],
        6,
    );
    check(
        &candidate,
        &oracle,
        "gamma after the metadata update",
        10,
        &[eq("tag", "gamma")],
        before,
        &[3, 11],
        2,
    );

    // A tombstone shrinks the row count to eleven, so the cap falls to five: alpha (six ids) is no longer used,
    // beta {2, 9} (row 10 is gone) still is.
    for db in [&mut candidate, &mut oracle] {
        db.collection_mut("indexed").unwrap().delete(10).unwrap();
    }
    check(
        &candidate,
        &oracle,
        "alpha above half of eleven rows",
        10,
        &[eq("tag", "alpha")],
        before,
        &[1, 4, 5, 6, 7, 8],
        0,
    );
    check(
        &candidate,
        &oracle,
        "beta after the tombstone",
        10,
        &[eq("tag", "beta")],
        before,
        &[2, 9],
        2,
    );
}
