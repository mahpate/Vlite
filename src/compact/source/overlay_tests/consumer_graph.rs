//! Graph retrieval over compact records: the pivot search and the evidence reads at the retrieval clock.
//!
//! Every retrieval runs on a compact candidate (`compact_graph_for_test`) and on a resident oracle (the same image with
//! source installation off, `graph`) at one explicit clock, after identical edits, and the two whole `Retrieval`s must
//! be equal once the timings are zeroed. Both arms share `retrieve_with_visibility`, so each test also pins literals
//! worked out from the freeze rules (a deadline hides a row at `T >= deadline`, the newest overlay entry decides,
//! an error from a base read is never absence) and from the retrieval contract: they are what a change to the shared
//! code has to answer to.

use super::super::dispatch_tests::{compact_dispatch_fixture, gates, point_parity, unsupported};
use super::assert_compact_resident_pair;
use super::consumer_vectors::{Tamper, is_corrupt, is_poisoned_cell, poison_cell, tamper_block};
use crate::property_graph::{
    EvidenceSpec, Expansion, GraphLimits, GraphRef, Pivot, PropertyProjection, ReadOptions,
    Retrieval, Retrieve, TruncationReason,
};
use crate::records::CollectionRecords;
use crate::{Condition, Database, Error, Expected, Metadata, MetadataValue, Mutation};

/// A deadline far in the future (2100-01-01), the base deadline of row 3 in the fixture.
const FAR: u64 = 4_102_444_800_000;
/// A clock at which nothing of the fixture has expired.
const NOW: u64 = 1;
const COLLECTIONS: [&str; 2] = ["half", "full"];
/// What a retrieval is expected to return at one clock: the clock, the pivot IDs, the evidence and the unmapped pivots.
type ClockRow<'a> = (u64, &'a [u64], &'a [(&'a str, u64)], &'a [u64]);

fn marker(value: &str) -> Metadata {
    Metadata::from([("marker".to_owned(), MetadataValue::String(value.to_owned()))])
}

fn phase(value: &str) -> Metadata {
    Metadata::from([("phase".to_owned(), MetadataValue::String(value.to_owned()))])
}

/// The deadline of a traversal is real time; its ceiling keeps a stalled machine from truncating a tiny graph.
fn options() -> ReadOptions<'static> {
    ReadOptions {
        limits: GraphLimits {
            deadline_ms: GraphLimits::CEILING.deadline_ms,
            ..GraphLimits::DEFAULT
        },
        ..ReadOptions::default()
    }
}

/// Ten pivots from `pivot` by an exact vector leg, and the attached records of `evidence` as evidence.
fn request(pivot: &str, vector: [f32; 2], evidence: &[&str]) -> Retrieve {
    let mut pivot = Pivot::new(pivot, 10);
    pivot.vector = Some(vector.to_vec());
    Retrieve {
        pivot,
        expansion: Expansion::default(),
        evidence: EvidenceSpec {
            collections: evidence.iter().map(|name| (*name).to_owned()).collect(),
            ..EvidenceSpec::default()
        },
    }
}

fn zeroed(mut retrieval: Retrieval) -> Retrieval {
    retrieval.stats.pivot_ms = 0;
    retrieval.stats.expand_ms = 0;
    retrieval.stats.expansion.elapsed_ms = 0;
    retrieval
}

fn compact_retrieve(db: &Database, now_ms: u64, request: &Retrieve) -> crate::Result<Retrieval> {
    db.compact_graph_for_test("g", &request.pivot.collection)?
        .with_clock(now_ms)
        .retrieve(request, &options())
        .map(zeroed)
}

fn resident_retrieve(db: &Database, now_ms: u64, request: &Retrieve) -> crate::Result<Retrieval> {
    db.graph("g")?
        .with_clock(now_ms)
        .retrieve(request, &options())
        .map(zeroed)
}

/// `request` on both arms at `now_ms`; the two retrievals have to be equal.
fn retrieve_both(
    candidate: &Database,
    oracle: &Database,
    now_ms: u64,
    request: &Retrieve,
) -> Retrieval {
    let actual = compact_retrieve(candidate, now_ms, request).unwrap();
    let expected = resident_retrieve(oracle, now_ms, request).unwrap();
    assert_eq!(actual, expected, "compact and resident differ at {now_ms}");
    actual
}

fn pivot_ids(retrieval: &Retrieval) -> Vec<u64> {
    retrieval.pivots.iter().map(|hit| hit.record_id).collect()
}

fn evidence_ids(retrieval: &Retrieval) -> Vec<(&str, u64)> {
    retrieval
        .evidence
        .iter()
        .map(|item| (item.collection.as_str(), item.record_id))
        .collect()
}

