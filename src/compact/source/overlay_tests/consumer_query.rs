//! The query language over compact records: clauses, ordering, grouping, facets and error precedence.

use super::consumer_cursor::decode_count_in as decode_count;
use super::{assert_compact_resident_pair, indexed_dispatch_fixture, mutate_indexed_rows};
use crate::compact::source::dispatch_tests::{compact_dispatch_fixture, gates};
use crate::query_language::{QueryOutput, parse};
use crate::records::CollectionRecords;
use crate::vectors::{RecordVectors, VersionedVector};
use crate::{CollectionMut, CollectionRef, Database, Metadata, MetadataValue};
use std::collections::{BTreeMap, BTreeSet};

/// A deadline far in the future (2100-01-01). `ParsedQuery::execute` samples the wall clock, so a row is
/// hidden by a deadline in the past (1) and shown by this one.
const FAR: u64 = 4_102_444_800_000;

type Outcome = crate::Result<QueryOutput>;

const MISSING: &str = r#"InvalidQuery("missing vector parameter")"#;
const DIMENSION_SHORT: &str = "DimensionMismatch { expected: 2, actual: 1 }";
const NON_FINITE: &str = "NonFiniteVector";
const VERSION_ON_DEFAULT: &str =
    r#"InvalidQuery("the legacy default vector has no model versions")"#;
const UNKNOWN_VECTOR: &str =
    r#"InvalidQuery("invalid vector declaration, version, batch or sparse vector")"#;
const UNDECLARED: &str = r#"InvalidQuery("text fields must be declared")"#;
const POISONED: &str = "Poisoned";

/// The query vector of the clause tables.
const Q: &[f32] = &[1.0, 1.0];
/// The query vector of the named-vector tables.
const NAMED_Q: &[f32] = &[0.3, 0.0];

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

    /// `indexed`: two dimensions, a declared Eq index on `tag`, the text field `title`; rows 1 and 2.
    fn indexed() -> Self {
        let (candidate, oracle) = indexed_dispatch_fixture();
        Self::new(candidate, oracle, "indexed")
    }

    /// `full`: two dimensions, the named vector `n` (versions v1 and v2), no declared text field; rows 0,
    /// 3 (deadline `FAR`), 10, 20 and `u64::MAX`.
    fn full() -> Self {
        let (candidate, oracle, _) = compact_dispatch_fixture(false);
        Self::new(candidate, oracle, "full")
    }

    fn mutate_databases(&mut self, apply: impl Fn(&mut Database)) {
        for db in [&mut self.candidate, &mut self.oracle] {
            apply(db);
        }
    }

    /// Applies one mutation to the candidate and to the oracle alike.
    fn mutate(&mut self, apply: impl Fn(&mut CollectionMut<'_>)) {
        let name = self.name;
        self.mutate_databases(|db| apply(&mut db.collection_mut(name).unwrap()));
    }

    /// The candidate's answer and, if the oracle answered differently, a description of the difference.
    fn parity(&self, query: &str, vector: Option<&[f32]>) -> (Outcome, Option<String>) {
        let parameters = parameters(vector);
        let actual = execute(&self.candidate, self.candidate_id, query, &parameters);
        let expected = execute(&self.oracle, self.oracle_id, query, &parameters);
        let (left, right) = (render(&actual), render(&expected));
        let mismatch =
            (left != right).then(|| format!("{query}: compact {left} != resident {right}"));
        (actual, mismatch)
    }

    /// Runs `query` on both handles, requires the identical output and returns it.
    fn run(&self, query: &str, vector: Option<&[f32]>) -> Outcome {
        let (actual, mismatch) = self.parity(query, vector);
        if let Some(message) = mismatch {
            panic!("{message}");
        }
        actual
    }

    fn hits(&self, query: &str) -> Vec<u64> {
        hit_ids(&self.run(query, Some(Q)).unwrap())
    }
}

fn parameters(vector: Option<&[f32]>) -> BTreeMap<String, Vec<f32>> {
    vector
        .into_iter()
        .map(|vector| ("q".to_owned(), vector.to_vec()))
        .collect()
}

fn execute(
    db: &Database,
    id: u16,
    query: &str,
    parameters: &BTreeMap<String, Vec<f32>>,
) -> Outcome {
    parse(query)
        .unwrap()
        .execute(CollectionRef { db, id }, parameters)
}

/// Hits, groups and facets, or the error, in one comparable string.
fn render(outcome: &Outcome) -> String {
    match outcome {
        Ok(output) => format!(
            "hits={:?} groups={:?} facets={:?}",
            output.hits, output.groups, output.facets
        ),
        Err(error) => format!("error={error:?}"),
    }
}

/// `ok` or the debug text of the error.
fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Ok(_) => "ok".to_owned(),
        Err(error) => format!("{error:?}"),
    }
}

fn hit_ids(output: &QueryOutput) -> Vec<u64> {
    output.hits.iter().map(|hit| hit.id).collect()
}

fn base_rows(db: &Database, id: u16) -> usize {
    let CollectionRecords::Compact { view, .. } = &db.state(id).records else {
        panic!("compact source required")
    };
    view.len()
}

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

fn row<const N: usize>(fields: [(&str, MetadataValue); N]) -> Metadata {
    Metadata::from(fields.map(|(name, value)| (name.to_owned(), value)))
}

