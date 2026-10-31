//! Replay's frame limits and snapshot rule for every reader (the P3a
//! review of 37eecbda). Full replay, the change feed and log export of a
//! complete precision log, plaintext or authenticated, refuse a frame
//! whose count, reserved ID, length or target collection replay refuses,
//! with replay's reason and before any of its body is decoded, so no
//! decoder preallocates for a count its body cannot hold; and each admits
//! in a graph snapshot only the entries a checkpoint writes.

use super::*;
use crate::property_graph::EdgeRef;
use crate::property_graph::engine::{self, GraphEntry};
use crate::{MAX_BATCH_OPERATIONS, MetadataValue};

/// Records one record block holds at most (`format::MAX_BLOCK_RECORDS`).
const BLOCK_RECORDS: usize = 4096;

/// An edit of the frames of an image under test.
type ImageEdit<'a> = dyn Fn(&mut Vec<Vec<u8>>) + 'a;

/// `result` as "ok", its corruption reason, or its `outcome`.
fn reason<T>(result: Result<T>) -> String {
    match result {
        Err(Error::Corrupt { reason, .. }) => reason.into(),
        other => outcome(other),
    }
}

/// Full replay, the change feed and log export of `log` after `floor`.
fn readers(log: &[u8], key: Option<&EncryptionKey>, floor: u64) -> [String; 3] {
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: floor,
    };
    let feed = crate::changes::read_changes(&mut Cursor::new(log), key, None, &cursor, 16);
    let export = logops::read_segment(&mut Cursor::new(log), key, None, floor, 1 << 20);
    [reason(load(log, key)), reason(feed), reason(export)]
}

/// `readers` of the plaintext `log` and of it encrypted.
fn both(log: &[u8], floor: u64) -> [[String; 3]; 2] {
    [
        readers(log, None, floor),
        readers(&encrypted(log), Some(&key()), floor),
    ]
}

fn alike(reason: &str) -> [[String; 3]; 2] {
    std::array::from_fn(|_| std::array::from_fn(|_| reason.to_owned()))
}

/// Each named case whose readers' outcomes are not all its expected
/// reason, with those outcomes.
fn mismatches<'a>(
    cases: impl IntoIterator<Item = (&'a str, [[String; 3]; 2], &'a str)>,
) -> Vec<String> {
    cases
        .into_iter()
        .filter(|(_, outcomes, expected)| *outcomes != alike(expected))
        .map(|(name, outcomes, _)| format!("{name}: {outcomes:?}"))
        .collect()
}

fn record(id: u64) -> Mutation {
    Mutation::Put {
        id,
        vector: vec![0.5, 0.25],
        payload: b"p".to_vec(),
    }
}

/// A precision store whose float32 `c` (ID 1, two dimensions) holds
/// records `1..=records`.
fn store(records: u64) -> Database {
    let mut db = Database::open_from_bytes(image_of(&initial(None)), None).unwrap();
    db.create_collection_as("c", spec(2), F32).unwrap();
    let ids: Vec<u64> = (1..=records).collect();
    for chunk in ids.chunks(MAX_BATCH_OPERATIONS) {
        let batch: Vec<_> = chunk.iter().map(|&id| record(id)).collect();
        db.collection_mut("c").unwrap().write_batch(&batch).unwrap();
    }
    db
}

/// The complete plaintext image of `db` at its head, which is its floor.
fn imaged(db: &Database) -> (Vec<u8>, u64) {
    let loaded = load(&db.to_bytes().unwrap(), None).unwrap();
    (image_of(&loaded), loaded.sequence)
}

/// The frame that `write` commits after `log`.
fn committed(log: &[u8], write: impl FnOnce(&mut Database) -> Result<()>) -> Vec<u8> {
    let mut db = Database::open_from_bytes(log.to_vec(), None).unwrap();
    write(&mut db).unwrap();
    db.to_bytes().unwrap()[log.len()..].to_vec()
}

fn set_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn set_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// `base` followed by `frames`.
fn with(base: &[u8], frames: &[&[u8]]) -> Vec<u8> {
    let mut log = base.to_vec();
    for frame in frames {
        log.extend_from_slice(frame);
    }
    log
}

/// `frame` with its body replaced by `body` holding `count` entries.
fn rebodied(frame: &[u8], count: u32, body: &[u8]) -> Vec<u8> {
    refit(frame, |bytes| {
        bytes.truncate(PREFIX_SIZE);
        bytes.extend_from_slice(body);
        set_u32(bytes, 24, count);
    })
}