#[test]
fn compact_graph_evidence_follows_the_requested_collections_and_effective_rows() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let text = "héllo wörld";
    let half_metadata = Metadata::from([
        (
            "src".to_owned(),
            MetadataValue::String("half-overlay".to_owned()),
        ),
        ("n".to_owned(), MetadataValue::Integer(1)),
        ("extra".to_owned(), MetadataValue::Boolean(true)),
    ]);
    let full_metadata = marker("full-overlay");
    // The attached record of `half` is replaced, the one of `full` only gets new metadata, and a third commit leaves
    // the revisions of both attached records behind the view's sequence.
    let mut commits = Vec::new();
    for db in [&mut candidate, &mut oracle] {
        let half = db
            .collection_mut("half")
            .unwrap()
            .put_with_metadata(0, &[0.5, 0.25], text.as_bytes(), &half_metadata)
            .unwrap();
        let full = db
            .collection_mut("full")
            .unwrap()
            .update_metadata(0, &full_metadata)
            .unwrap();
        let view = db
            .collection_mut("half")
            .unwrap()
            .update_metadata(10, &Metadata::new())
            .unwrap();
        commits.push((half, full, view));
    }
    assert_eq!(commits[0], commits[1]);
    let (half_revision, full_revision, view_sequence) = commits[0];
    assert!(half_revision < full_revision && full_revision < view_sequence);
    assert_eq!(candidate.sequence, view_sequence);
    for name in COLLECTIONS {
        assert_compact_resident_pair(&candidate, &oracle, name);
    }
    // The pivot is in `full` and the evidence comes from `half`: row 0 of `full` is the pivot hit at rank 1 (distance
    // 0), every other hit is unattached and ranks by distance, then ascending ID (3, 10 and u64::MAX tie at 0, row 20
    // is farther), and the evidence is the overlay row of `half`.
    let result = retrieve_both(
        &candidate,
        &oracle,
        NOW,
        &request("full", [0.1, -0.0], &["half"]),
    );
    assert_eq!(pivot_ids(&result), [0]);
    let hit = &result.pivots[0];
    assert_eq!(hit.node_key, "n");
    assert_eq!(hit.vector_rank, Some(1));
    assert_eq!(hit.distance, Some(0.0));
    assert_eq!((hit.bm25, hit.text_rank), (None, None));
    assert_eq!(hit.score, 1.0 / 61.0);
    assert_eq!(result.unmapped_pivots.record_ids, [3, 10, u64::MAX, 20]);
    assert_eq!(result.unmapped_pivots.count, 4);
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(
        (result.nodes[0].depth, result.nodes[0].node.key.as_str()),
        (0, "n")
    );
    assert!(result.edges.is_empty() && result.paths.is_empty());
    assert_eq!(result.stats.pivot_candidates, 5);
    assert_eq!(result.stats.pivots_mapped, 1);
    assert_eq!(result.stats.pivots_rejected, 0);
    assert_eq!(result.stats.evidence_items, 1);
    assert_eq!(result.view_sequence, view_sequence);
    assert!(result.complete && result.truncation.is_empty());
    let [item] = &result.evidence[..] else {
        panic!("one evidence item: {:?}", result.evidence)
    };
    assert_eq!((item.collection.as_str(), item.record_id), ("half", 0));
    assert_eq!(item.node_id, hit.node_id);
    assert_eq!(item.revision, half_revision);
    assert_eq!(item.view_sequence, view_sequence);
    assert_eq!(item.payload_excerpt, text.as_bytes());
    assert_eq!(item.payload_bytes, 13);
    assert!(!item.excerpt_truncated);
    assert_eq!(item.metadata, half_metadata);

    // The other way round: the pivot is the overlay row of `half` (rank 1, then row 20 at distance 0.125, then the
    // base rows at about 0.2225) and the evidence is the row of `full` with its new metadata and its base payload.
    let swapped = request("half", [0.5, 0.25], &["full"]);
    let result = retrieve_both(&candidate, &oracle, NOW, &swapped);
    assert_eq!(pivot_ids(&result), [0]);
    assert_eq!(result.unmapped_pivots.record_ids, [20, 3, 10, u64::MAX]);
    let [item] = &result.evidence[..] else {
        panic!("one evidence item: {:?}", result.evidence)
    };
    assert_eq!((item.collection.as_str(), item.record_id), ("full", 0));
    assert_eq!(item.revision, full_revision);
    assert_eq!(item.payload_excerpt, 0u64.to_le_bytes());
    assert_eq!(item.payload_bytes, 8);
    assert_eq!(item.metadata, full_metadata);

    // Projection: All keeps every field, None keeps none, Only keeps the named fields that the row has.
    let half_src = Metadata::from([(
        "src".to_owned(),
        MetadataValue::String("half-overlay".to_owned()),
    )]);
    for (fields, full_want, half_want) in [
        (
            PropertyProjection::All,
            full_metadata.clone(),
            half_metadata.clone(),
        ),
        (PropertyProjection::None, Metadata::new(), Metadata::new()),
        (
            PropertyProjection::Only(vec!["marker".to_owned(), "absent".to_owned()]),
            marker("full-overlay"),
            Metadata::new(),
        ),
        (
            PropertyProjection::Only(vec!["src".to_owned()]),
            Metadata::new(),
            half_src,
        ),
        (
            PropertyProjection::Only(vec!["absent".to_owned()]),
            Metadata::new(),
            Metadata::new(),
        ),
    ] {
        let mut projected = request("full", [0.1, -0.0], &["full", "half"]);
        projected.evidence.fields = fields.clone();
        let result = retrieve_both(&candidate, &oracle, NOW, &projected);
        assert_eq!(evidence_ids(&result), [("full", 0), ("half", 0)]);
        assert_eq!(result.evidence[0].metadata, full_want, "{fields:?}");
        assert_eq!(result.evidence[1].metadata, half_want, "{fields:?}");
    }

    // The excerpt is cut at a character boundary: byte 2 and 9 fall inside a two-byte character.
    for (max, excerpt, truncated) in [
        (0, "", true),
        (2, "h", true),
        (3, "hé", true),
        (9, "héllo w", true),
        (13, text, false),
        (4_096, text, false),
    ] {
        let mut cut = request("full", [0.1, -0.0], &["half"]);
        cut.evidence.max_excerpt_bytes = max;
        let result = retrieve_both(&candidate, &oracle, NOW, &cut);
        let item = &result.evidence[0];
        assert_eq!(item.payload_excerpt, excerpt.as_bytes(), "max {max}");
        assert_eq!(item.excerpt_truncated, truncated, "max {max}");
        assert_eq!(item.payload_bytes, 13, "the length is the payload's");
    }

    // At most `max_items` items are returned in the requested collection order, then the output is cut and incomplete.
    for (order, max_items, want, limited) in [
        (["half", "full"], 0, vec![], true),
        (["half", "full"], 1, vec![("half", 0)], true),
        (["full", "half"], 1, vec![("full", 0)], true),
        (["half", "full"], 2, vec![("half", 0), ("full", 0)], false),
        (["full", "half"], 2, vec![("full", 0), ("half", 0)], false),
        (
            ["half", "full"],
            1_000,
            vec![("half", 0), ("full", 0)],
            false,
        ),
    ] {
        let mut capped = request("full", [0.1, -0.0], &order);
        capped.evidence.max_items = max_items;
        let result = retrieve_both(&candidate, &oracle, NOW, &capped);
        assert_eq!(evidence_ids(&result), want, "{order:?} max {max_items}");
        assert_eq!(result.stats.evidence_items, want.len() as u64);
        assert_eq!(result.complete, !limited, "{order:?} max {max_items}");
        assert_eq!(
            result.truncation,
            if limited {
                vec![TruncationReason::OutputLimit]
            } else {
                Vec::new()
            },
            "{order:?} max {max_items}"
        );
        assert_eq!(
            result.pivots.len(),
            1,
            "truncation never touches the pivots"
        );
    }

    // A collection that is listed twice projects once; one that is not listed projects nothing.
    let result = retrieve_both(
        &candidate,
        &oracle,
        NOW,
        &request("full", [0.1, -0.0], &["half", "half", "full"]),
    );
    assert_eq!(evidence_ids(&result), [("half", 0), ("full", 0)]);
    let result = retrieve_both(&candidate, &oracle, NOW, &request("full", [0.1, -0.0], &[]));
    assert!(result.evidence.is_empty() && result.complete);
    assert_eq!(result.stats.evidence_items, 0);
    assert_eq!(pivot_ids(&result), [0]);

    // The ceilings and unknown names are answered before anything is read.
    let mut too_many = request("full", [0.1, -0.0], &["half"]);
    too_many.evidence.max_items = 1_001;
    let mut too_long = request("full", [0.1, -0.0], &["half"]);
    too_long.evidence.max_excerpt_bytes = 4_097;
    for (bad, want) in [
        (&too_many, "max_evidence_items"),
        (&too_long, "max_excerpt_bytes"),
    ] {
        for result in [
            compact_retrieve(&candidate, NOW, bad),
            resident_retrieve(&oracle, NOW, bad),
        ] {
            assert!(
                matches!(&result, Err(Error::GraphLimitExceedsCeiling { limit, .. }) if *limit == want),
                "{want}: {result:?}"
            );
        }
    }
    let unknown = request("full", [0.1, -0.0], &["nope"]);
    for result in [
        compact_retrieve(&candidate, NOW, &unknown),
        resident_retrieve(&oracle, NOW, &unknown),
    ] {
        assert!(
            matches!(result, Err(Error::CollectionNotFound)),
            "{result:?}"
        );
    }
}