/// A metadata value written the way a table writes it.
#[derive(Clone, Copy)]
enum V {
    S(&'static str),
    I(i64),
    B(bool),
    F(f64),
}

impl V {
    fn value(self) -> MetadataValue {
        match self {
            Self::S(value) => MetadataValue::String(value.into()),
            Self::I(value) => MetadataValue::Integer(value),
            Self::B(value) => MetadataValue::Boolean(value),
            Self::F(value) => MetadataValue::Float(value),
        }
    }
}

/// One query and the hits, groups and facets it must produce over `scene`, derived by hand.
struct Case {
    query: &'static str,
    hits: &'static [u64],
    groups: &'static [(V, &'static [u64])],
    facets: &'static [(V, u64)],
}

const fn only_hits(query: &'static str, hits: &'static [u64]) -> Case {
    Case {
        query,
        hits,
        groups: &[],
        facets: &[],
    }
}

/// The overlay over the base rows 1 (`[1, 1]` alpha "red apple") and 2 (`[2, 1]` beta "blue berry"):
///
/// | id | vector | tag | title | n | score | flag | deadline |
/// |----|--------|-----|-------|---|-------|------|----------|
/// | 1 | `[1, 1]` | alpha | red apple | 5 | 0.5 | true | none (replaces the base row) |
/// | 2 | | | | | | | tombstone |
/// | 3 | `[3, 1]` | alpha | apple pie | 3 | 1.5 | false | none |
/// | 4 | `[1, 3]` | beta | banana split | 9 | -2.5 | | none |
/// | 5 | `[2, 2]` | beta | apple tart | | 0.25 | true | none (and a `place`) |
/// | 6 | `[4, 4]` | | apple crumble | 1 | | | none |
/// | 7 | `[0, 0]` | alpha | apple cobbler apple | 7 | | | `FAR` |
/// | 8 | `[1, 2]` | gamma | apple pear | 3 | | | 1 (expired) |
///
/// For `$q = [1, 1]` the visible rows rank 1, 5, 7, 3, 4, 6 (squared distances 0, 2, 2, 4, 4, 18); row 8
/// would be at distance 1 if it were shown. BM25 for "apple" ranks 7 (two occurrences) first, then 1, 3,
/// 5 and 6 with equal scores (so by ID).
fn scene(collection: &mut CollectionMut<'_>) {
    let int = MetadataValue::Integer;
    let float = MetadataValue::Float;
    let flag = MetadataValue::Boolean;
    collection
        .put_with_metadata(
            1,
            &[1.0, 1.0],
            b"red apple",
            &row([
                ("tag", text("alpha")),
                ("title", text("red apple")),
                ("n", int(5)),
                ("score", float(0.5)),
                ("flag", flag(true)),
            ]),
        )
        .unwrap();
    collection.delete(2).unwrap();
    collection
        .put_with_metadata(
            3,
            &[3.0, 1.0],
            b"apple pie",
            &row([
                ("tag", text("alpha")),
                ("title", text("apple pie")),
                ("n", int(3)),
                ("score", float(1.5)),
                ("flag", flag(false)),
            ]),
        )
        .unwrap();
    collection
        .put_with_metadata(
            4,
            &[1.0, 3.0],
            b"banana split",
            &row([
                ("tag", text("beta")),
                ("title", text("banana split")),
                ("n", int(9)),
                ("score", float(-2.5)),
            ]),
        )
        .unwrap();
    collection
        .put_with_metadata(
            5,
            &[2.0, 2.0],
            b"apple tart",
            &row([
                ("tag", text("beta")),
                ("title", text("apple tart")),
                ("score", float(0.25)),
                ("flag", flag(true)),
                (
                    "place",
                    MetadataValue::GeoPoint {
                        latitude: 0.0,
                        longitude: 1.0,
                    },
                ),
            ]),
        )
        .unwrap();
    collection
        .put_with_metadata(
            6,
            &[4.0, 4.0],
            b"apple crumble",
            &row([("title", text("apple crumble")), ("n", int(1))]),
        )
        .unwrap();
    collection
        .put_with_expiry(
            7,
            &[0.0, 0.0],
            b"apple cobbler",
            &row([
                ("tag", text("alpha")),
                ("title", text("apple cobbler apple")),
                ("n", int(7)),
            ]),
            Some(FAR),
        )
        .unwrap();
    collection
        .put_with_expiry(
            8,
            &[1.0, 2.0],
            b"apple pear",
            &row([
                ("tag", text("gamma")),
                ("title", text("apple pear")),
                ("n", int(3)),
            ]),
            Some(1),
        )
        .unwrap();
}

