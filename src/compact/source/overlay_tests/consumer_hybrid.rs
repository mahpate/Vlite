//! Hybrid search over compact records: validation parity, one clock for both legs and fusion order.

use super::consumer_cursor::decode_count_in as decode_count;
use super::{assert_compact_resident_pair, indexed_dispatch_fixture};
use crate::compact::source::dispatch_tests::{compact_dispatch_fixture, gates};
use crate::hybrid::{FusionOptions, HybridHit, HybridQuery, VectorLeg};
use crate::{
    AnnOptions, CollectionMut, CollectionRef, Condition, Database, DistanceMetric, Error,
    HnswConfig, Metadata, MetadataValue, SearchStats,
};
use std::collections::BTreeSet;

/// A deadline far in the future (2100-01-01).
const FAR: u64 = 4_102_444_800_000;
/// The deadline of the expiring rows of the one-clock scenarios.
const DEADLINE: u64 = 1_000;

type Outcome = crate::Result<(Vec<HybridHit>, SearchStats)>;
/// The expected `(id, vector rank, text rank)` of each hit, in result order.
type Ranks = &'static [(u64, Option<u32>, Option<u32>)];

const OK: &str = "ok";
const K_MAX: &str = r#"InvalidQuery("hybrid k must be at most 1000")"#;
const CANDIDATES: &str = r#"InvalidQuery("fusion candidates must be k..=1000")"#;
const RRF: &str = r#"InvalidQuery("rrf_k must be 1..=1000")"#;
const WEIGHTS: &str = r#"InvalidQuery("fusion weights must be finite 0..=10")"#;
const BOTH_ZERO: &str = r#"InvalidQuery("fusion weights must not both be zero")"#;
const TEXT_BYTES: &str = r#"InvalidQuery("hybrid text exceeds 1024 bytes")"#;
const TOKENS: &str = r#"InvalidQuery("hybrid text exceeds 32 unique tokens")"#;
const NEEDS: &str = r#"InvalidQuery("hybrid search needs a vector or text tokens")"#;
const UNDECLARED: &str = r#"InvalidQuery("text search requires declared text_fields")"#;
const OPTIONS: &str = r#"InvalidQuery("rerank_multiplier must be 1..=16")"#;
const FILTER_FIELD: &str = r#"InvalidFilter("field names must be 1..256 UTF-8 bytes")"#;
const FILTER_RANGE: &str = r#"InvalidFilter("range requires min or max")"#;
const FILTER_COUNT: &str = r#"InvalidFilter("filter exceeds 64 conditions")"#;
const DIMENSION_SHORT: &str = "DimensionMismatch { expected: 2, actual: 1 }";
const DIMENSION_LONG: &str = "DimensionMismatch { expected: 2, actual: 3 }";
const NON_FINITE: &str = "NonFiniteVector";
const ZERO_NORM: &str = "ZeroNormQuery";

/// A compact candidate and its resident oracle, opened from one image.
struct Pair {
    candidate: Database,
    oracle: Database,
    name: &'static str,
    candidate_id: u16,
    oracle_id: u16,
}

impl Pair {
    fn new(candidate: Database, oracle: Database, name: &'static str) -> Self {
        assert_compact_resident_pair(&candidate, &oracle, name);
        let candidate_id = candidate.collection(name).unwrap().id();
        let oracle_id = oracle.collection(name).unwrap().id();
        Self {
            candidate,
            oracle,
            name,
            candidate_id,
            oracle_id,
        }
    }

    /// `indexed`: two dimensions, a declared Eq index on `tag` and the text field `title`; rows 1 and 2.
    fn indexed() -> Self {
        let (candidate, oracle) = indexed_dispatch_fixture();
        Self::new(candidate, oracle, "indexed")
    }

    /// `full`: two dimensions, no declared text field; rows 0, 3 (deadline `FAR`), 10, 20 and `u64::MAX`.
    fn full() -> Self {
        let (candidate, oracle, _) = compact_dispatch_fixture(false);
        Self::new(candidate, oracle, "full")
    }

    /// Applies one mutation to the candidate and to the oracle alike.
    fn mutate(&mut self, apply: impl Fn(&mut CollectionMut<'_>)) {
        for db in [&mut self.candidate, &mut self.oracle] {
            apply(&mut db.collection_mut(self.name).unwrap());
        }
    }

    /// The candidate's answer and, if the oracle answered differently, a description of the difference.
    fn parity(&self, query: &HybridQuery, now_ms: u64) -> (Outcome, Option<String>) {
        let actual = hybrid(&self.candidate, self.candidate_id, query, now_ms);
        let expected = hybrid(&self.oracle, self.oracle_id, query, now_ms);
        let (left, right) = (format!("{actual:?}"), format!("{expected:?}"));
        let mismatch = (left != right)
            .then(|| format!("compact {left} != resident {right} for {query:?} at {now_ms}"));
        (actual, mismatch)
    }

    /// Runs `query` at one clock on both handles, requires the identical result and returns it.
    fn run(&self, query: &HybridQuery, now_ms: u64) -> Outcome {
        let (actual, mismatch) = self.parity(query, now_ms);
        if let Some(message) = mismatch {
            panic!("{message}");
        }
        actual
    }
}

fn hybrid(db: &Database, id: u16, query: &HybridQuery, now_ms: u64) -> Outcome {
    CollectionRef { db, id }.search_hybrid_at(query, now_ms)
}

/// `ok` or the debug text of the error.
fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Ok(_) => OK.to_owned(),
        Err(error) => format!("{error:?}"),
    }
}

fn ids(hits: &[HybridHit]) -> Vec<u64> {
    hits.iter().map(|hit| hit.id).collect()
}

fn shape(hits: &[HybridHit]) -> Vec<(u64, Option<u32>, Option<u32>)> {
    hits.iter()
        .map(|hit| (hit.id, hit.vector_rank, hit.text_rank))
        .collect()
}

/// Id, score bits, distance bits and BM25 bits of each hit.
fn bits(hits: &[HybridHit]) -> Vec<(u64, u64, Option<u64>, Option<u64>)> {
    hits.iter()
        .map(|hit| {
            (
                hit.id,
                hit.score.to_bits(),
                hit.distance.map(f64::to_bits),
                hit.bm25.map(f64::to_bits),
            )
        })
        .collect()
}