/// The record decodes done by the generation so far (it is shared by every collection of the image).
fn decodes(db: &Database) -> usize {
    let collection_id = db.collection("full").unwrap().id();
    let CollectionRecords::Compact { view, .. } = &db.state(collection_id).records else {
        panic!("compact source required")
    };
    view.generation
        .decode_count
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// The IDs of the rows of `collection` whose record cell has been decoded.
fn materialized(db: &Database, collection: &str) -> Vec<u64> {
    let collection_id = db.collection(collection).unwrap().id();
    let CollectionRecords::Compact { view, .. } = &db.state(collection_id).records else {
        panic!("compact source required")
    };
    let source = view.source().unwrap();
    (0..view.len())
        .filter(|&row| source.cells[row].record.get().is_some())
        .filter_map(|row| view.id(row))
        .collect()
}

#[test]
fn compact_graph_evidence_decodes_only_the_attached_rows_it_projects() {
    let _gates = gates();
    // One fresh candidate per measurement: cells are never evicted, so a second call on the same generation would read
    // the rows that the first one decoded for free. Returns the decodes done by the call, the rows of `half` decoded
    // by then and the retrieval.
    let measure = |evidence: &[&str]| {
        let (candidate, _, _) = compact_dispatch_fixture(false);
        let before = decodes(&candidate);
        let result =
            compact_retrieve(&candidate, NOW, &request("full", [0.1, -0.0], evidence)).unwrap();
        (
            decodes(&candidate) - before,
            materialized(&candidate, "half"),
            result,
        )
    };

    // Without evidence the call is the pivot phase alone. It scans `full` only: the exact scan reads vectors and
    // metadata through the bounded workspace and decodes no cell, and the re-validation of each hit decodes the cell of
    // that hit's row (rows of `full`). Not one row of `half` is decoded.
    let (pivot_only, half_rows, result) = measure(&[]);
    assert!(pivot_only > 0, "the pivot hits are re-validated row by row");
    assert!(half_rows.is_empty(), "the pivot phase never reads `half`");
    assert!(result.evidence.is_empty());

    // With evidence from `half` the call adds exactly one decode: the attached row 0. The four other rows of `half`
    // (3, 10, 20 and u64::MAX) would add four more if the evidence phase walked the collection instead of the node's
    // attachments, and the pivot hits are not read again (their cells are decoded already).
    let (with_half, half_rows, result) = measure(&["half"]);
    assert_eq!(with_half, pivot_only + 1);
    assert_eq!(half_rows, [0]);
    assert_eq!(evidence_ids(&result), [("half", 0)]);

    // Evidence from `full` needs the row that the pivot phase decoded for its hit, so it decodes nothing more and
    // leaves `half` alone.
    let (with_full, half_rows, result) = measure(&["full"]);
    assert_eq!(with_full, pivot_only);
    assert!(half_rows.is_empty());
    assert_eq!(evidence_ids(&result), [("full", 0)]);
}

#[test]
fn compact_graph_retrieval_uses_one_explicit_clock_for_pivots_and_evidence() {
    let _gates = gates();
    for pivot_collection in COLLECTIONS {
        let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
        for db in [&mut candidate, &mut oracle] {
            for name in COLLECTIONS {
                db.collection_mut(name)
                    .unwrap()
                    .put_with_expiry(
                        7,
                        &[0.1, -0.0],
                        b"temporary pivot",
                        &Metadata::new(),
                        Some(1_000),
                    )
                    .unwrap();
            }
        }
        // The pivot hits rank by distance and then by ascending ID: 0 (attached, no deadline), 3 (base deadline FAR),
        // 7 (overlay-only, deadline 1000), 10, u64::MAX (no deadlines) and, farther, 20. Row 0 is the only mapped
        // pivot at every clock, also at u64::MAX; the unattached rows are what the deadlines show.
        let table: [(u64, &[u64]); 6] = [
            (0, &[3, 7, 10, u64::MAX, 20]),
            (999, &[3, 7, 10, u64::MAX, 20]),
            (1_000, &[3, 10, u64::MAX, 20]),
            (FAR - 1, &[3, 10, u64::MAX, 20]),
            (FAR, &[10, u64::MAX, 20]),
            (u64::MAX, &[10, u64::MAX, 20]),
        ];
        let request = request(pivot_collection, [0.1, -0.0], &["half", "full"]);
        for (now_ms, unmapped) in table {
            let result = retrieve_both(&candidate, &oracle, now_ms, &request);
            let at = format!("pivot in {pivot_collection} at {now_ms}");
            assert_eq!(pivot_ids(&result), [0], "{at}");
            assert_eq!(result.unmapped_pivots.record_ids, unmapped, "{at}");
            assert_eq!(result.unmapped_pivots.count, unmapped.len() as u64, "{at}");
            assert_eq!(
                result.stats.pivot_candidates,
                1 + unmapped.len() as u64,
                "{at}"
            );
            assert_eq!(result.stats.pivots_mapped, 1, "{at}");
            // The attached rows carry no deadline, so the evidence is the same at every clock.
            assert_eq!(evidence_ids(&result), [("half", 0), ("full", 0)], "{at}");
        }
    }
}

/// Gives the effective row of `id` the deadline `deadline` through the state that every writer ends in, as a write that
/// the public API refuses for an attached record would have left it.
fn inject_deadline(db: &mut Database, collection: &str, id: u64, deadline: u64) {
    let collection_id = db.collection(collection).unwrap().id();
    let row = db
        .collection(collection)
        .unwrap()
        .get_at(id, 0)
        .unwrap()
        .unwrap()
        .clone();
    let revision = db
        .collection(collection)
        .unwrap()
        .revision(id)
        .unwrap()
        .unwrap();
    db.state_mut(collection_id)
        .apply_committed(id, Some(row), revision, 0, Some(deadline))
        .unwrap();
}

#[test]
fn compact_graph_evidence_is_read_at_the_retrieval_clock_even_if_an_attached_row_had_a_deadline() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    // No public writer gives an attached record a deadline (`an_attached_record_never_carries_a_deadline...`), so the
    // evidence phase never sees one and a wrong clock would go unnoticed. The contract still says what an attached row
    // with a deadline means: it is hidden at `T >= deadline`, in the evidence phase as in the pivot phase, and the
    // retrieval clock decides, never the wall clock or a fixed one.
    for db in [&mut candidate, &mut oracle] {
        inject_deadline(db, "half", 0, 2_000);
        inject_deadline(db, "full", 0, 3_000);
    }
    assert_eq!(
        candidate.collection("half").unwrap().expires_at(0).unwrap(),
        Some(2_000)
    );
    let request = request("full", [0.1, -0.0], &["half", "full"]);
    let both: &[(&str, u64)] = &[("half", 0), ("full", 0)];
    let table: [ClockRow<'_>; 6] = [
        (0, &[0], both, &[3, 10, u64::MAX, 20]),
        (1_999, &[0], both, &[3, 10, u64::MAX, 20]),
        (2_000, &[0], &[("full", 0)], &[3, 10, u64::MAX, 20]),
        (2_999, &[0], &[("full", 0)], &[3, 10, u64::MAX, 20]),
        (3_000, &[], &[], &[3, 10, u64::MAX, 20]),
        (u64::MAX, &[], &[], &[10, u64::MAX, 20]),
    ];
    for (now_ms, pivots, evidence, unmapped) in table {
        let result = retrieve_both(&candidate, &oracle, now_ms, &request);
        assert_eq!(pivot_ids(&result), pivots, "pivots at {now_ms}");
        assert_eq!(evidence_ids(&result), evidence, "evidence at {now_ms}");
        assert_eq!(
            result.unmapped_pivots.record_ids, unmapped,
            "unmapped at {now_ms}"
        );
        // No pivot means no node, so no evidence either.
        assert_eq!(result.nodes.len(), pivots.len(), "nodes at {now_ms}");
        assert!(result.complete);
    }
}

