//! The unavailable-text-index arms of the compact hybrid search and of `execute_compact`.
//!
//! The text index documents the rule: exceeding its posting limit marks it unavailable ("text queries return
//! `IndexNotReady`") without rejecting a durable write, until the declarations change or the file is reopened. So a
//! query that uses the text leg answers `IndexNotReady` and a query that does not use it is not affected. The
//! expectations below are that rule: the literal error, or the answer the same query gave before the index was
//! exhausted. The resident store with the same unavailable index is the oracle for everything else.

use super::{assert_compact_resident_pair, indexed_dispatch_fixture};
use crate::compact::source::dispatch_tests::gates;
use crate::hybrid::{FusionOptions, HybridQuery};
use crate::query_language::parse;
use crate::text_index::TextIndex;
use crate::{CollectionRef, Condition, Database};
use std::collections::BTreeMap;

const NOW_MS: u64 = 1_000;
const NOT_READY: &str = "Err(IndexNotReady)";
const POISONED: &str = "Err(Poisoned)";
const BAD_RANGE: &str = r#"Err(InvalidFilter("range requires min or max"))"#;
const Q: [f32; 2] = [1.0, 1.0];

/// A compact candidate and its resident oracle over `indexed` (text field `title`, rows 1 and 2).
struct Pair {
    candidate: Database,
    oracle: Database,
    candidate_id: u16,
    oracle_id: u16,
}

impl Pair {
    fn new() -> Self {
        let (candidate, oracle) = indexed_dispatch_fixture();
        assert_compact_resident_pair(&candidate, &oracle, "indexed");
        let candidate_id = candidate.collection("indexed").unwrap().id();
        let oracle_id = oracle.collection("indexed").unwrap().id();
        Self {
            candidate,
            oracle,
            candidate_id,
            oracle_id,
        }
    }

    /// Leaves the text index of both handles as a write over the posting limit leaves it (`exhausted`) or rebuilds it
    /// with the default limit; both go through the rebuild that a reopen or a declaration change runs.
    fn set_text_index(&mut self, exhausted: bool) {
        for (db, id) in [
            (&mut self.candidate, self.candidate_id),
            (&mut self.oracle, self.oracle_id),
        ] {
            let state = db.state_mut(id);
            state.text_index = if exhausted {
                TextIndex::with_posting_limit(1)
            } else {
                TextIndex::default()
            };
            state.rebuild_derived().unwrap();
            let status = db.collection("indexed").unwrap().text_index_status();
            assert_eq!(status.declared_fields, 1);
            assert_eq!(status.available, !exhausted);
        }
    }

    /// The answer to a hybrid query, which the compact and the resident store must give alike.
    fn hybrid(&self, query: &HybridQuery) -> String {
        let answer = |db: &Database, id: u16| {
            format!(
                "{:?}",
                CollectionRef { db, id }.search_hybrid_at(query, NOW_MS)
            )
        };
        let actual = answer(&self.candidate, self.candidate_id);
        assert_eq!(
            actual,
            answer(&self.oracle, self.oracle_id),
            "compact differs from resident for {query:?}"
        );
        actual
    }

    /// The answer to a query-language statement, which the compact and the resident store must give alike.
    fn language(&self, query: &str) -> String {
        let parameters = BTreeMap::from([("q".to_owned(), Q.to_vec())]);
        let answer = |db: &Database, id: u16| {
            let parsed = parse(query).unwrap();
            format!(
                "{:?}",
                parsed.execute(CollectionRef { db, id }, &parameters)
            )
        };
        let actual = answer(&self.candidate, self.candidate_id);
        assert_eq!(
            actual,
            answer(&self.oracle, self.oracle_id),
            "compact differs from resident for {query}"
        );
        actual
    }
}

fn with_fusion(fusion: FusionOptions) -> HybridQuery {
    HybridQuery {
        fusion,
        ..HybridQuery::new(Some(Q.to_vec()), "apple", 10)
    }
}