/// The contract of a fused list: each score is the sum of its legs' `weight / (rrf_k + rank)` (bit for
/// bit), a leg's distance or BM25 is present exactly when that leg ranked the hit, and the order is
/// score descending, then id ascending.
fn assert_fusion(hits: &[HybridHit], fusion: FusionOptions, context: &str) {
    let rrf_k = f64::from(fusion.rrf_k);
    for hit in hits {
        let mut score = 0.0;
        if let Some(rank) = hit.vector_rank {
            score += fusion.vector_weight / (rrf_k + f64::from(rank));
        }
        if let Some(rank) = hit.text_rank {
            score += fusion.text_weight / (rrf_k + f64::from(rank));
        }
        assert_eq!(
            hit.score.to_bits(),
            score.to_bits(),
            "{context}: score of {hit:?}"
        );
        assert_eq!(
            hit.distance.is_some(),
            hit.vector_rank.is_some(),
            "{context}: distance of {hit:?}"
        );
        assert_eq!(
            hit.bm25.is_some(),
            hit.text_rank.is_some(),
            "{context}: bm25 of {hit:?}"
        );
    }
    for pair in hits.windows(2) {
        let (first, second) = (&pair[0], &pair[1]);
        let ordered = first.score > second.score
            || (first.score.to_bits() == second.score.to_bits() && first.id < second.id);
        assert!(ordered, "{context}: {first:?} must precede {second:?}");
    }
}

/// How many base rows of the candidate's collection have been decoded into cells so far.
fn titled(tag: &str, title: &str) -> Metadata {
    Metadata::from([
        ("tag".into(), MetadataValue::String(tag.into())),
        ("title".into(), MetadataValue::String(title.into())),
    ])
}

fn title_only(title: &str) -> Metadata {
    Metadata::from([("title".into(), MetadataValue::String(title.into()))])
}

fn tag_is(value: &str) -> Vec<Condition> {
    vec![Condition::Eq {
        field: "tag".into(),
        value: MetadataValue::String(value.into()),
    }]
}

fn query() -> HybridQuery {
    HybridQuery::new(Some(vec![1.0, 1.0]), "apple", 10)
}

fn weights(vector_weight: f64, text_weight: f64) -> FusionOptions {
    FusionOptions {
        vector_weight,
        text_weight,
        ..FusionOptions::default()
    }
}

fn candidates(count: usize) -> FusionOptions {
    FusionOptions {
        candidates: Some(count),
        ..FusionOptions::default()
    }
}

fn rrf(rrf_k: u32) -> FusionOptions {
    FusionOptions {
        rrf_k,
        ..FusionOptions::default()
    }
}

