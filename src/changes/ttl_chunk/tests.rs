//! `ttl_chunk_boundaries`: chunk sizes against an independent JSON builder.
//!
//! The builder below writes the canonical text itself (`format!` and a
//! per-character escaper); the counter under test never builds text. Short
//! cases are also hand-written literals.

use super::*;
use crate::property_graph::GraphRemovalReason::{Expired, NodeExpired};

/// Independent escaper: per character, never per byte.
fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn node_json(id: u64, key: &str, deadline: u64, at: u64) -> String {
    format!(
        r#"{{"expires_at_ms":"{deadline}","id":"{id}","key":{},"op":"node_deleted","removal_reason":"expired","removed_at_ms":"{at}"}}"#,
        quote(key)
    )
}

fn event_json(
    sequence: u64,
    committed: Option<u64>,
    ordinal: usize,
    graph: &str,
    operations: &[String],
) -> String {
    let committed = committed.map_or("null".to_owned(), |time| time.to_string());
    format!(
        r#"{{"change":{{"graph":{},"operations":[{}],"revision":"{sequence}","type":"graph_batch"}},"collection":"","committed_at_ms":{committed},"ordinal":{ordinal},"sequence":"{sequence}"}}"#,
        quote(graph),
        operations.join(",")
    )
}

fn node_delete(id: u64, key: &str, deadline: u64, at: u64) -> (GraphChange, Option<GraphTtl>) {
    (
        GraphChange::NodeDeleted {
            id,
            key: key.into(),
        },
        Some(GraphTtl::ExpiryRemoval {
            removal_reason: Expired,
            expires_at_ms: deadline,
            removed_at_ms: at,
        }),
    )
}

fn reclaim(operations: Vec<(GraphChange, Option<GraphTtl>)>) -> TtlEvent {
    let (operations, ttl) = operations.into_iter().unzip();
    TtlEvent {
        collection: String::new(),
        kind: ChangeKind::GraphBatch {
            graph: "g".into(),
            revision: 7,
            operations,
        },
        ttl,
        reclaim: true,
    }
}

fn ordinary(kind: ChangeKind) -> TtlEvent {
    TtlEvent {
        collection: String::new(),
        kind,
        ttl: Vec::new(),
        reclaim: false,
    }
}

fn dropped() -> TtlEvent {
    ordinary(ChangeKind::GraphDropped { graph: "g".into() })
}

/// `(ordinal, operation count)` of each event and the unit ends.
fn shape(expanded: &Expanded) -> (Vec<(u32, usize)>, Vec<usize>) {
    let events = expanded
        .events
        .iter()
        .map(|event| {
            let count = match &event.change.kind {
                ChangeKind::GraphBatch { operations, .. } => operations.len(),
                _ => 0,
            };
            (event.change.ordinal, count)
        })
        .collect();
    (events, expanded.ends.clone())
}

fn expanded(events: Vec<TtlEvent>, committed: Option<u64>) -> Expanded {
    match expand(7, committed, events, false, 0) {
        Ok(expanded) => expanded,
        Err(error) => panic!("expand failed: {error}"),
    }
}

/// A node key of exactly `bytes` canonical bytes inside its quotes, from
/// a fixed mix of multi-byte, escaped and plain characters.
fn padded_key(bytes: usize) -> String {
    // 'é' 2 bytes, '😀' 4, '"' escaped 2, U+0001 escaped 6: 14 in total.
    let mut key = String::from("é😀\"\u{1}");
    key.push_str(&"x".repeat(bytes - 14));
    assert_eq!(quote(&key).len(), bytes + 2);
    key
}