#[test]
fn compact_graph_retrieve_propagates_read_errors_of_the_pivot_and_evidence_collections() {
    let _gates = gates();
    let with_half = request("full", [0.1, -0.0], &["half"]);
    let without = request("full", [0.1, -0.0], &[]);
    let with_full = request("full", [0.1, -0.0], &["full"]);

    // A poisoned record cell of a pivot hit. The exact scan reads through the workspace and never takes a cell lock, so
    // it is the re-validation of the hit that meets the lock; the call fails instead of rejecting the pivot.
    {
        let (candidate, oracle, _) = compact_dispatch_fixture(false);
        assert!(resident_retrieve(&oracle, NOW, &with_half).is_ok());
        poison_cell(&candidate, "full", 10);
        for request in [&without, &with_half, &with_full] {
            let result = compact_retrieve(&candidate, NOW, request);
            assert!(is_poisoned_cell(&result), "{result:?}");
        }
    }

    // A poisoned record cell of the attached evidence row. The pivot phase never reads `half`, so only a request for
    // evidence from `half` fails; one that asks for none, or for `full`, answers in full.
    {
        let (candidate, oracle, _) = compact_dispatch_fixture(false);
        poison_cell(&candidate, "half", 0);
        let result = compact_retrieve(&candidate, NOW, &with_half);
        assert!(is_poisoned_cell(&result), "{result:?}");
        let ok = compact_retrieve(&candidate, NOW, &without).unwrap();
        assert_eq!(ok, resident_retrieve(&oracle, NOW, &without).unwrap());
        assert_eq!(pivot_ids(&ok), [0]);
        let ok = compact_retrieve(&candidate, NOW, &with_full).unwrap();
        assert_eq!(ok, resident_retrieve(&oracle, NOW, &with_full).unwrap());
        assert_eq!(evidence_ids(&ok), [("full", 0)]);
    }

    // A damaged block: plain (the frame changed) and sealed (the authentication fails), in the pivot collection and in
    // the evidence collection.
    for (how, encrypted, why) in [
        (Tamper::Checksum, false, "compact source frame changed"),
        (Tamper::Ciphertext, true, "frame authentication failed"),
    ] {
        let (mut candidate, oracle, _) = compact_dispatch_fixture(encrypted);
        tamper_block(&mut candidate, "full", &how);
        for request in [&without, &with_half, &with_full] {
            let result = compact_retrieve(&candidate, NOW, request);
            assert!(is_corrupt(&result, why), "pivot: {result:?}");
        }
        assert!(resident_retrieve(&oracle, NOW, &with_half).is_ok());

        let (mut candidate, oracle, _) = compact_dispatch_fixture(encrypted);
        tamper_block(&mut candidate, "half", &how);
        let result = compact_retrieve(&candidate, NOW, &with_half);
        assert!(is_corrupt(&result, why), "evidence: {result:?}");
        let ok = compact_retrieve(&candidate, NOW, &without).unwrap();
        assert_eq!(ok, resident_retrieve(&oracle, NOW, &without).unwrap());
        let ok = compact_retrieve(&candidate, NOW, &with_full).unwrap();
        assert_eq!(ok, resident_retrieve(&oracle, NOW, &with_full).unwrap());
    }
}