/// The body of the plaintext `frame`.
fn body(frame: &[u8]) -> &[u8] {
    &frame[PREFIX_SIZE..frame.len() - FOOTER_SIZE]
}

/// `store(2)` with float16 `h` (ID 2) and graph `g`: nodes `a` and `b`
/// joined by edge `e`, each with a deadline, and `b` attached to record 1
/// of `c`.
fn graph_store() -> Database {
    let mut db = store(2);
    db.create_collection_as("h", spec(2), F16).unwrap();
    db.create_graph("g").unwrap();
    let mut graph = db.graph_mut("g").unwrap();
    let nodes: Vec<_> = ["a", "b"]
        .map(|key| GraphOp::CreateNode {
            key: key.into(),
            labels: Vec::new(),
            properties: Metadata::new(),
        })
        .into();
    graph.write_batch(&nodes).unwrap();
    graph
        .create_edge(Some("e"), "a", "knows", "b", &Metadata::new())
        .unwrap();
    graph
        .set_node_expiry("a", Some(FAR), Expected::Any)
        .unwrap();
    graph
        .set_edge_expiry(EdgeRef::Key("e".into()), Some(FAR), Expected::Any)
        .unwrap();
    graph.attach("b", "c", 1).unwrap();
    db
}

#[test]
fn batch_decoding_never_preallocates_past_its_body() {
    // A count no body can hold is refused at the first missing entry, not
    // reserved first: reserving `usize::MAX` entries would panic on
    // capacity overflow before any allocation.
    assert_eq!(
        reason(format::decode_batch(&[], usize::MAX, 2, 0, format::V5)),
        "truncated batch entry header"
    );
}