#[test]
fn ttl_chunk_boundaries() {
    // Hand-written literals: each operation shape, both removal reasons,
    // a null edge key, a commit time and a two-digit ordinal.
    let edge = r#"{"expires_at_ms":"100","id":"2","key":null,"op":"edge_deleted","removal_reason":"node_expired","removed_at_ms":"150"}"#;
    let keyed = r#"{"expires_at_ms":"18446744073709551615","id":"18446744073709551615","key":"ab","op":"edge_deleted","removal_reason":"expired","removed_at_ms":"0"}"#;
    let detach =
        r#"{"collection":"default","node":"0","node_key":"a","op":"detached","record":"1"}"#;
    let node = r#"{"expires_at_ms":"100","id":"0","key":"a","op":"node_deleted","removal_reason":"expired","removed_at_ms":"150"}"#;
    let explicit_node = r#"{"id":"5","key":"k","op":"node_deleted","removal_reason":"explicit"}"#;
    let explicit_edge = r#"{"id":"6","key":null,"op":"edge_deleted","removal_reason":"explicit"}"#;
    let removal = |reason, deadline, at| {
        Some(GraphTtl::ExpiryRemoval {
            removal_reason: reason,
            expires_at_ms: deadline,
            removed_at_ms: at,
        })
    };
    let cases = [
        (
            GraphChange::EdgeDeleted { id: 2, key: None },
            removal(NodeExpired, 100, 150),
            edge,
        ),
        (
            GraphChange::EdgeDeleted {
                id: u64::MAX,
                key: Some("ab".into()),
            },
            removal(Expired, u64::MAX, 0),
            keyed,
        ),
        (
            GraphChange::Detached {
                node: 0,
                node_key: "a".into(),
                collection: "default".into(),
                record: 1,
            },
            None,
            detach,
        ),
        (
            GraphChange::NodeDeleted {
                id: 0,
                key: "a".into(),
            },
            removal(Expired, 100, 150),
            node,
        ),
        (
            GraphChange::NodeDeleted {
                id: 5,
                key: "k".into(),
            },
            Some(GraphTtl::ExplicitRemoval),
            explicit_node,
        ),
        (
            GraphChange::EdgeDeleted { id: 6, key: None },
            Some(GraphTtl::ExplicitRemoval),
            explicit_edge,
        ),
    ];
    for (operation, ttl, literal) in &cases {
        assert_eq!(
            operation_bytes(operation, ttl.as_ref()),
            Some(literal.len()),
            "{literal}"
        );
    }
    let whole = concat!(
        r#"{"change":{"graph":"g","operations":["#,
        r#"{"expires_at_ms":"100","id":"2","key":null,"op":"edge_deleted","removal_reason":"node_expired","removed_at_ms":"150"},"#,
        r#"{"collection":"default","node":"0","node_key":"a","op":"detached","record":"1"},"#,
        r#"{"expires_at_ms":"100","id":"0","key":"a","op":"node_deleted","removal_reason":"expired","removed_at_ms":"150"}"#,
        r#"],"revision":"7","type":"graph_batch"},"collection":"","committed_at_ms":1700000000000,"ordinal":12,"sequence":"7"}"#
    );
    assert_eq!(
        envelope_bytes(7, Some(1_700_000_000_000), 12, "g")
            + edge.len()
            + detach.len()
            + node.len()
            + 2,
        whole.len()
    );
    assert_eq!(
        envelope_bytes(7, None, 0, "g"),
        r#"{"change":{"graph":"g","operations":[],"revision":"7","type":"graph_batch"},"collection":"","committed_at_ms":null,"ordinal":0,"sequence":"7"}"#.len()
    );
    // Escapes and raw UTF-8: slash and non-ASCII stay raw.
    let text = "q\"b\\s/\u{0}\u{1f}\u{8}\t\n\u{c}\ré😀";
    let literal = r#""q\"b\\s/\u0000\u001f\b\t\n\f\ré😀""#;
    assert_eq!(quote(text), literal);
    let mut count = Count::new();
    count.string(text);
    assert_eq!(count.total(), literal.len());
    assert_eq!(literal.len(), 38);
    // Shapes no reclaim produces are refused, not counted.
    let updated = GraphTtl::Updated {
        expires_at_ms: None,
        expiry_changed: false,
    };
    assert_eq!(operation_bytes(&cases[3].0, Some(&updated)), None);
    assert_eq!(operation_bytes(&cases[3].0, None), None);
    assert_eq!(operation_bytes(&cases[2].0, Some(&updated)), None);

    // 255, 256 and 257 small operations: the operation limit.
    let small = |count: u64| -> Vec<_> {
        (0..count)
            .map(|id| node_delete(id, "k", 100, 150))
            .collect()
    };
    for (count, events, ends) in [
        (255, vec![(0, 255)], vec![1]),
        (256, vec![(0, 256)], vec![1]),
        (257, vec![(0, 256), (1, 1)], vec![1, 2]),
        (513, vec![(0, 256), (1, 256), (2, 1)], vec![1, 2, 3]),
    ] {
        let got = expanded(vec![reclaim(small(count))], None);
        assert_eq!(shape(&got), (events, ends), "{count} operations");
        let ids: Vec<u64> = got
            .events
            .iter()
            .flat_map(|event| match &event.change.kind {
                ChangeKind::GraphBatch { operations, .. } => operations.clone(),
                _ => Vec::new(),
            })
            .map(|operation| match operation {
                GraphChange::NodeDeleted { id, .. } => id,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, (0..count).collect::<Vec<_>>(), "order kept");
        for event in &got.events {
            let ChangeKind::GraphBatch { operations, .. } = &event.change.kind else {
                panic!("not a batch");
            };
            assert_eq!(event.ttl.len(), operations.len());
            assert!(event.change.collection.is_empty());
        }
    }

    // 1 MiB minus one, exact and plus one: two operations, the second
    // padded with multi-byte and escaped characters.
    let first = node_json(0, "a", 100, 150);
    let base = event_json(
        7,
        None,
        0,
        "g",
        &[first.clone(), node_json(1, "", 100, 150)],
    )
    .len();
    for (target, events) in [
        (CHUNK_BYTES - 1, vec![(0, 2)]),
        (CHUNK_BYTES, vec![(0, 2)]),
        (CHUNK_BYTES + 1, vec![(0, 1), (1, 1)]),
    ] {
        let key = padded_key(target - base);
        let text = event_json(
            7,
            None,
            0,
            "g",
            &[first.clone(), node_json(1, &key, 100, 150)],
        );
        assert_eq!(text.len(), target);
        let got = expanded(
            vec![reclaim(vec![
                node_delete(0, "a", 100, 150),
                node_delete(1, &key, 100, 150),
            ])],
            None,
        );
        let ends = (1..=events.len()).collect();
        assert_eq!(shape(&got), (events, ends), "event of {target} bytes");
    }

    // Ordinal digits: nine events before, so the first reclaim is ordinal
    // 9 and the second 10. The same two operations fit at 9 (exactly 1 MiB)
    // and do not at 10 (one byte more).
    let base = event_json(
        7,
        Some(5),
        9,
        "g",
        &[first.clone(), node_json(1, "", 100, 150)],
    )
    .len();
    let key = padded_key(CHUNK_BYTES - base);
    let pair = || {
        reclaim(vec![
            node_delete(0, "a", 100, 150),
            node_delete(1, &key, 100, 150),
        ])
    };
    let at_nine = event_json(
        7,
        Some(5),
        9,
        "g",
        &[first.clone(), node_json(1, &key, 100, 150)],
    );
    assert_eq!(at_nine.len(), CHUNK_BYTES);
    let at_ten = event_json(
        7,
        Some(5),
        10,
        "g",
        &[first.clone(), node_json(1, &key, 100, 150)],
    );
    assert_eq!(at_ten.len(), CHUNK_BYTES + 1);
    let mut events: Vec<TtlEvent> = (0..9).map(|_| dropped()).collect();
    events.push(pair());
    events.push(pair());
    let got = expanded(events, Some(5));
    let mut expected: Vec<(u32, usize)> = (0..9).map(|ordinal| (ordinal, 0)).collect();
    expected.extend([(9, 2), (10, 1), (11, 1)]);
    assert_eq!(shape(&got), (expected, vec![9, 10, 11, 12]));
    for (index, event) in got.events.iter().enumerate() {
        assert_eq!(event.change.ordinal as usize, index);
        assert_eq!(
            (event.change.sequence, event.change.committed_at_ms),
            (7, Some(5))
        );
    }

    // A single operation may exceed 1 MiB; its neighbours get own chunks.
    let big = padded_key(CHUNK_BYTES + 1000);
    let got = expanded(
        vec![reclaim(vec![
            node_delete(0, "a", 100, 150),
            node_delete(1, &big, 100, 150),
            node_delete(2, "c", 100, 150),
        ])],
        None,
    );
    assert_eq!(shape(&got), (vec![(0, 1), (1, 1), (2, 1)], vec![1, 2, 3]));

    // Exactly 16 MiB is delivered; one byte more is refused whole.
    let base = event_json(7, None, 1, "g", &[node_json(1, "", 100, 150)]).len();
    let key = padded_key(EVENT_LIMIT - base);
    let limit = event_json(7, None, 1, "g", &[node_json(1, &key, 100, 150)]);
    assert_eq!(limit.len(), EVENT_LIMIT);
    let got = expanded(
        vec![dropped(), reclaim(vec![node_delete(1, &key, 100, 150)])],
        None,
    );
    assert_eq!(shape(&got), (vec![(0, 0), (1, 1)], vec![1, 2]));
    let key = padded_key(EVENT_LIMIT - base + 1);
    match expand(
        7,
        None,
        vec![dropped(), reclaim(vec![node_delete(1, &key, 100, 150)])],
        false,
        0,
    ) {
        Err(GraphFeedError::ResponseTooLarge { bytes, limit }) => {
            assert_eq!((bytes, limit), (EVENT_LIMIT + 1, 16_777_216));
        }
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("an event above the limit was delivered"),
    }

    // Ordinals count every event of the commit; ordinary runs between
    // reclaims are single units and never merge into a chunk.
    let put = ordinary(ChangeKind::Delete { id: 3 });
    let batch = ordinary(ChangeKind::GraphBatch {
        graph: "g".into(),
        revision: 7,
        operations: vec![GraphChange::SchemaUpdated {
            schema: Default::default(),
        }],
    });
    let got = expanded(
        vec![put, dropped(), reclaim(small(300)), batch, dropped()],
        None,
    );
    assert_eq!(
        shape(&got),
        (
            vec![(0, 0), (1, 0), (2, 256), (3, 44), (4, 1), (5, 0)],
            vec![2, 3, 4, 6]
        )
    );
    // Adjacent reclaims stay distinct runs.
    let got = expanded(vec![reclaim(small(2)), reclaim(small(1))], None);
    assert_eq!(shape(&got), (vec![(0, 2), (1, 1)], vec![1, 2]));

    // A whole commit (temporal) is one unit and its events are never split.
    let Ok(got) = expand(7, None, vec![dropped(), reclaim(small(300))], true, 0) else {
        panic!("whole commit refused");
    };
    assert_eq!(shape(&got), (vec![(0, 0), (1, 300)], vec![2]));
}