#[test]
fn compact_graph_lists_at_most_sixty_four_unmapped_pivots_and_counts_all_of_them() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    // Seventy overlay-only rows at the pivot vector: 0 (attached), 3, 10, 100..170, u64::MAX tie at distance 0, row 20
    // is farther. That makes 75 rows and 75 pivot hits, one mapped and 74 unmapped.
    let batch: Vec<Mutation> = (100u64..170)
        .map(|id| Mutation::Put {
            id,
            vector: vec![0.1, -0.0],
            payload: id.to_le_bytes().to_vec(),
        })
        .collect();
    for db in [&mut candidate, &mut oracle] {
        db.collection_mut("full")
            .unwrap()
            .write_batch(&batch)
            .unwrap();
    }
    let mut wide = request("full", [0.1, -0.0], &["half"]);
    wide.pivot.k = 100;
    let result = retrieve_both(&candidate, &oracle, NOW, &wide);
    assert_eq!(result.stats.pivot_candidates, 75);
    assert_eq!(pivot_ids(&result), [0]);
    assert_eq!(result.unmapped_pivots.count, 74);
    let mut listed = vec![3, 10];
    listed.extend(100..162);
    assert_eq!(listed.len(), 64);
    assert_eq!(result.unmapped_pivots.record_ids, listed);

    // The exact leg runs on a collection of at most `max_exact_scan_records` rows: 75 rows, so 75 passes and 74 does not.
    for (limit, passes) in [(75, true), (74, false)] {
        let mut guarded = wide.clone();
        guarded.pivot.max_exact_scan_records = limit;
        let actual = compact_retrieve(&candidate, NOW, &guarded);
        let expected = resident_retrieve(&oracle, NOW, &guarded);
        for result in [&actual, &expected] {
            assert_eq!(result.is_ok(), passes, "limit {limit}: {result:?}");
            if !passes {
                assert!(matches!(result, Err(Error::GraphPivotTooExpensive)));
            }
        }
        if passes {
            assert_eq!(actual.unwrap(), expected.unwrap());
        }
    }
}