/// `count` unique tokens `t0 t1 ...`.
fn tokens(count: usize) -> String {
    (0..count)
        .map(|index| format!("t{index}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn approximate(rerank_multiplier: u8) -> VectorLeg {
    VectorLeg::ApproximateReady {
        config: HnswConfig::default(),
        ef_search: 64,
        options: AnnOptions {
            rerank_multiplier,
            ..AnnOptions::default()
        },
    }
}

fn empty_field() -> Vec<Condition> {
    vec![Condition::Eq {
        field: String::new(),
        value: MetadataValue::String("x".into()),
    }]
}

/// Base rows 1 (`[1, 1]`, "red apple") and 2 (`[2, 1]`, "blue berry") plus the overlay rows 3, 4 and 5. For
/// the query vector `[1, 1]` the vector leg ranks 1, 2, 5, 3, 4 (squared distances 0, 1, 1, 4, 32) and the
/// BM25 leg for "apple" ranks 4 (three occurrences), 1 and 3 (equal scores, so by id).
fn scene_a(collection: &mut CollectionMut<'_>) {
    collection
        .put_with_metadata(3, &[1.0, 3.0], b"apple pie", &titled("alpha", "apple pie"))
        .unwrap();
    collection
        .put_with_metadata(
            4,
            &[5.0, 5.0],
            b"apple apple apple",
            &titled("beta", "apple apple apple"),
        )
        .unwrap();
    collection
        .put_with_metadata(5, &[1.0, 2.0], b"green pear", &titled("beta", "green pear"))
        .unwrap();
}

struct Case {
    label: &'static str,
    query: HybridQuery,
    expected: &'static str,
}

fn case(label: &'static str, query: HybridQuery, expected: &'static str) -> Case {
    Case {
        label,
        query,
        expected,
    }
}

/// Invalid and boundary queries for `indexed`, in the order the validation checks them.
fn indexed_cases() -> Vec<Case> {
    let no_bounds = || {
        vec![Condition::Range {
            field: "n".into(),
            min: None,
            max: None,
        }]
    };
    let too_many = || {
        (0..65)
            .map(|index| Condition::Eq {
                field: format!("f{index}"),
                value: MetadataValue::Integer(index),
            })
            .collect::<Vec<_>>()
    };
    let with_text = |text: String| HybridQuery { text, ..query() };
    let with_vector = |vector: Vec<f32>| HybridQuery {
        vector: Some(vector),
        ..query()
    };
    let wide_text = (0..150)
        .map(|index| format!("token{index:03} "))
        .collect::<String>();
    vec![
        case("k 1001", HybridQuery { k: 1001, ..query() }, K_MAX),
        case("k 1000", HybridQuery { k: 1000, ..query() }, OK),
        case(
            "candidates below k",
            HybridQuery {
                fusion: candidates(9),
                ..query()
            },
            CANDIDATES,
        ),
        case(
            "candidates equal to k",
            HybridQuery {
                fusion: candidates(10),
                ..query()
            },
            OK,
        ),
        case(
            "candidates 1001",
            HybridQuery {
                fusion: candidates(1001),
                ..query()
            },
            CANDIDATES,
        ),
        case(
            "candidates 1000",
            HybridQuery {
                fusion: candidates(1000),
                ..query()
            },
            OK,
        ),
        case(
            "k 0 with candidates 0",
            HybridQuery {
                k: 0,
                fusion: candidates(0),
                ..query()
            },
            OK,
        ),
        case(
            "rrf_k 0",
            HybridQuery {
                fusion: rrf(0),
                ..query()
            },
            RRF,
        ),
        case(
            "rrf_k 1001",
            HybridQuery {
                fusion: rrf(1001),
                ..query()
            },
            RRF,
        ),
        case(
            "rrf_k 1",
            HybridQuery {
                fusion: rrf(1),
                ..query()
            },
            OK,
        ),
        case(
            "rrf_k 1000",
            HybridQuery {
                fusion: rrf(1000),
                ..query()
            },
            OK,
        ),
        case(
            "vector weight NaN",
            HybridQuery {
                fusion: weights(f64::NAN, 1.0),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "vector weight infinite",
            HybridQuery {
                fusion: weights(f64::INFINITY, 1.0),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "vector weight negative infinity",
            HybridQuery {
                fusion: weights(f64::NEG_INFINITY, 1.0),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "vector weight negative",
            HybridQuery {
                fusion: weights(-0.5, 1.0),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "vector weight above ten",
            HybridQuery {
                fusion: weights(10.5, 1.0),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "vector weight ten",
            HybridQuery {
                fusion: weights(10.0, 1.0),
                ..query()
            },
            OK,
        ),
        case(
            "vector weight zero",
            HybridQuery {
                fusion: weights(0.0, 1.0),
                ..query()
            },
            OK,
        ),
        case(
            "text weight NaN",
            HybridQuery {
                fusion: weights(1.0, f64::NAN),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "text weight infinite",
            HybridQuery {
                fusion: weights(1.0, f64::INFINITY),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "text weight negative",
            HybridQuery {
                fusion: weights(1.0, -0.5),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "text weight above ten",
            HybridQuery {
                fusion: weights(1.0, 10.5),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "text weight ten",
            HybridQuery {
                fusion: weights(1.0, 10.0),
                ..query()
            },
            OK,
        ),
        case(
            "text weight zero",
            HybridQuery {
                fusion: weights(1.0, 0.0),
                ..query()
            },
            OK,
        ),
        case(
            "both weights zero",
            HybridQuery {
                fusion: weights(0.0, 0.0),
                ..query()
            },
            BOTH_ZERO,
        ),
        case(
            "text of 1025 bytes",
            with_text("a".repeat(1025)),
            TEXT_BYTES,
        ),
        case("text of 1024 bytes", with_text("a".repeat(1024)), OK),
        case(
            "text of 513 two-byte characters",
            with_text("é".repeat(513)),
            TEXT_BYTES,
        ),
        case(
            "text of 512 two-byte characters",
            with_text("é".repeat(512)),
            OK,
        ),
        case(
            "text of 1025 bytes with a disabled text leg",
            HybridQuery {
                text: "a".repeat(1025),
                fusion: weights(1.0, 0.0),
                ..query()
            },
            TEXT_BYTES,
        ),
        case("33 unique tokens", with_text(tokens(33)), TOKENS),
        case("32 unique tokens", with_text(tokens(32)), OK),
        case(
            "64 tokens that are 32 unique tokens",
            with_text(format!("{0} {0}", tokens(32))),
            OK,
        ),
        case(
            "33 unique tokens with a disabled text leg",
            HybridQuery {
                text: tokens(33),
                fusion: weights(1.0, 0.0),
                ..query()
            },
            TOKENS,
        ),
        case(
            "no vector and no text",
            HybridQuery::new(None, "", 10),
            NEEDS,
        ),
        case(
            "no vector and punctuation only",
            HybridQuery::new(None, "!!! ???", 10),
            NEEDS,
        ),
        case(
            "no vector and one over-long token",
            HybridQuery::new(None, "x".repeat(65), 10),
            NEEDS,
        ),
        case(
            "vector only",
            HybridQuery::new(Some(vec![1.0, 1.0]), "", 10),
            OK,
        ),
        case("text only", HybridQuery::new(None, "apple", 10), OK),
        case(
            "text only with a disabled text leg",
            HybridQuery {
                fusion: weights(1.0, 0.0),
                ..HybridQuery::new(None, "apple", 10)
            },
            OK,
        ),
        case(
            "filter with an empty field",
            HybridQuery {
                conditions: empty_field(),
                ..query()
            },
            FILTER_FIELD,
        ),
        case(
            "range without bounds",
            HybridQuery {
                conditions: no_bounds(),
                ..query()
            },
            FILTER_RANGE,
        ),
        case(
            "65 conditions",
            HybridQuery {
                conditions: too_many(),
                ..query()
            },
            FILTER_COUNT,
        ),
        case(
            "valid filter",
            HybridQuery {
                conditions: tag_is("alpha"),
                ..query()
            },
            OK,
        ),
        case(
            "filter with an empty field and no vector",
            HybridQuery {
                conditions: empty_field(),
                ..HybridQuery::new(None, "apple", 10)
            },
            FILTER_FIELD,
        ),
        case("vector too short", with_vector(vec![1.0]), DIMENSION_SHORT),
        case(
            "vector too long",
            with_vector(vec![1.0, 1.0, 1.0]),
            DIMENSION_LONG,
        ),
        case(
            "vector with a NaN component",
            with_vector(vec![f32::NAN, 1.0]),
            NON_FINITE,
        ),
        case(
            "cosine metric with a zero vector",
            HybridQuery {
                metric: DistanceMetric::Cosine,
                ..with_vector(vec![0.0, 0.0])
            },
            ZERO_NORM,
        ),
        case(
            "short vector with a disabled vector leg",
            HybridQuery {
                fusion: weights(0.0, 1.0),
                ..with_vector(vec![1.0])
            },
            OK,
        ),
        case(
            "k 0 still validates the vector",
            HybridQuery {
                k: 0,
                ..with_vector(vec![1.0])
            },
            DIMENSION_SHORT,
        ),
        case(
            "approximate options 0",
            HybridQuery {
                vector_leg: approximate(0),
                ..query()
            },
            OPTIONS,
        ),
        case(
            "approximate options 17",
            HybridQuery {
                vector_leg: approximate(17),
                ..query()
            },
            OPTIONS,
        ),
        case(
            "approximate options without a vector",
            HybridQuery {
                vector_leg: approximate(0),
                ..HybridQuery::new(None, "apple", 10)
            },
            OPTIONS,
        ),
        case(
            "approximate options with a disabled vector leg",
            HybridQuery {
                vector_leg: approximate(0),
                fusion: weights(0.0, 1.0),
                ..query()
            },
            OPTIONS,
        ),
        case(
            "valid approximate options without a vector",
            HybridQuery {
                vector_leg: approximate(4),
                ..HybridQuery::new(None, "apple", 10)
            },
            OK,
        ),
        case(
            "valid approximate options with a disabled vector leg",
            HybridQuery {
                vector_leg: approximate(4),
                fusion: weights(0.0, 1.0),
                ..query()
            },
            OK,
        ),
        case(
            "k before candidates",
            HybridQuery {
                k: 1001,
                fusion: candidates(5),
                ..query()
            },
            K_MAX,
        ),
        case(
            "candidates before rrf_k",
            HybridQuery {
                fusion: FusionOptions {
                    candidates: Some(5),
                    rrf_k: 0,
                    ..FusionOptions::default()
                },
                ..query()
            },
            CANDIDATES,
        ),
        case(
            "rrf_k before the weights",
            HybridQuery {
                fusion: FusionOptions {
                    rrf_k: 0,
                    vector_weight: f64::NAN,
                    ..FusionOptions::default()
                },
                ..query()
            },
            RRF,
        ),
        case(
            "weights before the text size",
            HybridQuery {
                text: "a".repeat(1025),
                fusion: weights(f64::NAN, 1.0),
                ..query()
            },
            WEIGHTS,
        ),
        case(
            "both weights zero before the text size",
            HybridQuery {
                text: "a".repeat(1025),
                fusion: weights(0.0, 0.0),
                ..query()
            },
            BOTH_ZERO,
        ),
        case(
            "text size before the token count",
            with_text(wide_text),
            TEXT_BYTES,
        ),
        case(
            "token count before the filter",
            HybridQuery {
                text: tokens(33),
                conditions: empty_field(),
                ..query()
            },
            TOKENS,
        ),
        case(
            "missing query before the filter",
            HybridQuery {
                conditions: empty_field(),
                ..HybridQuery::new(None, "", 10)
            },
            NEEDS,
        ),
        case(
            "filter before the vector",
            HybridQuery {
                conditions: empty_field(),
                ..with_vector(vec![1.0])
            },
            FILTER_FIELD,
        ),
        case(
            "approximate options before the vector",
            HybridQuery {
                vector_leg: approximate(0),
                ..with_vector(vec![1.0])
            },
            OPTIONS,
        ),
        case(
            "weights before the vector",
            HybridQuery {
                fusion: weights(f64::NAN, 1.0),
                ..with_vector(vec![1.0])
            },
            WEIGHTS,
        ),
    ]
}

/// Queries for `full`, whose collection declares no text field.
fn full_cases() -> Vec<Case> {
    vec![
        case("undeclared text fields", query(), UNDECLARED),
        case(
            "undeclared text fields, text only",
            HybridQuery::new(None, "apple", 10),
            UNDECLARED,
        ),
        case(
            "undeclared text before the vector",
            HybridQuery {
                vector: Some(vec![1.0]),
                ..query()
            },
            UNDECLARED,
        ),
        case(
            "filter before undeclared text",
            HybridQuery {
                conditions: empty_field(),
                ..query()
            },
            FILTER_FIELD,
        ),
        case(
            "undeclared text before the approximate options",
            HybridQuery {
                vector_leg: approximate(0),
                ..query()
            },
            UNDECLARED,
        ),
        case(
            "a disabled text leg never consults the index",
            HybridQuery {
                fusion: weights(1.0, 0.0),
                ..query()
            },
            OK,
        ),
        case(
            "text without tokens never consults the index",
            HybridQuery::new(Some(vec![1.0, 1.0]), "!!!", 10),
            OK,
        ),
        case(
            "vector only",
            HybridQuery::new(Some(vec![1.0, 1.0]), "", 10),
            OK,
        ),
    ]
}

#[test]
fn hybrid_validation_matches_resident_in_check_order() {
    // Not covered: the `IndexNotReady` answer for an unavailable text index. `TextIndex::limit` and
    // `available` are private to `text_index.rs` and `MAX_TEXT_POSTINGS` (16,777,216) is out of reach of
    // a test, so no table row can make the index unavailable.
    let _gates = gates();
    let indexed = Pair::indexed();
    let full = Pair::full();
    let (indexed_table, full_table) = (indexed_cases(), full_cases());
    let mut failures = Vec::new();
    for (pair, table) in [(&indexed, &indexed_table), (&full, &full_table)] {
        for case in table {
            let (actual, mismatch) = pair.parity(&case.query, 0);
            failures.extend(mismatch);
            let observed = outcome_text(&actual);
            if observed != case.expected {
                let (label, expected) = (case.label, case.expected);
                failures.push(format!(
                    "{label}: expected {expected} but the compact collection answered {observed}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Every branch of the resident validation is reached, as an error and as an accepted boundary.
    let reached: BTreeSet<&str> = indexed_table
        .iter()
        .chain(&full_table)
        .map(|case| case.expected)
        .collect();
    for branch in [
        OK,
        K_MAX,
        CANDIDATES,
        RRF,
        WEIGHTS,
        BOTH_ZERO,
        TEXT_BYTES,
        TOKENS,
        NEEDS,
        UNDECLARED,
        OPTIONS,
        FILTER_FIELD,
        FILTER_RANGE,
        FILTER_COUNT,
        DIMENSION_SHORT,
        DIMENSION_LONG,
        NON_FINITE,
        ZERO_NORM,
    ] {
        assert!(reached.contains(branch), "no case reaches {branch}");
    }
}

#[test]
fn hybrid_health_precedes_every_validation_and_every_valid_query() {
    let _gates = gates();
    for (mut pair, table) in [
        (Pair::indexed(), indexed_cases()),
        (Pair::full(), full_cases()),
    ] {
        assert!(table.iter().any(|case| case.expected == OK));
        assert!(table.iter().any(|case| case.expected != OK));
        pair.candidate.poisoned = true;
        pair.oracle.poisoned = true;
        for case in &table {
            for (side, db, id) in [
                ("compact", &pair.candidate, pair.candidate_id),
                ("resident", &pair.oracle, pair.oracle_id),
            ] {
                let result = hybrid(db, id, &case.query, 0);
                let label = case.label;
                assert!(
                    matches!(result, Err(Error::Poisoned)),
                    "{side} {label} must answer Poisoned, not {result:?}"
                );
            }
        }
    }
}

/// An overlay put with a deadline over a live base row: the overlay replaces the base "red apple" by
/// "green pear" at the same vector, so a revealed base row would be a BM25 match and the overlay row is not.
fn expired_overlay_over_live_base(collection: &mut CollectionMut<'_>) {
    shared_rows(collection);
    collection
        .put_with_expiry(
            1,
            &[1.0, 1.0],
            b"overlay",
            &titled("alpha", "green pear"),
            Some(DEADLINE),
        )
        .unwrap();
}

/// A tombstone over base row 1.
fn tombstone_over_live_base(collection: &mut CollectionMut<'_>) {
    shared_rows(collection);
    collection.delete(1).unwrap();
}

/// Base row 1 rewritten identically, with only a deadline added.
fn deadline_only_overlay(collection: &mut CollectionMut<'_>) {
    shared_rows(collection);
    collection
        .put_with_expiry(
            1,
            &[1.0, 1.0],
            b"red apple",
            &titled("alpha", "red apple"),
            Some(DEADLINE),
        )
        .unwrap();
}

/// Rows 3 (`[3, 1]`, "apple pie") and 4 (`[1, 2]`, "green pear") next to base rows 1 and 2.
fn shared_rows(collection: &mut CollectionMut<'_>) {
    collection
        .put_with_metadata(3, &[3.0, 1.0], b"apple pie", &titled("alpha", "apple pie"))
        .unwrap();
    collection
        .put_with_metadata(4, &[1.0, 2.0], b"green pear", &titled("beta", "green pear"))
        .unwrap();
}

/// Runs the apple query with `k = candidates = 3` (each leg and the result hold three rows at most) at
/// clocks around `DEADLINE` and checks the ranks of every hit: `live` while the overlay row is visible,
/// `hidden` once it is not.
fn check_clock_scenario(
    name: &str,
    apply: fn(&mut CollectionMut<'_>),
    live: Ranks,
    hidden: Ranks,
    expires: bool,
) {
    let mut pair = Pair::indexed();
    pair.mutate(apply);
    let query = HybridQuery {
        k: 3,
        fusion: candidates(3),
        ..query()
    };
    for now_ms in [0, DEADLINE - 1, DEADLINE, DEADLINE + 1, FAR] {
        let hits = pair.run(&query, now_ms).unwrap().0;
        let expected = if expires && now_ms < DEADLINE {
            live
        } else {
            hidden
        };
        assert_eq!(shape(&hits), expected, "{name} at {now_ms}");
        assert_fusion(&hits, query.fusion, &format!("{name} at {now_ms}"));
    }
}

// Query vector [1, 1], text "apple". Rows 2 ([2, 1], "blue berry"), 3 ([3, 1], "apple pie") and 4
// ([1, 2], "green pear") never expire; the vector leg ranks them 2, 4, 3 by distance 1, 1, 4.
/// Overlay row 1 is `[1, 1]` "green pear": nearest, so vector rank 1 without a BM25 match; it ties with
/// the text-only hit 3 (both 1/61), so the lower id comes first, and the cut drops row 4.
const OVERLAY_LIVE: Ranks = &[(1, Some(1), None), (3, None, Some(1)), (2, Some(2), None)];
/// Row 1 is `[1, 1]` "red apple": nearest and a BM25 match, so both legs rank it first; the vector leg's
/// three rows are 1, 2 and 4, so row 3 is a text-only hit that ties with row 2 (both 1/62).
const DEADLINE_ONLY_LIVE: Ranks = &[
    (1, Some(1), Some(1)),
    (2, Some(2), None),
    (3, None, Some(2)),
];
/// Without row 1 the vector leg holds 2, 4 and 3 and the text leg only row 3.
const WITHOUT_ROW_ONE: Ranks = &[
    (3, Some(3), Some(1)),
    (2, Some(1), None),
    (4, Some(2), None),
];

#[test]
fn hybrid_expired_overlay_put_hides_the_row_in_both_legs_and_never_reveals_the_base_row() {
    let _gates = gates();
    // At and after the deadline the overlay put is gone from both legs; the base row 1 ([1, 1], "red
    // apple": distance 0, a BM25 match) must not come back, which would put it first in both legs.
    check_clock_scenario(
        "expired overlay put",
        expired_overlay_over_live_base,
        OVERLAY_LIVE,
        WITHOUT_ROW_ONE,
        true,
    );
}

#[test]
fn hybrid_tombstone_hides_the_base_row_at_every_clock() {
    let _gates = gates();
    check_clock_scenario(
        "tombstone",
        tombstone_over_live_base,
        WITHOUT_ROW_ONE,
        WITHOUT_ROW_ONE,
        false,
    );
}

#[test]
fn hybrid_deadline_only_overlay_write_applies_its_deadline_to_both_legs() {
    let _gates = gates();
    // The overlay row equals the base row except for its deadline; the base row has none.
    check_clock_scenario(
        "deadline-only overlay",
        deadline_only_overlay,
        DEADLINE_ONLY_LIVE,
        WITHOUT_ROW_ONE,
        true,
    );
}

#[test]
fn hybrid_base_row_deadline_hides_exactly_at_the_clock_and_an_overlay_without_one_restores_it() {
    let _gates = gates();
    let mut pair = Pair::full();
    // Declare the text field `id` (the decimal string of the row ID) on both handles.
    for db in [&mut pair.candidate, &mut pair.oracle] {
        let mut settings = db.collection("full").unwrap().settings().clone();
        settings.text_fields = vec!["id".into()];
        db.set_collection_settings("full", settings).unwrap();
    }
    // Rows 0, 3, 10 and MAX hold the same vector and tie at distance 0, so they rank by ID; row 20 is
    // farther. Only row 3 matches the text "3", and its base deadline is FAR.
    let query = HybridQuery::new(Some(vec![0.1, 0.0]), "3", 10);
    const LIVE: Ranks = &[
        (3, Some(2), Some(1)),
        (0, Some(1), None),
        (10, Some(3), None),
        (u64::MAX, Some(4), None),
        (20, Some(5), None),
    ];
    const HIDDEN: Ranks = &[
        (0, Some(1), None),
        (10, Some(2), None),
        (u64::MAX, Some(3), None),
        (20, Some(4), None),
    ];
    for (now_ms, expected) in [(0, LIVE), (FAR - 1, LIVE), (FAR, HIDDEN), (FAR + 1, HIDDEN)] {
        let hits = pair.run(&query, now_ms).unwrap().0;
        assert_eq!(shape(&hits), expected, "base deadline at {now_ms}");
        assert_fusion(&hits, query.fusion, &format!("base deadline at {now_ms}"));
    }
    // A later overlay put without a deadline replaces the base deadline: the row is never hidden again.
    pair.mutate(|collection| {
        collection
            .put_with_metadata(
                3,
                &[0.1, -0.0],
                &3u64.to_le_bytes(),
                &Metadata::from([("id".into(), MetadataValue::String("3".into()))]),
            )
            .unwrap();
    });
    for now_ms in [FAR - 1, FAR, FAR + 1, u64::MAX] {
        let hits = pair.run(&query, now_ms).unwrap().0;
        assert_eq!(shape(&hits), LIVE, "restored row at {now_ms}");
    }
}

#[test]
fn hybrid_wall_clock_entry_hides_past_deadlines_and_keeps_far_ones_in_both_legs() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(|collection| {
        // The nearest row and a BM25 match, but its deadline passed long ago.
        collection
            .put_with_expiry(
                3,
                &[1.0, 1.0],
                b"expired",
                &titled("alpha", "apple pie"),
                Some(1),
            )
            .unwrap();
        collection
            .put_with_expiry(
                4,
                &[3.0, 1.0],
                b"far",
                &titled("alpha", "apple crumble"),
                Some(FAR),
            )
            .unwrap();
    });
    let actual = pair
        .candidate
        .collection("indexed")
        .unwrap()
        .search_hybrid(&query());
    let expected = pair
        .oracle
        .collection("indexed")
        .unwrap()
        .search_hybrid(&query());
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    let hits = actual.unwrap().0;
    assert_eq!(
        shape(&hits),
        [
            (1, Some(1), Some(1)),
            (4, Some(3), Some(2)),
            (2, Some(2), None)
        ]
    );
    assert_fusion(&hits, query().fusion, "wall clock");
}

#[test]
fn hybrid_k_zero_keeps_the_vector_leg_work_and_its_validation() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(scene_a);
    // The vector leg's depth is `candidates` (50 by default), so all five rows are scanned and scored.
    let five_rows = SearchStats {
        records_scanned: 5,
        distance_evaluations: 5,
        ..SearchStats::default()
    };
    let zero = HybridQuery::new(Some(vec![1.0, 1.0]), "apple", 0);
    let (hits, stats) = pair.run(&zero, 0).unwrap();
    assert!(hits.is_empty());
    assert_eq!(stats, five_rows);
    // The same work with k = 1.
    let (hits, stats) = pair
        .run(
            &HybridQuery {
                k: 1,
                ..zero.clone()
            },
            0,
        )
        .unwrap();
    assert_eq!(shape(&hits), [(1, Some(1), Some(2))]);
    assert_eq!(stats, five_rows);
    // A depth of zero scans nothing.
    let shallow = HybridQuery {
        fusion: candidates(0),
        ..zero.clone()
    };
    let (hits, stats) = pair.run(&shallow, 0).unwrap();
    assert!(hits.is_empty());
    assert_eq!(stats, SearchStats::default());
    // Text only: no vector leg, so no work to report.
    let (hits, stats) = pair.run(&HybridQuery::new(None, "apple", 0), 0).unwrap();
    assert!(hits.is_empty());
    assert_eq!(stats, SearchStats::default());
    // The vector is validated before the early return.
    let short = HybridQuery::new(Some(vec![1.0]), "apple", 0);
    assert_eq!(outcome_text(&pair.run(&short, 0)), DIMENSION_SHORT);
}

#[test]
fn hybrid_k_truncates_the_fused_list_and_candidates_bound_each_leg() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(scene_a);
    // Five fused rows: ranks (vector, text) of 1 are (1, 2), of 4 are (5, 1), of 3 are (4, 3), of 2 are
    // (2, -) and of 5 are (3, -).
    const ALL: Ranks = &[
        (1, Some(1), Some(2)),
        (4, Some(5), Some(1)),
        (3, Some(4), Some(3)),
        (2, Some(2), None),
        (5, Some(3), None),
    ];
    let cases: [(usize, Option<usize>, Ranks); 11] = [
        (1, None, &ALL[..1]),
        (2, None, &ALL[..2]),
        (3, None, &ALL[..3]),
        (4, None, &ALL[..4]),
        (5, None, ALL),
        (100, None, ALL),
        // With `candidates` = k each leg stops after k rows, so a row loses the ranks it holds deeper than
        // that: row 4 its vector rank 5, row 3 its text rank 3 (k = 2) and its vector rank 4 (k = 2, 3).
        (2, Some(2), &[(1, Some(1), Some(2)), (4, None, Some(1))]),
        (
            3,
            Some(3),
            &[
                (1, Some(1), Some(2)),
                (4, None, Some(1)),
                (2, Some(2), None),
            ],
        ),
        (
            4,
            Some(4),
            &[
                (1, Some(1), Some(2)),
                (3, Some(4), Some(3)),
                (4, None, Some(1)),
                (2, Some(2), None),
            ],
        ),
        // Three candidates stop the vector leg before row 4, which stays a text-only hit while k cuts
        // the list after two rows.
        (2, Some(3), &[(1, Some(1), Some(2)), (4, None, Some(1))]),
        // Five candidates reach every row: the default ranks.
        (5, Some(5), ALL),
    ];
    for (k, depth, expected) in cases {
        let query = HybridQuery {
            fusion: FusionOptions {
                candidates: depth,
                ..FusionOptions::default()
            },
            ..HybridQuery::new(Some(vec![1.0, 1.0]), "apple", k)
        };
        let hits = pair.run(&query, 0).unwrap().0;
        assert_eq!(shape(&hits), expected, "k {k} candidates {depth:?}");
        assert_fusion(&hits, query.fusion, &format!("k {k} candidates {depth:?}"));
    }
}

#[test]
fn hybrid_approximate_leg_stays_refused_on_compact_records_without_side_effects() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(scene_a);
    let id = pair.candidate_id;
    let sequence = pair.candidate.sequence;
    let decoded = decode_count(&pair.candidate, id);
    let effective_ids = pair.candidate.state(id).records.effective_ids();
    let text_status = pair.candidate.state(id).text_index.status();
    for (label, query) in [
        (
            "vector and text",
            HybridQuery {
                vector_leg: approximate(4),
                ..query()
            },
        ),
        (
            "vector only",
            HybridQuery {
                vector_leg: approximate(4),
                ..HybridQuery::new(Some(vec![1.0, 1.0]), "", 10)
            },
        ),
        (
            "k 0",
            HybridQuery {
                k: 0,
                vector_leg: approximate(4),
                ..query()
            },
        ),
        (
            "with a filter",
            HybridQuery {
                conditions: tag_is("alpha"),
                vector_leg: approximate(16),
                ..query()
            },
        ),
    ] {
        let result = hybrid(&pair.candidate, id, &query, 0);
        assert!(
            matches!(
                result,
                Err(Error::UnsupportedFeature(crate::format::FLAG_COMPACT))
            ),
            "{label}: {result:?}"
        );
        // The same well-formed query is only missing an index on the resident oracle.
        let resident = hybrid(&pair.oracle, pair.oracle_id, &query, 0);
        assert!(
            matches!(resident, Err(Error::IndexNotReady)),
            "{label}: {resident:?}"
        );
    }
    assert_eq!(
        decode_count(&pair.candidate, id),
        decoded,
        "a refused leg must not materialize rows"
    );
    assert_eq!(pair.candidate.sequence, sequence);
    assert_eq!(
        pair.candidate.state(id).records.effective_ids(),
        effective_ids
    );
    assert_eq!(pair.candidate.state(id).text_index.status(), text_status);
    // Invalid options are reported before the leg is refused, on both handles alike.
    for options in [0, 17] {
        let invalid = HybridQuery {
            vector_leg: approximate(options),
            ..query()
        };
        assert_eq!(outcome_text(&pair.run(&invalid, 0)), OPTIONS);
    }
}

#[test]
fn hybrid_filter_applies_to_both_legs_and_the_vector_leg_reports_its_work() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(scene_a);
    let with = |conditions: Vec<Condition>| HybridQuery {
        conditions,
        ..query()
    };
    // alpha has two postings, at most half of the five rows: the index narrows the scan to rows 1 and 3.
    let (hits, stats) = pair.run(&with(tag_is("alpha")), 0).unwrap();
    assert_eq!(shape(&hits), [(1, Some(1), Some(1)), (3, Some(2), Some(2))]);
    assert_eq!(
        stats,
        SearchStats {
            records_scanned: 2,
            distance_evaluations: 2,
            index_candidates: 2,
            ..SearchStats::default()
        }
    );
    // beta has three postings, more than half of the rows: a full scan that scores only rows 2, 4 and 5.
    // Row 4 is the only beta row that matches "apple".
    let (hits, stats) = pair.run(&with(tag_is("beta")), 0).unwrap();
    assert_eq!(
        shape(&hits),
        [
            (4, Some(3), Some(1)),
            (2, Some(1), None),
            (5, Some(2), None)
        ]
    );
    assert_eq!(
        stats,
        SearchStats {
            records_scanned: 5,
            distance_evaluations: 3,
            ..SearchStats::default()
        }
    );
    // The text leg alone honours the filter too.
    let text_only = HybridQuery {
        conditions: tag_is("beta"),
        ..HybridQuery::new(None, "apple", 10)
    };
    let (hits, stats) = pair.run(&text_only, 0).unwrap();
    assert_eq!(shape(&hits), [(4, None, Some(1))]);
    assert_eq!(stats, SearchStats::default());
    // An indexed value nobody holds: nothing is scanned.
    let (hits, stats) = pair.run(&with(tag_is("zeta")), 0).unwrap();
    assert!(hits.is_empty());
    assert_eq!(stats, SearchStats::default());
    // A field without an index: every row is scanned and none is scored.
    let missing = vec![Condition::Eq {
        field: "missing".into(),
        value: MetadataValue::String("x".into()),
    }];
    let (hits, stats) = pair.run(&with(missing), 0).unwrap();
    assert!(hits.is_empty());
    assert_eq!(
        stats,
        SearchStats {
            records_scanned: 5,
            ..SearchStats::default()
        }
    );
}

#[test]
fn hybrid_fusion_scores_distances_and_bm25_equal_the_resident_values_bit_for_bit() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(scene_a);
    // Vector leg (top 4): 1, 2, 5, 3. Text leg (top 4, three matches): 4, 1, 3. Only 1 and 3 are in both.
    let fusion = FusionOptions {
        candidates: Some(4),
        rrf_k: 10,
        vector_weight: 2.0,
        text_weight: 3.0,
    };
    let query = HybridQuery {
        fusion,
        ..HybridQuery::new(Some(vec![1.0, 1.0]), "apple", 4)
    };
    let actual = hybrid(&pair.candidate, pair.candidate_id, &query, 0).unwrap();
    let expected = hybrid(&pair.oracle, pair.oracle_id, &query, 0).unwrap();
    assert_eq!(bits(&actual.0), bits(&expected.0));
    assert_eq!(shape(&actual.0), shape(&expected.0));
    assert_eq!(actual.1, expected.1);
    let hits = actual.0;
    assert_eq!(
        shape(&hits),
        [
            (1, Some(1), Some(2)),
            (3, Some(4), Some(3)),
            (4, None, Some(1)),
            (2, Some(2), None),
        ]
    );
    assert_fusion(&hits, fusion, "fusion arithmetic");
    // Hand-computed scores (rrf_k 10, weights 2 and 3): row 1 is vector rank 1 and text rank 2, row 3 is
    // vector rank 4 and text rank 3, row 4 is text rank 1 only and row 2 is vector rank 2 only.
    assert_eq!(
        hits[0].score.to_bits(),
        (2.0 / 11.0 + 3.0 / 12.0f64).to_bits()
    );
    assert_eq!(
        hits[1].score.to_bits(),
        (2.0 / 14.0 + 3.0 / 13.0f64).to_bits()
    );
    assert_eq!(hits[2].score.to_bits(), (3.0 / 11.0f64).to_bits());
    assert_eq!(hits[3].score.to_bits(), (2.0 / 12.0f64).to_bits());
    // Squared L2 distances of [1, 1] to rows 1, 3 and 2.
    assert_eq!(hits[0].distance, Some(0.0));
    assert_eq!(hits[1].distance, Some(4.0));
    assert_eq!(hits[3].distance, Some(1.0));
    assert_eq!(hits[2].distance, None);
    // BM25 (k1 = 1.2, b = 0.75) over five documents of 2, 2, 2, 3 and 2 tokens: "apple" has df 3.
    let bm25 = |term_frequency: f64, length: f64| {
        let (documents, df, average) = (5.0f64, 3.0f64, 11.0 / 5.0);
        let idf = (1.0 + (documents - df + 0.5) / (df + 0.5)).ln();
        idf * term_frequency * 2.2 / (term_frequency + 1.2 * (0.25 + 0.75 * length / average))
    };
    let close = |left: f64, right: f64| (left - right).abs() < 1e-12;
    assert!(close(hits[0].bm25.unwrap(), bm25(1.0, 2.0)));
    assert!(close(hits[1].bm25.unwrap(), bm25(1.0, 2.0)));
    assert!(close(hits[2].bm25.unwrap(), bm25(3.0, 3.0)));
    assert!(hits[3].bm25.is_none());
    // The metric reaches the vector leg: for [1, 0] the nearest row is 1 under squared L2, 2 under cosine
    // and 4 under inner product.
    for (metric, nearest) in [
        (DistanceMetric::SquaredL2, 1),
        (DistanceMetric::Cosine, 2),
        (DistanceMetric::InnerProduct, 4),
    ] {
        let query = HybridQuery {
            metric,
            ..HybridQuery::new(Some(vec![1.0, 0.0]), "", 5)
        };
        let hits = pair.run(&query, 0).unwrap().0;
        assert_eq!(hits.len(), 5, "{metric:?}");
        assert_eq!(hits[0].id, nearest, "{metric:?}");
        assert_eq!(hits[0].vector_rank, Some(1), "{metric:?}");
    }
}

#[test]
fn hybrid_disabled_legs_are_never_evaluated() {
    let _gates = gates();
    // A zero text weight never consults the text index, so undeclared fields are not an error and the
    // answer is the vector-only answer, work counters included.
    let full = Pair::full();
    let (expected_hits, expected_stats) = full
        .run(&HybridQuery::new(Some(vec![1.0, 1.0]), "", 3), 0)
        .unwrap();
    assert_eq!(expected_hits.len(), 3);
    let disabled = HybridQuery {
        fusion: weights(1.0, 0.0),
        ..HybridQuery::new(Some(vec![1.0, 1.0]), "apple banana", 3)
    };
    let (hits, stats) = full.run(&disabled, 0).unwrap();
    assert_eq!(hits, expected_hits);
    assert_eq!(stats, expected_stats);
    assert!(hits.iter().all(|hit| hit.text_rank.is_none()));
    let enabled = HybridQuery {
        fusion: FusionOptions::default(),
        ..disabled
    };
    assert_eq!(outcome_text(&full.run(&enabled, 0)), UNDECLARED);

    // A zero vector weight never evaluates the vector leg: a vector of the wrong dimension, an
    // approximate leg that would be refused and the work counters all stay out of the answer.
    let mut indexed = Pair::indexed();
    indexed.mutate(scene_a);
    let text_only = HybridQuery::new(None, "apple", 3);
    let (text_hits, text_stats) = indexed.run(&text_only, 0).unwrap();
    assert_eq!(
        shape(&text_hits),
        [(4, None, Some(1)), (1, None, Some(2)), (3, None, Some(3))]
    );
    assert_eq!(text_stats, SearchStats::default());
    for disabled in [
        HybridQuery {
            vector: Some(vec![1.0]),
            fusion: weights(0.0, 1.0),
            ..text_only.clone()
        },
        HybridQuery {
            vector: Some(vec![1.0, 1.0]),
            vector_leg: approximate(4),
            fusion: weights(0.0, 1.0),
            ..text_only.clone()
        },
    ] {
        let (hits, stats) = indexed.run(&disabled, 0).unwrap();
        assert_eq!(hits, text_hits);
        assert_eq!(stats, SearchStats::default());
    }
}

/// Ties of the fused score between a row only the vector leg returns and a row only the text leg returns,
/// at the same rank: rank 1 has the lower id in the vector leg (61 < 62), rank 2 in the text leg
/// (51 < 52) and rank 3 in the vector leg again (41 < 42). The best pair holds the highest ids, so an
/// ID-ordered prefix is never the score-ordered prefix.
const TIE_VECTOR_ROWS: [(u64, [f32; 2]); 6] = [
    (61, [100.0, 100.0]),
    (52, [101.0, 100.0]),
    (41, [100.0, 102.0]),
    (32, [103.0, 100.0]),
    (21, [100.0, 104.0]),
    (12, [105.0, 100.0]),
];
/// The text-only rows. One more token each, so BM25 for "tart" ranks them in this order.
const TIE_TEXT_ROWS: [(u64, &str); 6] = [
    (62, "tart"),
    (51, "tart pie"),
    (42, "tart pie cake"),
    (31, "tart pie cake jam"),
    (22, "tart pie cake jam fig"),
    (11, "tart pie cake jam fig plum"),
];
/// The fused order: rank 1 pair, rank 2 pair, rank 3 pair, and so on; ascending id inside each pair.
const TIE_ORDER: [u64; 12] = [61, 62, 51, 52, 41, 42, 31, 32, 21, 22, 11, 12];

/// Twelve overlay rows next to the base rows 1 and 2 (about 19 000 away from `[100, 100]`, no "tart"):
/// with `candidates = 6` the vector leg returns exactly `TIE_VECTOR_ROWS` and the text leg exactly
/// `TIE_TEXT_ROWS`, so the legs share no row.
fn tie_rows(collection: &mut CollectionMut<'_>) {
    for (id, vector) in TIE_VECTOR_ROWS {
        collection.put(id, &vector, b"vector only").unwrap();
    }
    for (id, title) in TIE_TEXT_ROWS {
        collection
            .put_with_metadata(id, &[0.0, 0.0], title.as_bytes(), &title_only(title))
            .unwrap();
    }
}

#[test]
fn hybrid_equal_fused_scores_break_ties_by_ascending_id_across_the_legs() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(tie_rows);
    // An ID-ordered prefix differs from the score-ordered prefix for every k, so truncating before
    // sorting cannot pass.
    let mut ascending = TIE_ORDER;
    ascending.sort_unstable();
    for k in 1..=6 {
        assert_ne!(ascending[..k], TIE_ORDER[..k], "k {k}");
    }
    for fusion in [
        FusionOptions {
            candidates: Some(6),
            rrf_k: 20,
            vector_weight: 1.5,
            text_weight: 1.5,
        },
        FusionOptions {
            candidates: Some(6),
            ..FusionOptions::default()
        },
    ] {
        assert_eq!(fusion.vector_weight.to_bits(), fusion.text_weight.to_bits());
        let all = HybridQuery {
            fusion,
            ..HybridQuery::new(Some(vec![100.0, 100.0]), "tart", 6)
        };
        let hits = pair.run(&all, 0).unwrap().0;
        // Each pair is one hit of the vector leg alone and one of the text leg alone, at the same rank.
        assert_eq!(
            shape(&hits),
            [
                (61, Some(1), None),
                (62, None, Some(1)),
                (51, None, Some(2)),
                (52, Some(2), None),
                (41, Some(3), None),
                (42, None, Some(3)),
            ]
        );
        assert_fusion(&hits, fusion, "ties");
        // The scores of a pair are the same bits, not merely close, and the pairs are strictly ordered.
        for (first, rank) in [(0, 1u32), (2, 2), (4, 3)] {
            let score = fusion.vector_weight / (f64::from(fusion.rrf_k) + f64::from(rank));
            assert_eq!(hits[first].score.to_bits(), score.to_bits(), "rank {rank}");
            assert_eq!(
                hits[first + 1].score.to_bits(),
                score.to_bits(),
                "rank {rank}"
            );
        }
        assert!(hits[1].score > hits[2].score && hits[3].score > hits[4].score);
        // k cuts the list at every position: the lower id of a cut pair survives, wherever its row came
        // from.
        for k in 1..=6 {
            let cut = pair.run(&HybridQuery { k, ..all.clone() }, 0).unwrap().0;
            assert_eq!(ids(&cut), TIE_ORDER[..k].to_vec(), "k {k}");
            assert_fusion(&cut, fusion, &format!("ties cut at {k}"));
        }
        for (k, lower, higher) in [(1, 61, 62), (3, 51, 52), (5, 41, 42)] {
            let cut = ids(&pair.run(&HybridQuery { k, ..all.clone() }, 0).unwrap().0);
            assert_eq!(cut.last(), Some(&lower), "k {k}");
            assert!(!cut.contains(&higher), "k {k}");
        }
    }
}

#[test]
fn hybrid_equal_fused_scores_of_rows_in_both_legs_break_ties_by_ascending_id() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(|collection| {
        // Rows 8 and 5 swap their ranks between the legs: 8 is nearest and the second BM25 match for
        // "tart", 5 is second nearest and the first match. Row 3 is a third vector-only hit.
        collection
            .put_with_metadata(8, &[100.0, 100.0], b"tart pie", &title_only("tart pie"))
            .unwrap();
        collection
            .put_with_metadata(5, &[101.0, 100.0], b"tart", &title_only("tart"))
            .unwrap();
        collection.put(3, &[100.0, 102.0], b"third").unwrap();
    });
    let fusion = FusionOptions {
        candidates: Some(3),
        rrf_k: 20,
        vector_weight: 1.5,
        text_weight: 1.5,
    };
    let all = HybridQuery {
        fusion,
        ..HybridQuery::new(Some(vec![100.0, 100.0]), "tart", 3)
    };
    let hits = pair.run(&all, 0).unwrap().0;
    assert_eq!(
        shape(&hits),
        [
            (5, Some(2), Some(1)),
            (8, Some(1), Some(2)),
            (3, Some(3), None),
        ]
    );
    assert_fusion(&hits, fusion, "swapped ranks");
    // Both rows add the same two terms in the other order: the sums are the same bits.
    assert_eq!(hits[0].score.to_bits(), hits[1].score.to_bits());
    assert!(hits[1].score > hits[2].score);
    // The row ID order is [3, 5, 8]; the score order puts 3 last, and a cut keeps the lower tied id.
    for k in 1..=3 {
        let cut = pair.run(&HybridQuery { k, ..all.clone() }, 0).unwrap().0;
        assert_eq!(ids(&cut), [5, 8, 3][..k].to_vec(), "k {k}");
    }
}

/// A compile-time guard: `search_hybrid_compact` copies the validation and the fusion of the resident search, and
/// nothing but the parity tables tells one copy from the other. A new field of `HybridQuery` or `FusionOptions` stops
/// this from compiling until both arms (and the tables above) have been taught about it.
#[test]
fn every_field_of_the_hybrid_query_is_known_to_this_suite() {
    let HybridQuery {
        vector: _,
        text: _,
        k: _,
        conditions: _,
        metric: _,
        vector_leg: _,
        fusion,
    } = HybridQuery::new(None, "a", 1);
    let FusionOptions {
        candidates: _,
        rrf_k: _,
        vector_weight: _,
        text_weight: _,
    } = fusion;
}