#[test]
fn compact_text_queries_answer_index_not_ready_while_the_index_is_unavailable() {
    let _gates = gates();
    let mut pair = Pair::new();
    // Each query and whether it uses the text index: the text leg runs with a positive weight and at least one token,
    // and it is checked before `k = 0` returns early.
    let hybrids = [
        (
            "vector and text",
            HybridQuery::new(Some(Q.to_vec()), "apple", 10),
            true,
        ),
        ("text only", HybridQuery::new(None, "apple", 10), true),
        (
            "text leg only, by weight",
            with_fusion(FusionOptions {
                vector_weight: 0.0,
                ..FusionOptions::default()
            }),
            true,
        ),
        ("text with k = 0", HybridQuery::new(None, "apple", 0), true),
        (
            "vector only",
            HybridQuery::new(Some(Q.to_vec()), "", 10),
            false,
        ),
        (
            "text without a token",
            HybridQuery::new(Some(Q.to_vec()), "!!!", 10),
            false,
        ),
        (
            "text leg of weight 0",
            with_fusion(FusionOptions {
                text_weight: 0.0,
                ..FusionOptions::default()
            }),
            false,
        ),
    ];
    // A text clause is checked before its tokens are looked at, so even one without a token needs the index.
    let languages = [
        (r#"SEARCH TEXT MATCH "apple""#, true),
        (r#"SEARCH TEXT MATCH "!!!""#, true),
        (r#"SEARCH default NEAR $q TEXT "apple""#, true),
        ("SEARCH default NEAR $q", false),
        (r#"SEARCH default NEAR $q WHERE tag = "alpha""#, false),
    ];

    let healthy_hybrids: Vec<String> = hybrids
        .iter()
        .map(|(label, query, _)| {
            let answer = pair.hybrid(query);
            assert!(answer.starts_with("Ok("), "{label}: {answer}");
            answer
        })
        .collect();
    let healthy_languages: Vec<String> = languages
        .iter()
        .map(|(query, _)| {
            let answer = pair.language(query);
            assert!(answer.starts_with("Ok("), "{query}: {answer}");
            answer
        })
        .collect();

    pair.set_text_index(true);
    for ((label, query, blocked), healthy) in hybrids.iter().zip(&healthy_hybrids) {
        let answer = pair.hybrid(query);
        if *blocked {
            assert_eq!(answer, NOT_READY, "{label}");
        } else {
            assert_eq!(
                &answer, healthy,
                "{label}: a query without the text leg is unaffected"
            );
        }
    }
    for ((query, blocked), healthy) in languages.iter().zip(&healthy_languages) {
        let answer = pair.language(query);
        if *blocked {
            assert_eq!(answer, NOT_READY, "{query}");
        } else {
            assert_eq!(
                &answer, healthy,
                "{query}: a query without text is unaffected"
            );
        }
    }

    // Validation comes first: an invalid filter is refused before the index is looked at.
    let invalid = HybridQuery {
        conditions: vec![Condition::Range {
            field: "tag".into(),
            min: None,
            max: None,
        }],
        ..HybridQuery::new(Some(Q.to_vec()), "apple", 10)
    };
    assert_eq!(pair.hybrid(&invalid), BAD_RANGE);

    // A rebuilt index answers again, as it did before.
    pair.set_text_index(false);
    for ((label, query, _), healthy) in hybrids.iter().zip(&healthy_hybrids) {
        assert_eq!(&pair.hybrid(query), healthy, "{label}: rebuilt");
    }
    for ((query, _), healthy) in languages.iter().zip(&healthy_languages) {
        assert_eq!(&pair.language(query), healthy, "{query}: rebuilt");
    }
}

#[test]
fn compact_text_queries_report_a_poisoned_handle_before_an_unavailable_index() {
    let _gates = gates();
    let mut pair = Pair::new();
    pair.set_text_index(true);
    pair.candidate.poisoned = true;
    pair.oracle.poisoned = true;
    assert_eq!(pair.hybrid(&HybridQuery::new(None, "apple", 10)), POISONED);
    assert_eq!(pair.language(r#"SEARCH TEXT MATCH "apple""#), POISONED);
}