#[test]
fn compact_graph_pivot_filter_sees_the_effective_metadata() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    // The overlay replaces the metadata of the attached row: it had id == "0" (and more) in the image, and has
    // marker == "overlay" now.
    for db in [&mut candidate, &mut oracle] {
        for name in COLLECTIONS {
            db.collection_mut(name)
                .unwrap()
                .update_metadata(0, &marker("overlay"))
                .unwrap();
        }
    }
    let eq = |field: &str, value: &str| Condition::Eq {
        field: field.to_owned(),
        value: MetadataValue::String(value.to_owned()),
    };
    for pivot_collection in COLLECTIONS {
        let with_filter = |condition: Condition| {
            let mut filtered = request(pivot_collection, [0.1, -0.0], &["half", "full"]);
            filtered.pivot.filter = vec![condition];
            retrieve_both(&candidate, &oracle, NOW, &filtered)
        };

        // The base row matched, the effective row does not: no pivot, no node, no evidence.
        let result = with_filter(eq("id", "0"));
        assert_eq!(result.stats.pivot_candidates, 0, "{pivot_collection}");
        assert!(result.pivots.is_empty() && result.unmapped_pivots.count == 0);
        assert!(result.nodes.is_empty() && result.evidence.is_empty());
        assert_eq!(result.stats.pivots_rejected, 0);

        // The effective row matches: it is the only hit and the evidence is projected from the overlay rows.
        let result = with_filter(eq("marker", "overlay"));
        assert_eq!(pivot_ids(&result), [0], "{pivot_collection}");
        assert_eq!(result.stats.pivot_candidates, 1);
        assert_eq!(result.unmapped_pivots.count, 0);
        assert_eq!(evidence_ids(&result), [("half", 0), ("full", 0)]);
        assert_eq!(result.evidence[0].metadata, marker("overlay"));
        assert_eq!(result.evidence[1].metadata, marker("overlay"));

        // A base row that was not touched still matches by its own metadata, unmapped.
        let result = with_filter(eq("id", "3"));
        assert!(result.pivots.is_empty());
        assert_eq!(result.unmapped_pivots.record_ids, [3], "{pivot_collection}");
        assert_eq!(result.stats.pivot_candidates, 1);
    }
}