#[test]
fn readers_refuse_frames_beyond_replay_limits_alike() {
    let _gate = Activation::on();
    let (image, floor) = imaged(&store(1));
    let tagged = Metadata::from([("k".to_owned(), MetadataValue::Integer(1))]);
    let put = committed(&image, |db| {
        db.collection_mut("c")?.put(2, &[0.5, 0.25], b"p").map(drop)
    });
    let extended = committed(&image, |db| {
        db.collection_mut("c")?
            .put_with_metadata(2, &[0.5, 0.25], b"p", &tagged)
            .map(drop)
    });
    let expiring = committed(&image, |db| {
        db.collection_mut("c")?
            .put_with_expiry(2, &[0.5, 0.25], b"p", &tagged, Some(FAR))
            .map(drop)
    });
    let delete = committed(&image, |db| db.collection_mut("c")?.delete(1).map(drop));
    let update = committed(&image, |db| {
        db.collection_mut("c")?
            .update_metadata(1, &tagged)
            .map(drop)
    });
    let puts: Vec<_> = (2..2 + MAX_BATCH_OPERATIONS as u64).map(record).collect();
    let batch = committed(&image, |db| db.collection_mut("c")?.write_batch(&puts));
    let chunk = committed(&image, |db| {
        let job = db.next_import_job_id()?;
        db.collection_mut("c")?
            .import_chunk(job, 1, &puts, POLICY)
            .map(drop)
    });
    let operations = [&put, &extended, &expiring, &delete, &update, &batch, &chunk];
    assert_eq!(operations.map(|frame| frame[4]), [1, 4, 9, 2, 8, 3, 3]);
    // The largest batch, and the largest import chunk with its marker.
    assert_eq!(u32_at(&batch, 24) as usize, MAX_BATCH_OPERATIONS);
    assert_eq!(u32_at(&chunk, 24) as usize, MAX_BATCH_OPERATIONS + 1);
    let create = format::precise_create_frame(
        floor + 1,
        9,
        "later",
        2,
        &CollectionSettings::default(),
        F32,
    );
    let drop = committed(&with(&image, &[&create]), |db| db.drop_collection("later"));

    for (at, frame) in operations.into_iter().enumerate() {
        assert_eq!(both(&with(&image, &[frame]), floor), alike("ok"), "{at}");
    }

    let mut deletes = Vec::new();
    let removed: Vec<_> = (1..=MAX_BATCH_OPERATIONS as u64 + 2)
        .map(|id| Mutation::Delete { id })
        .collect();
    format::encode_batch_entries(&removed, &mut deletes);
    let cases: [(&str, Vec<Vec<u8>>, &str); 12] = [
        ("create and drop", vec![create.clone(), drop.clone()], "ok"),
        (
            "a batch beyond the limit",
            vec![rebodied(&batch, removed.len() as u32, &deletes)],
            "invalid batch count, reserved ID or body length",
        ),
        (
            "a batch with a reserved ID",
            vec![refit(&batch, |bytes| set_u64(bytes, 16, 1))],
            "invalid batch count, reserved ID or body length",
        ),
        (
            "a vector of other dimensions",
            vec![refit(&put, |bytes| {
                set_u32(bytes, 24, 1);
                bytes.drain(PREFIX_SIZE + 4..PREFIX_SIZE + 8);
            })],
            "invalid vector or payload length",
        ),
        (
            "an extended record of other dimensions",
            vec![refit(&extended, |bytes| set_u32(bytes, 24, 3))],
            "invalid extended record length",
        ),
        (
            "an expiring record of other dimensions",
            vec![refit(&expiring, |bytes| set_u32(bytes, 24, 3))],
            "invalid expiring record length",
        ),
        (
            "an expiring record shorter than its marker and vector",
            vec![refit(&expiring, |bytes| bytes.truncate(PREFIX_SIZE + 12))],
            "invalid expiring record length",
        ),
        (
            "a delete with a count",
            vec![refit(&delete, |bytes| set_u32(bytes, 24, 1))],
            "invalid vector or payload length",
        ),
        (
            "a delete in an unknown collection",
            vec![refit(&delete, |bytes| bytes[5..7].copy_from_slice(&[7, 0]))],
            "record references unknown collection",
        ),
        (
            "a metadata update in an unknown collection",
            vec![refit(&update, |bytes| bytes[5..7].copy_from_slice(&[7, 0]))],
            "metadata update references unknown collection",
        ),
        (
            "a drop of an ID beyond 16 bits",
            vec![
                create.clone(),
                refit(&drop, |bytes| set_u64(bytes, 16, 0x1_0009)),
            ],
            "invalid collection frame fields",
        ),
        (
            "a drop with a body",
            vec![create, refit(&drop, |bytes| bytes.push(0))],
            "invalid collection frame fields",
        ),
    ];
    let failed = mismatches(cases.into_iter().map(|(name, frames, expected)| {
        let frames: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
        (name, both(&with(&image, &frames), floor), expected)
    }));
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn readers_refuse_restated_frames_beyond_replay_limits_alike() {
    let _gate = Activation::on();
    // The largest record block is accepted.
    let (image, floor) = imaged(&store(BLOCK_RECORDS as u64));
    let (_, frames) = split(&image);
    let blocks: Vec<u32> = frames
        .iter()
        .filter(|frame| frame[4] == 14)
        .map(|frame| u32_at(frame, 24))
        .collect();
    assert_eq!(blocks, [BLOCK_RECORDS as u32]);
    assert_eq!(both(&image, floor), alike("ok"));

    let (image, floor) = imaged(&store(BLOCK_RECORDS as u64 + 1));
    let (header, mut frames) = split(&image);
    frames.pop();
    let first = frames.iter().position(|frame| frame[4] == 14).unwrap();
    let counts = [first, first + 1].map(|at| u32_at(&frames[at], 24));
    assert_eq!(counts, [BLOCK_RECORDS as u32, 1]);
    let create = frames.iter().position(|frame| frame[4] == 5).unwrap();
    let image_with = |edit: &dyn Fn(&mut Vec<Vec<u8>>)| {
        let mut frames = frames.clone();
        edit(&mut frames);
        both(&sealed(&header, &frames), floor)
    };
    let cases: [(&str, &ImageEdit<'_>, &str); 5] = [
        ("the image", &|_| {}, "ok"),
        (
            "a record block with a reserved ID",
            &|frames| frames[first] = refit(&frames[first], |bytes| set_u64(bytes, 16, 1)),
            "invalid record block",
        ),
        (
            "a record block beyond the limit",
            &|frames| {
                let next = frames.remove(first + 1);
                frames[first] = refit(&frames[first], |bytes| {
                    bytes.extend_from_slice(body(&next));
                    set_u32(bytes, 24, BLOCK_RECORDS as u32 + 1);
                });
            },
            "invalid record block",
        ),
        (
            "a collection create with a count",
            &|frames| frames[create] = refit(&frames[create], |bytes| set_u32(bytes, 24, 1)),
            "invalid collection frame fields",
        ),
        (
            "a collection create of an ID beyond 16 bits",
            &|frames| frames[create] = refit(&frames[create], |bytes| set_u64(bytes, 16, 0x1_0001)),
            "invalid collection frame fields",
        ),
    ];
    let failed = mismatches(
        cases
            .into_iter()
            .map(|(name, edit, expected)| (name, image_with(edit), expected)),
    );
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn readers_admit_only_checkpoint_entries_in_graph_snapshots() {
    let _gate = Activation::on();
    let (image, floor) = imaged(&graph_store());
    let (header, mut frames) = split(&image);
    frames.pop();
    let at = frames.iter().position(|frame| frame[4] == 15).unwrap();
    assert_eq!(frames.iter().filter(|frame| frame[4] == 15).count(), 1);
    let snapshot = engine::decode_body(body(&frames[at]), u32_at(&frames[at], 24)).unwrap();
    // The checkpoint wrote a deadline for a node and an edge and an
    // attachment beside the graph, its nodes, edge and high-water.
    let tags: Vec<u8> = snapshot.entries.iter().map(GraphEntry::tag).collect();
    let node = snapshot
        .entries
        .iter()
        .find_map(|entry| match entry {
            GraphEntry::PutNode { node_id, .. } => Some(*node_id),
            _ => None,
        })
        .unwrap();
    let image_with = |entry: Option<GraphEntry>| {
        // An added entry goes before the closing high-water.
        let mut snapshot = snapshot.clone();
        let last = snapshot.entries.len() - 1;
        snapshot.entries.extend(entry);
        let end = snapshot.entries.len() - 1;
        snapshot.entries.swap(last, end);
        let encoded = engine::encode_body(&snapshot).unwrap();
        let mut frames = frames.clone();
        frames[at] = rebodied(&frames[at], snapshot.entries.len() as u32, &encoded);
        both(&sealed(&header, &frames), floor)
    };
    assert_eq!(tags.len(), 8, "{tags:?}");
    assert_eq!(image_with(None), alike("ok"));
    let mut wide = Vec::new();
    format::encode_batch_entries(
        &[Mutation::Put {
            id: 1,
            vector: vec![0.5, 0.25, 0.125],
            payload: Vec::new(),
        }],
        &mut wide,
    );
    let vector = |collection, vector: [f32; 2]| {
        crate::property_graph::frame::vector_entry(
            collection,
            &Mutation::Put {
                id: 1,
                vector: vector.into(),
                payload: Vec::new(),
            },
        )
    };
    let entries = [
        crate::property_graph::frame::vector_entry(1, &record(1)),
        // Vector puts of 0.1, which float16 cannot hold, into float32 `c`
        // and float16 `h`, and of float16-exact values into `h`, each
        // beside the same nodes and edges, so revision coverage matches.
        vector(1, [0.1, 0.25]),
        vector(2, [0.1, 0.25]),
        vector(2, [0.5, 0.25]),
        crate::property_graph::frame::vector_entry(1, &Mutation::Delete { id: 1 }),
        GraphEntry::VectorMutation {
            collection: 1,
            entry: wide,
        },
        GraphEntry::Detach {
            node_id: node,
            collection: 1,
        },
        GraphEntry::NodeExpiry {
            node_id: node,
            expires_at_ms: None,
        },
    ];
    let names = [
        "a vector put",
        "a float32 vector put of 0.1",
        "a float16 vector put of 0.1",
        "a float16-exact float16 vector put",
        "a vector delete",
        "a wide vector put",
        "a detach",
        "a cleared deadline",
    ];
    let failed = mismatches(
        names
            .into_iter()
            .zip(entries)
            .map(|(name, entry)| (name, image_with(Some(entry)), "invalid graph snapshot")),
    );
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn readers_refuse_huge_counts_before_decoding() {
    // Every reader refuses a count beyond its operation's limit, or one
    // that the body is too short to hold, from the prefix, before it
    // decodes or reserves anything for it: the frames here are a few bytes
    // long, and their counts would size reservations of up to hundreds of
    // gigabytes.
    let _gate = Activation::on();
    const BATCH: &str = "invalid batch count, reserved ID or body length";
    let huge = |frame: &[u8]| refit(frame, |bytes| set_u32(bytes, 24, u32::MAX));
    let (image, floor) = imaged(&graph_store());
    let tagged = Metadata::from([("k".to_owned(), MetadataValue::Integer(1))]);
    // Record 1 of `c` is attached; record 2 is not.
    let batch = committed(&image, |db| {
        db.collection_mut("c")?
            .write_batch(&[Mutation::Delete { id: 2 }])
    });
    let delete = committed(&image, |db| db.collection_mut("c")?.delete(2).map(drop));
    let extended = committed(&image, |db| {
        db.collection_mut("c")?
            .put_with_metadata(3, &[0.5, 0.25], b"p", &tagged)
            .map(drop)
    });
    let expiring = committed(&image, |db| {
        db.collection_mut("c")?
            .put_with_expiry(3, &[0.5, 0.25], b"p", &tagged, Some(FAR))
            .map(drop)
    });
    let update = committed(&image, |db| {
        db.collection_mut("c")?
            .update_metadata(2, &tagged)
            .map(drop)
    });
    let create = format::precise_create_frame(
        floor + 1,
        9,
        "later",
        2,
        &CollectionSettings::default(),
        F32,
    );
    let dropped = committed(&with(&image, &[&create]), |db| db.drop_collection("later"));
    let edge = committed(&image, |db| {
        db.graph_mut("g")?
            .create_edge(Some("f"), "b", "knows", "a", &Metadata::new())
            .map(drop)
    });
    let operations = [
        &batch, &delete, &extended, &expiring, &update, &dropped, &edge,
    ];
    assert_eq!(operations.map(|frame| frame[4]), [3, 2, 4, 9, 8, 6, 12]);
    assert_eq!(u32_at(&batch, 24), 1);
    let suffixes: [(&str, Vec<Vec<u8>>, &str); 13] = [
        ("a batch", vec![batch.clone()], "ok"),
        ("a batch of u32::MAX entries", vec![huge(&batch)], BATCH),
        (
            "a batch of the most entries in one entry's bytes",
            vec![refit(&batch, |bytes| {
                set_u32(bytes, 24, MAX_BATCH_OPERATIONS as u32)
            })],
            BATCH,
        ),
        (
            "a delete of u32::MAX",
            vec![huge(&delete)],
            "invalid vector or payload length",
        ),
        (
            "an extended record of u32::MAX dimensions",
            vec![huge(&extended)],
            "invalid extended record length",
        ),
        (
            "an expiring record of u32::MAX dimensions",
            vec![huge(&expiring)],
            "invalid expiring record length",
        ),
        (
            "a metadata update of u32::MAX",
            vec![huge(&update)],
            "invalid metadata-only update length",
        ),
        (
            "create and drop",
            vec![create.clone(), dropped.clone()],
            "ok",
        ),
        (
            "a drop of u32::MAX",
            vec![create, huge(&dropped)],
            "invalid collection frame fields",
        ),
        ("a graph frame", vec![edge.clone()], "ok"),
        (
            "a graph frame of u32::MAX entries",
            vec![huge(&edge)],
            "invalid graph frame count, reserved ID or body length",
        ),
        ("an extended record", vec![extended], "ok"),
        ("an expiring record", vec![expiring], "ok"),
    ];
    let mut failed = mismatches(suffixes.into_iter().map(|(name, frames, expected)| {
        let frames: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
        (name, both(&with(&image, &frames), floor), expected)
    }));

    let (header, mut frames) = split(&image);
    frames.pop();
    let only = |operation: u8| {
        let mut found = frames
            .iter()
            .enumerate()
            .filter(|(_, frame)| frame[4] == operation);
        let (first, _) = found.next().unwrap();
        assert!(found.next().is_none(), "one op-{operation} frame");
        first
    };
    let block = only(14);
    assert_eq!(u32_at(&frames[block], 24), 2);
    let snapshot = only(15);
    let create = frames.iter().position(|frame| frame[4] == 5).unwrap();
    let image_with = |at: usize, count: u32| {
        let mut frames = frames.clone();
        frames[at] = refit(&frames[at], |bytes| set_u32(bytes, 24, count));
        both(&sealed(&header, &frames), floor)
    };
    let images = [
        ("the image", image_with(block, 2), "ok"),
        (
            "a record block of u32::MAX records",
            image_with(block, u32::MAX),
            "invalid record block",
        ),
        (
            "a record block of the most records in two records' bytes",
            image_with(block, BLOCK_RECORDS as u32),
            "invalid record block",
        ),
        (
            "a collection create of u32::MAX",
            image_with(create, u32::MAX),
            "invalid collection frame fields",
        ),
        (
            "a graph snapshot of u32::MAX entries",
            image_with(snapshot, u32::MAX),
            "invalid graph snapshot",
        ),
    ];
    failed.extend(mismatches(images));
    assert!(failed.is_empty(), "{failed:#?}");
}