/// Every clause, alone and combined, over `scene` with `$q = [1, 1]`. The ranks of a fused result (NEAR
/// and TEXT) are: vector order 1, 5, 7, 3, 4, 6 and text order 7, 1, 3, 5, 6 give the scores
/// 1/61 + 1/62 for 1, 1/63 + 1/61 for 7, 1/62 + 1/64 for 5, 1/64 + 1/63 for 3, 1/66 + 1/65 for 6 and
/// 1/65 for 4, so the order 1, 7, 5, 3, 6, 4.
const CASES: &[Case] = &[
    // The vector leg alone, and LIMIT: 0, 1, fewer than, exactly and more than the six hits.
    only_hits("SEARCH default NEAR $q", &[1, 5, 7, 3, 4, 6]),
    only_hits("SEARCH default NEAR $q LIMIT 0", &[]),
    only_hits("SEARCH default NEAR $q LIMIT 1", &[1]),
    only_hits("SEARCH default NEAR $q LIMIT 5", &[1, 5, 7, 3, 4]),
    only_hits("SEARCH default NEAR $q LIMIT 6", &[1, 5, 7, 3, 4, 6]),
    only_hits("SEARCH default NEAR $q LIMIT 7", &[1, 5, 7, 3, 4, 6]),
    only_hits("SEARCH default NEAR $q K 2", &[1, 5]),
    only_hits("SEARCH default NEAR $q K 2 LIMIT 3", &[1, 5, 7]),
    // The vector leg looks at every row, not at LIMIT of them: the nearest row is filtered out.
    only_hits(r#"search default near $q where tag = "beta" limit 1"#, &[5]),
    // Every comparison the parser supports.
    only_hits("SEARCH default NEAR $q WHERE n = 5", &[1]),
    only_hits("SEARCH default NEAR $q WHERE n < 5", &[3, 6]),
    only_hits("SEARCH default NEAR $q WHERE n <= 5", &[1, 3, 6]),
    only_hits("SEARCH default NEAR $q WHERE n > 5", &[7, 4]),
    only_hits("SEARCH default NEAR $q WHERE n >= 5", &[1, 7, 4]),
    only_hits("SEARCH default NEAR $q WHERE n IN (1, 9)", &[4, 6]),
    only_hits("SEARCH default NEAR $q WHERE tag EXISTS", &[1, 5, 7, 3, 4]),
    only_hits("SEARCH default NEAR $q WHERE EXISTS flag", &[1, 5, 3]),
    only_hits("SEARCH default NEAR $q WHERE flag = true", &[1, 5]),
    only_hits("SEARCH default NEAR $q WHERE flag = false", &[3]),
    only_hits("SEARCH default NEAR $q WHERE score >= 0.5", &[1, 3]),
    only_hits("SEARCH default NEAR $q WHERE score < 0.0", &[4]),
    only_hits("SEARCH default NEAR $q WHERE score = 0.25", &[5]),
    // An integer literal never compares with a float field.
    only_hits("SEARCH default NEAR $q WHERE score < 1", &[]),
    only_hits(r#"SEARCH default NEAR $q WHERE tag > "alpha""#, &[5, 4]),
    only_hits(
        r#"SEARCH default NEAR $q WHERE tag <= "beta""#,
        &[1, 5, 7, 3, 4],
    ),
    only_hits(
        r#"SEARCH default NEAR $q WHERE tag = "alpha" AND n > 3"#,
        &[1, 7],
    ),
    only_hits(
        r#"SEARCH default NEAR $q WHERE tag IN ("beta", "gamma")"#,
        &[5, 4],
    ),
    only_hits(
        "SEARCH default NEAR $q WHERE GEO(place, 0.0, 1.0, 10.0)",
        &[5],
    ),
    only_hits(
        "SEARCH default NEAR $q WHERE GEO(place, 45.0, 45.0, 10.0)",
        &[],
    ),
    // ORDER BY distance, id and metadata fields, both ways; rows without the field sort last when
    // ascending and first when descending; equal keys fall back to the ID.
    only_hits(
        "SEARCH default NEAR $q ORDER BY distance",
        &[1, 5, 7, 3, 4, 6],
    ),
    only_hits(
        "SEARCH default NEAR $q ORDER BY distance DESC",
        &[6, 3, 4, 5, 7, 1],
    ),
    only_hits("SEARCH default NEAR $q ORDER BY id", &[1, 3, 4, 5, 6, 7]),
    only_hits(
        "SEARCH default NEAR $q ORDER BY id DESC",
        &[7, 6, 5, 4, 3, 1],
    ),
    only_hits("SEARCH default NEAR $q ORDER BY n", &[6, 3, 1, 7, 4, 5]),
    only_hits("SEARCH default NEAR $q ORDER BY n ASC", &[6, 3, 1, 7, 4, 5]),
    only_hits(
        "SEARCH default NEAR $q ORDER BY n DESC",
        &[5, 4, 7, 1, 3, 6],
    ),
    only_hits("SEARCH default NEAR $q ORDER BY tag", &[1, 3, 7, 4, 5, 6]),
    only_hits(
        "SEARCH default NEAR $q ORDER BY tag DESC",
        &[6, 4, 5, 1, 3, 7],
    ),
    only_hits("SEARCH default NEAR $q ORDER BY score", &[4, 5, 1, 3, 6, 7]),
    only_hits(
        "SEARCH default NEAR $q ORDER BY score DESC",
        &[6, 7, 3, 1, 5, 4],
    ),
    only_hits("SEARCH default NEAR $q ORDER BY flag", &[3, 1, 5, 4, 6, 7]),
    only_hits(
        "SEARCH default NEAR $q ORDER BY flag DESC",
        &[4, 6, 7, 1, 5, 3],
    ),
    only_hits("SEARCH default NEAR $q ORDER BY title", &[7, 6, 3, 5, 4, 1]),
    only_hits(
        "SEARCH default NEAR $q ORDER BY title DESC",
        &[1, 4, 5, 3, 6, 7],
    ),
    only_hits(
        "SEARCH default NEAR $q ORDER BY nosuch",
        &[1, 3, 4, 5, 6, 7],
    ),
    only_hits(
        "SEARCH default NEAR $q ORDER BY nosuch DESC",
        &[1, 3, 4, 5, 6, 7],
    ),
    only_hits("SEARCH default NEAR $q ORDER BY n DESC LIMIT 2", &[5, 4]),
    only_hits(
        r#"SEARCH default NEAR $q WHERE tag = "alpha" ORDER BY n DESC"#,
        &[7, 1, 3],
    ),
    // GROUP BY: groups appear in order of their first hit, hold every hit of the value (not only the
    // first LIMIT hits) and are at most LIMIT in number; rows without the field are in no group.
    Case {
        query: "SEARCH default NEAR $q GROUP BY tag",
        hits: &[1, 5, 7, 3, 4, 6],
        groups: &[(V::S("alpha"), &[1, 7, 3]), (V::S("beta"), &[5, 4])],
        facets: &[],
    },
    Case {
        query: "SEARCH default NEAR $q GROUP BY tag LIMIT 1",
        hits: &[1],
        groups: &[(V::S("alpha"), &[1, 7, 3])],
        facets: &[],
    },
    Case {
        query: "SEARCH default NEAR $q GROUP BY tag LIMIT 0",
        hits: &[],
        groups: &[],
        facets: &[],
    },
    Case {
        query: "SEARCH default NEAR $q GROUP BY n LIMIT 3",
        hits: &[1, 5, 7],
        groups: &[(V::I(5), &[1]), (V::I(7), &[7]), (V::I(3), &[3])],
        facets: &[],
    },
    // FACET BY: counts over every hit before LIMIT, in order of first occurrence.
    Case {
        query: "SEARCH default NEAR $q FACET BY tag",
        hits: &[1, 5, 7, 3, 4, 6],
        groups: &[],
        facets: &[(V::S("alpha"), 3), (V::S("beta"), 2)],
    },
    Case {
        query: "SEARCH default NEAR $q FACET tag LIMIT 2",
        hits: &[1, 5],
        groups: &[],
        facets: &[(V::S("alpha"), 3), (V::S("beta"), 2)],
    },
    Case {
        query: "SEARCH default NEAR $q GROUP BY tag FACET BY tag LIMIT 2",
        hits: &[1, 5],
        groups: &[(V::S("alpha"), &[1, 7, 3]), (V::S("beta"), &[5, 4])],
        facets: &[(V::S("alpha"), 3), (V::S("beta"), 2)],
    },
    Case {
        query: "SEARCH default NEAR $q FACET BY tag ORDER BY n DESC",
        hits: &[5, 4, 7, 1, 3, 6],
        groups: &[],
        facets: &[(V::S("beta"), 2), (V::S("alpha"), 3)],
    },
    Case {
        query: "SEARCH default NEAR $q FACET BY flag",
        hits: &[1, 5, 7, 3, 4, 6],
        groups: &[],
        facets: &[(V::B(true), 2), (V::B(false), 1)],
    },
    Case {
        query: "SEARCH default NEAR $q FACET BY score",
        hits: &[1, 5, 7, 3, 4, 6],
        groups: &[],
        facets: &[
            (V::F(0.5), 1),
            (V::F(0.25), 1),
            (V::F(1.5), 1),
            (V::F(-2.5), 1),
        ],
    },
    // Text alone: BM25 order, the deleted row 2 ("berry") and the expired row 8 ("pear") are gone.
    only_hits(r#"SEARCH TEXT MATCH "apple""#, &[7, 1, 3, 5, 6]),
    only_hits(r#"SEARCH TEXT MATCH "apple" LIMIT 2"#, &[7, 1]),
    only_hits(
        r#"SEARCH TEXT MATCH "apple" WHERE tag = "alpha""#,
        &[7, 1, 3],
    ),
    only_hits(r#"SEARCH TEXT MATCH "apple" ORDER BY id"#, &[1, 3, 5, 6, 7]),
    only_hits(
        r#"SEARCH TEXT MATCH "apple" ORDER BY id DESC"#,
        &[7, 6, 5, 3, 1],
    ),
    only_hits(r#"SEARCH TEXT MATCH "apple" VERSION v1"#, &[7, 1, 3, 5, 6]),
    only_hits(r#"SEARCH TEXT MATCH "berry""#, &[]),
    only_hits(r#"SEARCH TEXT MATCH "pear""#, &[]),
    only_hits(r#"SEARCH TEXT MATCH "!!!""#, &[]),
    only_hits(r#"SEARCH TEXT MATCH "red""#, &[1]),
    only_hits(r#"SEARCH TEXT MATCH "apple pie""#, &[3, 7, 1, 5, 6]),
    // GROUP BY over a text result: row 6 has no tag.
    Case {
        query: r#"SEARCH TEXT MATCH "apple" GROUP BY tag"#,
        hits: &[7, 1, 3, 5, 6],
        groups: &[(V::S("alpha"), &[7, 1, 3]), (V::S("beta"), &[5])],
        facets: &[],
    },
    // The text leg looks at every match, not at LIMIT of them: the facets count all five.
    Case {
        query: r#"SEARCH TEXT MATCH "apple" FACET BY tag LIMIT 2"#,
        hits: &[7, 1],
        groups: &[],
        facets: &[(V::S("alpha"), 3), (V::S("beta"), 1)],
    },
    // Both legs, fused: the distance of a hit is the negated fused score.
    only_hits(
        r#"SEARCH default NEAR $q TEXT "apple""#,
        &[1, 7, 5, 3, 6, 4],
    ),
    only_hits(r#"SEARCH default NEAR $q TEXT "apple" LIMIT 3"#, &[1, 7, 5]),
    // The filter applies to each leg before fusion, so it changes the ranks that are fused.
    only_hits(
        r#"SEARCH default NEAR $q TEXT "apple" WHERE tag = "beta""#,
        &[5, 4],
    ),
    only_hits(
        r#"SEARCH default NEAR $q TEXT "apple" WHERE tag = "alpha""#,
        &[1, 7, 3],
    ),
    only_hits(
        r#"SEARCH default NEAR $q TEXT "apple" ORDER BY distance"#,
        &[1, 7, 5, 3, 6, 4],
    ),
    only_hits(
        r#"SEARCH default NEAR $q TEXT "apple" ORDER BY distance DESC"#,
        &[4, 6, 3, 5, 7, 1],
    ),
    only_hits(r#"SEARCH default NEAR $q TEXT "!!!""#, &[1, 5, 7, 3, 4, 6]),
    Case {
        query: r#"SEARCH default NEAR $q TEXT "apple" FACET BY tag LIMIT 2"#,
        hits: &[1, 7],
        groups: &[],
        facets: &[(V::S("alpha"), 3), (V::S("beta"), 2)],
    },
    Case {
        query: r#"SEARCH default NEAR $q TEXT "apple" WHERE tag EXISTS GROUP BY tag FACET BY tag ORDER BY title LIMIT 3"#,
        hits: &[7, 3, 5],
        groups: &[(V::S("alpha"), &[7, 3, 1]), (V::S("beta"), &[5, 4])],
        facets: &[(V::S("alpha"), 3), (V::S("beta"), 2)],
    },
];

/// Parity of every table query between the candidate and the oracle.
fn check_parity(pair: &Pair, state: &str, failures: &mut Vec<String>) {
    for case in CASES {
        let (_, mismatch) = pair.parity(case.query, Some(Q));
        failures.extend(mismatch.map(|message| format!("{state}: {message}")));
    }
}

/// The hand-derived hits, groups and facets of every table query, and parity.
fn check_expected(pair: &Pair, failures: &mut Vec<String>) {
    for case in CASES {
        let (outcome, mismatch) = pair.parity(case.query, Some(Q));
        failures.extend(mismatch);
        let output = match outcome {
            Ok(output) => output,
            Err(error) => {
                failures.push(format!("{}: unexpected error {error:?}", case.query));
                continue;
            }
        };
        let hits = hit_ids(&output);
        let groups: Vec<(MetadataValue, Vec<u64>)> = output
            .groups
            .iter()
            .map(|group| {
                (
                    group.value.clone(),
                    group.hits.iter().map(|hit| hit.id).collect(),
                )
            })
            .collect();
        let facets: Vec<(MetadataValue, u64)> = output
            .facets
            .iter()
            .map(|facet| (facet.value.clone(), facet.count))
            .collect();
        let want_groups: Vec<(MetadataValue, Vec<u64>)> = case
            .groups
            .iter()
            .map(|(value, ids)| (value.value(), ids.to_vec()))
            .collect();
        let want_facets: Vec<(MetadataValue, u64)> = case
            .facets
            .iter()
            .map(|(value, count)| (value.value(), *count))
            .collect();
        if hits != case.hits || groups != want_groups || facets != want_facets {
            failures.push(format!(
                "{}: expected hits {:?} groups {want_groups:?} facets {want_facets:?}, got hits {hits:?} groups {groups:?} facets {facets:?}",
                case.query, case.hits
            ));
        }
    }
}

#[test]
fn query_clauses_match_resident_before_and_after_overlay_mutations() {
    let _gates = gates();
    let mut failures = Vec::new();

    // The unmodified image: only base rows 1 and 2.
    let before = Pair::indexed();
    check_parity(&before, "base image", &mut failures);
    assert_eq!(before.hits("SEARCH default NEAR $q"), [1, 2]);
    assert_eq!(before.hits(r#"SEARCH TEXT MATCH "berry""#), [2]);
    assert_eq!(before.hits(r#"SEARCH TEXT MATCH "apple""#), [1]);
    assert_eq!(
        before.hits(r#"SEARCH default NEAR $q WHERE tag = "beta""#),
        [2]
    );

    // The lane's standard mutation: row 1 replaced, row 2 deleted, rows 7 (deadline FAR), 8 and 9 added.
    let mut lane = Pair::indexed();
    lane.mutate_databases(|db| mutate_indexed_rows(db, FAR));
    check_parity(&lane, "lane mutation", &mut failures);
    assert_eq!(lane.hits("SEARCH default NEAR $q"), [9, 8, 7, 1]);
    assert!(lane.hits(r#"SEARCH TEXT MATCH "berry""#).is_empty());
    assert_eq!(
        lane.hits(r#"SEARCH default NEAR $q WHERE tag = "gamma""#),
        [1]
    );
    let facets = lane
        .run("SEARCH default NEAR $q FACET BY tag", Some(Q))
        .unwrap()
        .facets;
    assert_eq!(
        facets
            .iter()
            .map(|facet| (facet.value.clone(), facet.count))
            .collect::<Vec<_>>(),
        [(text("alpha"), 3), (text("gamma"), 1)]
    );

    // The scene: every row kind of the overlay, with hand-derived expectations.
    let mut scene_pair = Pair::indexed();
    scene_pair.mutate(scene);
    check_expected(&scene_pair, &mut failures);

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// What a named-vector query must answer.
enum Want {
    Ids(&'static [u64]),
    Error(&'static str),
}

/// `full` as built: the named vector `n` holds [0.3, 0.0] in version v1 for rows 0, 3, 10 and MAX and
/// version v2 only for row 0; the primary vectors are [0.1, 0.0] (rows 0, 3, 10, MAX) and [0.25, 0.5].
const NAMED_BEFORE: &[(&str, Want)] = &[
    ("SEARCH n NEAR $q", Want::Ids(&[0, 3, 10, u64::MAX])),
    (
        "SEARCH n NEAR $q VERSION v1",
        Want::Ids(&[0, 3, 10, u64::MAX]),
    ),
    ("SEARCH n NEAR $q VERSION v2", Want::Ids(&[0])),
    ("SEARCH n NEAR $q VERSION v9", Want::Ids(&[])),
    ("SEARCH n NEAR $q LIMIT 2", Want::Ids(&[0, 3])),
    ("SEARCH nosuch NEAR $q", Want::Error(UNKNOWN_VECTOR)),
    (
        "SEARCH default NEAR $q VERSION v1",
        Want::Error(VERSION_ON_DEFAULT),
    ),
    (
        "SEARCH default NEAR $q",
        Want::Ids(&[0, 3, 10, u64::MAX, 20]),
    ),
    (r#"SEARCH n NEAR $q WHERE id = "10""#, Want::Ids(&[10])),
    (
        "SEARCH n NEAR $q WHERE flag = true ORDER BY id DESC LIMIT 3",
        Want::Ids(&[u64::MAX, 10, 3]),
    ),
    (
        "SEARCH n NEAR $q WHERE GEO(place, 0.0, 1.0, 1.0)",
        Want::Ids(&[0, 3, 10, u64::MAX]),
    ),
    (
        "SEARCH n NEAR $q WHERE integer < 0",
        Want::Ids(&[0, 3, 10, u64::MAX]),
    ),
];

/// After row 10 is deleted, row 3 is rewritten with a deadline in the past (its named vector kept in the
/// store, as in the named-vector visibility test) and row 7 is added at [0.4, 0.0].
const NAMED_AFTER: &[(&str, Want)] = &[
    ("SEARCH n NEAR $q", Want::Ids(&[0, u64::MAX])),
    ("SEARCH n NEAR $q VERSION v2", Want::Ids(&[0])),
    ("SEARCH default NEAR $q", Want::Ids(&[7, 0, u64::MAX, 20])),
    (
        "SEARCH n NEAR $q WHERE flag = true",
        Want::Ids(&[0, u64::MAX]),
    ),
    (
        "SEARCH default NEAR $q WHERE flag = true ORDER BY id DESC",
        Want::Ids(&[u64::MAX, 7, 0]),
    ),
    (r#"SEARCH n NEAR $q WHERE id = "10""#, Want::Ids(&[])),
    (r#"SEARCH n NEAR $q WHERE id = "3""#, Want::Ids(&[])),
];

/// With the text field `id` declared (each row's decimal ID is its one token): the fusion of a named or
/// default vector leg with the text leg. Rows 0, 3, 10 and MAX tie in the named vector leg (by ID), so the
/// text hit moves up by one rank; with VERSION v2 the vector leg holds row 0 only and ties with the text
/// hit 10 (both 1/61), so the lower ID comes first.
const NAMED_TEXT_BEFORE: &[(&str, Want)] = &[
    (
        r#"SEARCH n NEAR $q TEXT "10""#,
        Want::Ids(&[10, 0, 3, u64::MAX]),
    ),
    (
        r#"SEARCH n NEAR $q TEXT "3""#,
        Want::Ids(&[3, 0, 10, u64::MAX]),
    ),
    (
        r#"SEARCH n NEAR $q VERSION v2 TEXT "10""#,
        Want::Ids(&[0, 10]),
    ),
    (r#"SEARCH TEXT MATCH "10""#, Want::Ids(&[10])),
];

/// After the mutation: row 10 is deleted and row 3 expired (both leave the text leg and the vector leg),
/// row 7 is new.
const NAMED_TEXT_AFTER: &[(&str, Want)] = &[
    (r#"SEARCH n NEAR $q TEXT "10""#, Want::Ids(&[0, u64::MAX])),
    (r#"SEARCH n NEAR $q TEXT "3""#, Want::Ids(&[0, u64::MAX])),
    (r#"SEARCH TEXT MATCH "3""#, Want::Ids(&[])),
    (
        r#"SEARCH default NEAR $q TEXT "7""#,
        Want::Ids(&[7, 0, u64::MAX, 20]),
    ),
];

fn check_named(pair: &Pair, table: &[(&str, Want)], failures: &mut Vec<String>) {
    for (query, want) in table {
        let (outcome, mismatch) = pair.parity(query, Some(NAMED_Q));
        failures.extend(mismatch);
        let expected = match want {
            Want::Ids(ids) => format!("{ids:?}"),
            Want::Error(error) => (*error).to_owned(),
        };
        let observed = match &outcome {
            Ok(output) => format!("{:?}", hit_ids(output)),
            Err(error) => format!("{error:?}"),
        };
        if observed != expected {
            failures.push(format!("{query}: expected {expected}, got {observed}"));
        }
    }
}

#[test]
fn query_named_vectors_match_resident_on_the_full_collection() {
    let _gates = gates();
    let mut failures = Vec::new();
    let mut pair = Pair::full();
    check_named(&pair, NAMED_BEFORE, &mut failures);

    // Declare the text field `id` on both handles.
    pair.mutate_databases(|db| {
        let mut settings = db.collection("full").unwrap().settings().clone();
        settings.text_fields = vec!["id".into()];
        db.set_collection_settings("full", settings).unwrap();
    });
    check_named(&pair, NAMED_TEXT_BEFORE, &mut failures);

    pair.mutate_databases(|db| {
        let id = db.collection("full").unwrap().id();
        {
            let mut collection = db.collection_mut("full").unwrap();
            collection.delete(10).unwrap();
            collection
                .put_with_expiry(
                    3,
                    &[0.1, -0.0],
                    b"expired",
                    &row([("id", text("3")), ("flag", MetadataValue::Boolean(true))]),
                    Some(1),
                )
                .unwrap();
            collection
                .put_with_metadata(
                    7,
                    &[0.4, 0.0],
                    b"new",
                    &row([("id", text("7")), ("flag", MetadataValue::Boolean(true))]),
                )
                .unwrap();
        }
        // The named-vector store is independent of the records: keep the vector of the expired row.
        db.state_mut(id).vectors.records.insert(
            3,
            RecordVectors {
                dense: BTreeMap::from([(
                    "n".into(),
                    vec![VersionedVector {
                        version: "v1".into(),
                        vector: vec![0.3, -0.0],
                    }],
                )]),
                sparse: BTreeMap::new(),
            },
        );
    });
    check_named(&pair, NAMED_AFTER, &mut failures);
    check_named(&pair, NAMED_TEXT_AFTER, &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    // The expired row 3 and the deleted row 10 are in no facet; rows 0, 7 and MAX are.
    let output = pair
        .run("SEARCH default NEAR $q FACET BY id", Some(NAMED_Q))
        .unwrap();
    assert_eq!(
        output
            .facets
            .iter()
            .map(|facet| (facet.value.clone(), facet.count))
            .collect::<Vec<_>>(),
        [
            (text("7"), 1),
            (text("0"), 1),
            (text(&u64::MAX.to_string()), 1)
        ]
    );
}

type ErrorCase = (&'static str, Option<&'static [f32]>, &'static str);

const ERRORS_INDEXED: &[ErrorCase] = &[
    ("SEARCH default NEAR $q", None, MISSING),
    ("SEARCH default NEAR $q", Some(&[1.0]), DIMENSION_SHORT),
    ("SEARCH default NEAR $q", Some(&[f32::NAN, 1.0]), NON_FINITE),
    (
        "SEARCH default NEAR $q VERSION v1",
        Some(&[1.0, 1.0]),
        VERSION_ON_DEFAULT,
    ),
    // A missing parameter is reported before a version on the default vector.
    ("SEARCH default NEAR $q VERSION v1", None, MISSING),
    ("SEARCH nosuch NEAR $q", Some(&[1.0, 1.0]), UNKNOWN_VECTOR),
    // The vector leg's own error comes before a version problem is looked at.
    ("SEARCH nosuch NEAR $q VERSION v1", None, MISSING),
];

/// `full` declares no text field, so every text clause is an error, but only after the vector leg. (The
/// `IndexNotReady` answer for an unavailable text index is not covered: `TextIndex::limit` and `available`
/// are private to `text_index.rs` and `MAX_TEXT_POSTINGS` is out of reach of a test.)
const ERRORS_FULL: &[ErrorCase] = &[
    (r#"SEARCH TEXT MATCH "apple""#, None, UNDECLARED),
    (
        r#"SEARCH default NEAR $q TEXT "apple""#,
        Some(&[1.0, 1.0]),
        UNDECLARED,
    ),
    (r#"SEARCH default NEAR $q TEXT "apple""#, None, MISSING),
    (
        r#"SEARCH default NEAR $q TEXT "apple""#,
        Some(&[1.0]),
        DIMENSION_SHORT,
    ),
    (
        r#"SEARCH default NEAR $q VERSION v1 TEXT "apple""#,
        Some(&[1.0, 1.0]),
        VERSION_ON_DEFAULT,
    ),
    (
        r#"SEARCH nosuch NEAR $q TEXT "apple""#,
        Some(&[1.0, 1.0]),
        UNKNOWN_VECTOR,
    ),
    (r#"SEARCH TEXT MATCH "!!!""#, None, UNDECLARED),
];

#[test]
fn query_errors_match_resident_and_precede_row_materialization() {
    let _gates = gates();
    for (pair, table) in [
        (Pair::indexed(), ERRORS_INDEXED),
        (Pair::full(), ERRORS_FULL),
    ] {
        let (db, id) = (&pair.candidate, pair.candidate_id);
        assert_eq!(decode_count(db, id), 0);
        for (query, vector, expected) in table {
            let (outcome, mismatch) = pair.parity(query, *vector);
            assert!(mismatch.is_none(), "{}", mismatch.unwrap_or_default());
            assert_eq!(outcome_text(&outcome), *expected, "{query} {vector:?}");
            assert_eq!(
                decode_count(db, id),
                0,
                "{query} {vector:?} must fail before any row is decoded"
            );
        }
    }
    // The counter does move: a text query decodes rows through the effective iterator, so the zeros above mean something.
    let pair = Pair::indexed();
    let (db, id) = (&pair.candidate, pair.candidate_id);
    assert!(base_rows(db, id) > 0);
    assert_eq!(pair.hits(r#"SEARCH TEXT MATCH "apple""#), [1]);
    assert!(decode_count(db, id) > 0);
}

/// Valid queries and queries that are invalid in several ways at once.
const POISONED_QUERIES: &[(&str, Option<&[f32]>)] = &[
    ("SEARCH default NEAR $q", Some(&[1.0, 1.0])),
    (r#"SEARCH TEXT MATCH "apple""#, None),
    (
        r#"SEARCH default NEAR $q TEXT "apple" WHERE tag EXISTS ORDER BY title LIMIT 2"#,
        Some(&[1.0, 1.0]),
    ),
    ("SEARCH default NEAR $q", None),
    ("SEARCH default NEAR $q VERSION v1", None),
    (
        r#"SEARCH nosuch NEAR $q VERSION v1 TEXT "apple""#,
        Some(&[1.0]),
    ),
];

#[test]
fn query_health_precedes_every_validation_and_every_valid_query() {
    let _gates = gates();
    for mut pair in [Pair::indexed(), Pair::full()] {
        pair.candidate.poisoned = true;
        pair.oracle.poisoned = true;
        for (query, vector) in POISONED_QUERIES {
            let parameters = parameters(*vector);
            for (side, db, id) in [
                ("compact", &pair.candidate, pair.candidate_id),
                ("resident", &pair.oracle, pair.oracle_id),
            ] {
                let outcome = execute(db, id, query, &parameters);
                assert_eq!(
                    outcome_text(&outcome),
                    POISONED,
                    "{side} {query} {vector:?}"
                );
            }
        }
    }
}

#[test]
fn query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest() {
    let _gates = gates();
    let mut pair = Pair::indexed();
    pair.mutate(scene);
    // Row 2 is a tombstone over a base row and row 8's deadline passed: neither is in any hit or group.
    for query in [
        "SEARCH default NEAR $q",
        r#"SEARCH TEXT MATCH "apple""#,
        r#"SEARCH TEXT MATCH "pear""#,
        r#"SEARCH default NEAR $q TEXT "apple""#,
        "SEARCH default NEAR $q GROUP BY tag",
        "SEARCH default NEAR $q GROUP BY title",
        "SEARCH default NEAR $q WHERE n = 3",
    ] {
        let output = pair.run(query, Some(Q)).unwrap();
        let seen: BTreeSet<u64> = hit_ids(&output)
            .into_iter()
            .chain(
                output
                    .groups
                    .iter()
                    .flat_map(|group| group.hits.iter().map(|hit| hit.id)),
            )
            .collect();
        assert!(!seen.contains(&2), "{query}: the tombstoned base row");
        assert!(!seen.contains(&8), "{query}: the expired row");
    }
    // Row 3 is the only live row with `n` = 3 (row 8 has it too, but is expired).
    assert_eq!(pair.hits("SEARCH default NEAR $q WHERE n = 3"), [3]);
    // Row 7 (a far deadline) and the overlay-only rows 3 to 6 are shown.
    let shown = pair.hits("SEARCH default NEAR $q");
    for id in [3, 4, 5, 6, 7] {
        assert!(shown.contains(&id), "row {id} must be shown: {shown:?}");
    }
    // Facets count neither the tombstoned row's value ("blue berry") nor the expired row's ("gamma").
    let tags = pair
        .run("SEARCH default NEAR $q FACET BY tag", Some(Q))
        .unwrap()
        .facets;
    assert_eq!(
        tags.iter()
            .map(|facet| (facet.value.clone(), facet.count))
            .collect::<Vec<_>>(),
        [(text("alpha"), 3), (text("beta"), 2)]
    );
    let titles = pair
        .run("SEARCH default NEAR $q FACET BY title", Some(Q))
        .unwrap()
        .facets
        .into_iter()
        .map(|facet| facet.value)
        .collect::<Vec<_>>();
    for title in ["apple cobbler apple", "apple tart", "apple crumble"] {
        assert!(titles.contains(&text(title)), "{title}: {titles:?}");
    }
    for title in ["blue berry", "apple pear"] {
        assert!(!titles.contains(&text(title)), "{title}: {titles:?}");
    }
}