#[test]
fn an_attached_record_never_carries_a_deadline_on_compact_or_resident_records() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);

    // (a) The only API that sets a record's deadline, `put_with_expiry`, refuses every deadline for an attached
    // record, and writes nothing. Records 0 of `half` and `full` are attached to node `n`.
    for db in [&mut candidate, &mut oracle] {
        for name in COLLECTIONS {
            let sequence = db.sequence;
            let bytes = db.to_bytes();
            for deadline in [0, 1, 2, FAR, u64::MAX] {
                let refused = db.collection_mut(name).unwrap().put_with_expiry(
                    0,
                    &[0.5, 0.5],
                    b"deadline",
                    &Metadata::new(),
                    Some(deadline),
                );
                assert!(
                    matches!(refused, Err(Error::GraphAttachedRecordExpiry)),
                    "{name} {deadline}: {refused:?}"
                );
            }
            assert_eq!(db.sequence, sequence, "{name}");
            assert_eq!(db.to_bytes(), bytes, "{name}");
            assert_eq!(db.collection(name).unwrap().expires_at(0).unwrap(), None);
        }
    }

    // (b) Every other write path leaves the attached record without a deadline: a plain put, a batch (the vocabulary of
    // a batch, an import chunk and a graph vector operation is put, put with metadata and delete: no form carries a
    // deadline), a conditional put, a metadata update and `put_with_expiry` with no deadline at all.
    let batch = [
        Mutation::Put {
            id: 0,
            vector: vec![0.5, 0.5],
            payload: b"batch".to_vec(),
        },
        Mutation::PutWithMetadata {
            id: 0,
            vector: vec![0.5, 0.25],
            payload: b"batch with metadata".to_vec(),
            metadata: phase("batch"),
        },
    ];
    // Exhaustive on purpose: a new `Mutation` form has to be classified here, and one that can carry a deadline must be
    // refused for attached records like `put_with_expiry` is.
    fn form(mutation: &Mutation) -> &'static str {
        match mutation {
            Mutation::PutWithMetadata { .. } => "put with metadata",
            Mutation::Put { .. } => "put",
            Mutation::Delete { .. } => "delete",
        }
    }
    assert_eq!(
        batch.iter().map(form).collect::<Vec<_>>(),
        ["put", "put with metadata"]
    );
    for db in [&mut candidate, &mut oracle] {
        for name in COLLECTIONS {
            let mut collection = db.collection_mut(name).unwrap();
            collection.put(0, &[0.5, 0.5], b"plain").unwrap();
            collection.write_batch(&batch).unwrap();
            let revision = collection.view().revision(0).unwrap().unwrap();
            collection
                .put_with_metadata_if(
                    0,
                    &[0.25, 0.5],
                    b"conditional",
                    &phase("conditional"),
                    Expected::Revision(revision),
                )
                .unwrap();
            collection.update_metadata(0, &phase("metadata")).unwrap();
            collection
                .put_with_expiry(0, &[0.5, 0.25], b"none", &phase("none"), None)
                .unwrap();
            assert_eq!(collection.view().expires_at(0).unwrap(), None, "{name}");
        }
    }
    point_parity(&candidate, &oracle);

    // The evidence of the attached rows is the same at every clock, also at the extremes: a record cannot expire.
    let request = request("full", [0.1, -0.0], &["half", "full"]);
    let first = retrieve_both(&candidate, &oracle, 0, &request);
    assert_eq!(evidence_ids(&first), [("half", 0), ("full", 0)]);
    assert_eq!(first.evidence[0].payload_excerpt, b"none");
    assert_eq!(first.evidence[0].metadata, phase("none"));
    for now_ms in [NOW, 1_999, FAR - 1, FAR, u64::MAX] {
        let later = retrieve_both(&candidate, &oracle, now_ms, &request);
        assert_eq!(later.evidence, first.evidence, "evidence at {now_ms}");
        assert_eq!(pivot_ids(&later), [0], "pivot 0 at {now_ms}");
    }

    // (c) Attaching a record that has a deadline is refused: only live records without an expiry can be attached.
    // Graph writes are refused on compact records as a whole, so no attachment can be made there at all.
    unsupported(candidate.graph_mut("g"), "graph_mut");
    {
        let mut graph = oracle.graph_mut("g").unwrap();
        graph.create_node("m", &[], &Metadata::new()).unwrap();
        // Row 3 of `full` has the deadline FAR; row 12345 does not exist.
        assert!(matches!(
            graph.attach("m", "full", 3),
            Err(Error::GraphRecordNotAttachable)
        ));
        assert!(matches!(
            graph.attach("m", "full", 12_345),
            Err(Error::GraphRecordNotAttachable)
        ));
        // Row 10 has none, so it attaches, and from then on it cannot be given one.
        graph.attach("m", "full", 10).unwrap();
    }
    assert!(matches!(
        oracle.collection_mut("full").unwrap().put_with_expiry(
            10,
            &[0.1, -0.0],
            b"deadline",
            &Metadata::new(),
            Some(5)
        ),
        Err(Error::GraphAttachedRecordExpiry)
    ));
    assert_eq!(
        oracle.collection("full").unwrap().expires_at(10).unwrap(),
        None
    );
    assert_eq!(
        oracle.collection("full").unwrap().expires_at(3).unwrap(),
        Some(FAR)
    );
}

#[test]
fn compact_graph_access_and_retrieval_answer_poisoned_on_an_unhealthy_handle() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    candidate.poisoned = true;
    oracle.poisoned = true;
    // The health check is the first one, before the graph and the collection are looked up.
    assert!(matches!(
        candidate.compact_graph_for_test("g", "full"),
        Err(Error::Poisoned)
    ));
    assert!(matches!(
        candidate.compact_graph_for_test("missing", "missing"),
        Err(Error::Poisoned)
    ));
    assert!(matches!(oracle.graph("g"), Err(Error::Poisoned)));
    assert!(matches!(oracle.graph("missing"), Err(Error::Poisoned)));

    // A handle that turns unhealthy after the graph handle was taken (a failed periodic sync) answers Poisoned from
    // `retrieve` itself, never a result that was read from a handle that is no longer trusted.
    let (mut candidate, _, _) = compact_dispatch_fixture(false);
    candidate.flusher = Some(
        crate::durability::Flusher::start(
            tempfile::tempfile().unwrap(),
            std::time::Duration::from_secs(3_600),
        )
        .unwrap(),
    );
    let graph = candidate
        .compact_graph_for_test("g", "full")
        .unwrap()
        .with_clock(NOW);
    let request = request("full", [0.1, -0.0], &["half"]);
    assert!(graph.retrieve(&request, &options()).is_ok());
    let flusher = candidate.flusher.as_ref().unwrap();
    flusher.mark_written();
    flusher.inject_sync_failure();
    assert!(flusher.sync_now().is_err());
    assert!(flusher.failed());
    let result = graph.retrieve(&request, &options());
    assert!(matches!(result, Err(Error::Poisoned)), "{result:?}");
}

/// Gives `db` the flusher whose failed sync turns a handle unhealthy.
fn with_flusher(mut db: Database) -> Database {
    db.flusher = Some(
        crate::durability::Flusher::start(
            tempfile::tempfile().unwrap(),
            std::time::Duration::from_secs(3_600),
        )
        .unwrap(),
    );
    db
}

fn fail_the_periodic_sync(db: &Database) {
    let flusher = db.flusher.as_ref().unwrap();
    flusher.mark_written();
    flusher.inject_sync_failure();
    assert!(flusher.sync_now().is_err());
    assert!(flusher.failed());
}

/// `retrieve` validates the bounds of the request before it looks anything up, and the first lookup is the first
/// health check, so a handle that turned unhealthy after `graph()` answers a request over a ceiling with its
/// `GraphLimitExceedsCeiling` and every other request with `Poisoned`. The freeze note (ERR-8) puts the health check
/// first; this is the order the resident code has, the compact arm shares it, and the test pins it as it is (a known
/// order after acquisition, to be settled in 0.7.2) on both arms.
#[test]
fn graph_retrieve_validates_the_request_bounds_before_the_health_of_a_handle_that_turned_unhealthy()
{
    let _gates = gates();
    let (candidate, oracle, _) = compact_dispatch_fixture(false);
    let (candidate, oracle) = (with_flusher(candidate), with_flusher(oracle));
    let answer = |graph: &GraphRef<'_>, request: &Retrieve, options: &ReadOptions<'_>| {
        format!("{:?}", graph.retrieve(request, options).map(|_| ()))
    };
    let valid = || request("full", [0.1, -0.0], &["half"]);
    let over_pivot_k = {
        let mut request = valid();
        request.pivot.k = Pivot::MAX_K + 1;
        request
    };
    let over_evidence_items = {
        let mut request = valid();
        request.evidence.max_items = EvidenceSpec::MAX_ITEMS + 1;
        request
    };
    let over_read_limit = {
        let mut options = options();
        options.limits.max_hops = GraphLimits::CEILING.max_hops + 1;
        options
    };
    let over_a_ceiling = [
        (
            "pivot k",
            over_pivot_k,
            options(),
            r#"Err(GraphLimitExceedsCeiling { limit: "pivot_k", requested: 101, ceiling: 100 })"#,
        ),
        (
            "evidence items",
            over_evidence_items,
            options(),
            r#"Err(GraphLimitExceedsCeiling { limit: "max_evidence_items", requested: 1001, ceiling: 1000 })"#,
        ),
        (
            "read limit",
            valid(),
            over_read_limit,
            r#"Err(GraphLimitExceedsCeiling { limit: "max_hops", requested: 5, ceiling: 4 })"#,
        ),
    ];
    let arms = [
        (
            "compact",
            &candidate,
            candidate.compact_graph_for_test("g", "full").unwrap(),
        ),
        ("resident", &oracle, oracle.graph("g").unwrap()),
    ];
    for (arm, db, graph) in arms {
        let graph = graph.with_clock(NOW);
        assert_eq!(answer(&graph, &valid(), &options()), "Ok(())", "{arm}");
        for (what, request, options, error) in &over_a_ceiling {
            assert_eq!(answer(&graph, request, options), *error, "{arm}: {what}");
        }
        fail_the_periodic_sync(db);
        assert_eq!(
            answer(&graph, &valid(), &options()),
            "Err(Poisoned)",
            "{arm}"
        );
        for (what, request, options, error) in &over_a_ceiling {
            assert_eq!(
                answer(&graph, request, options),
                *error,
                "{arm}: {what} after the failed sync"
            );
        }
    }
}
