//! Replay's operation semantics for every reader (the P3a review of
//! 6f20f1d7). Full replay, the change feed and log export of a complete
//! precision log, plaintext or authenticated, refuse alike and with
//! replay's reason a drop of an unknown collection or of one that holds
//! records or an open import job; settings that do not decode or name no
//! live collection; a graph snapshot that is not canonical or attaches a
//! record that is missing or expiring; and the metadata, expiry, vector,
//! import, create and record-block frames that replay refuses for the
//! records, jobs and names the log holds so far.

use super::limits::{
    body, both, committed, graph_store, imaged, mismatches, rebodied, record, set_u64, store, with,
};
use super::*;
use crate::property_graph::GraphSchema;
use crate::property_graph::engine::{self, GraphEntry};
use crate::vectors::VectorSpec;
use crate::{DistanceMetric, MetadataValue};

const UNKNOWN: &str = "drop of unknown collection";
const FILLED: &str = "drop of non-empty collection";
const UNSET: &str = "settings for unknown collection";
const NON_CANONICAL: &str = "invalid graph snapshot";
const UNATTACHABLE: &str = "graph attachment references a missing or expiring record";

/// A write committed after a log under test.
type Write<'a> = dyn Fn(&mut Database) -> Result<()> + 'a;
/// An edit of the entries of a graph snapshot under test.
type Entries<'a> = dyn Fn(&mut Vec<GraphEntry>) + 'a;

/// `frame` committed at `sequence` instead.
fn at(frame: &[u8], sequence: u64) -> Vec<u8> {
    refit(frame, |bytes| set_u64(bytes, 8, sequence))
}

/// The frames of the whole frames `bytes`.
fn frames_of(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut log = vec![0; 32];
    log.extend_from_slice(bytes);
    split(&log).1
}

/// Every named case whose readers do not all give its expected outcome.
fn failures(cases: &[(&str, Vec<u8>, &str)], floor: u64) -> Vec<String> {
    mismatches(
        cases
            .iter()
            .map(|(name, log, expected)| (*name, both(log, floor), *expected)),
    )
}

#[test]
fn readers_refuse_drops_and_settings_alike() {
    let _gate = Activation::on();
    let (image, floor) = imaged(&store(1));
    let after = |write: &Write| with(&image, &[&committed(&image, write)]);
    let dropping = format::collection_drop_frame;
    let setting =
        |sequence, id| format::settings_frame(sequence, id, &CollectionSettings::default());
    // `d` (ID 2) created after the image, then dropped or given record 1.
    let dropped = after(&|db| {
        db.create_collection_as("d", spec(2), F32)?;
        db.drop_collection("d")
    });
    let filled = after(&|db| {
        db.create_collection_as("d", spec(2), F32)?;
        db.collection_mut("d")?.write_batch(&[record(1)])
    });
    // `d` whose only record an import job wrote and the job's collection
    // then deleted, while the job is open or once it finished.
    let imported = |finish: bool| {
        after(&|db| {
            db.create_collection_as("d", spec(2), F32)?;
            let job = db.next_import_job_id()?;
            let mut d = db.collection_mut("d")?;
            d.import_chunk(job, 1, &[record(1)], POLICY)?;
            if finish {
                d.finish_import(job)?;
            }
            d.delete(1).map(|_| ())
        })
    };
    let emptied = after(&|db| db.collection_mut("c")?.delete(1).map(|_| ()));
    let settings = after(&|db| {
        db.set_collection_settings("c", CollectionSettings::default())
            .map(|_| ())
    });
    let unsupported = refit(&setting(floor + 1, 1), |bytes| bytes[PREFIX_SIZE] = 2);
    let truncated = |id| {
        refit(&setting(floor + 1, id), |bytes| {
            bytes.truncate(PREFIX_SIZE + 5)
        })
    };
    let cases = [
        ("a drop of an empty collection", dropped.clone(), "ok"),
        ("settings", settings, "ok"),
        (
            "a drop of an unknown collection",
            with(&image, &[&dropping(floor + 1, 9)]),
            UNKNOWN,
        ),
        (
            "a drop of a dropped collection",
            with(&dropped, &[&dropping(floor + 3, 2)]),
            UNKNOWN,
        ),
        (
            "a drop of a collection the image fills",
            with(&image, &[&dropping(floor + 1, 1)]),
            FILLED,
        ),
        (
            "a drop once its records are deleted",
            with(&emptied, &[&dropping(floor + 2, 1)]),
            "ok",
        ),
        (
            "a drop of a collection given a record",
            with(&filled, &[&dropping(floor + 3, 2)]),
            FILLED,
        ),
        (
            "a drop of a collection with an open import",
            with(&imported(false), &[&dropping(floor + 4, 2)]),
            FILLED,
        ),
        (
            "a drop once its import finished",
            with(&imported(true), &[&dropping(floor + 5, 2)]),
            "ok",
        ),
        (
            "settings for an unknown collection",
            with(&image, &[&setting(floor + 1, 9)]),
            UNSET,
        ),
        (
            "settings for a dropped collection",
            with(&dropped, &[&setting(floor + 3, 2)]),
            UNSET,
        ),
        (
            "settings of an unsupported version",
            with(&image, &[&unsupported]),
            "unsupported collection settings version",
        ),
        (
            "truncated settings",
            with(&image, &[&truncated(1)]),
            "truncated collection settings",
        ),
        // Replay decodes settings before it looks up their collection.
        (
            "truncated settings for an unknown collection",
            with(&image, &[&truncated(9)]),
            "truncated collection settings",
        ),
    ];
    let mut failed = failures(&cases, floor);
    // `d` whose job the image restates as open, or as finished.
    for (finish, name, expected) in [
        (
            false,
            "a drop of a collection with a restated open import",
            FILLED,
        ),
        (true, "a drop once its restated import finished", "ok"),
    ] {
        let mut db = store(1);
        db.create_collection_as("d", spec(2), F32).unwrap();
        let job = db.next_import_job_id().unwrap();
        let mut d = db.collection_mut("d").unwrap();
        d.import_chunk(job, 1, &[record(1)], POLICY).unwrap();
        if finish {
            d.finish_import(job).unwrap();
        }
        d.delete(1).unwrap();
        let (image, floor) = imaged(&db);
        let log = with(&image, &[&dropping(floor + 1, 2)]);
        failed.extend(failures(&[(name, log, expected)], floor));
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn readers_refuse_restated_settings_alike() {
    let _gate = Activation::on();
    let mut db = store(1);
    db.set_collection_settings("default", CollectionSettings::default())
        .unwrap();
    let (image, floor) = imaged(&db);
    let (header, mut frames) = split(&image);
    frames.pop();
    // The image restates `default`'s settings before its table.
    let at = frames.iter().position(|frame| frame[4] == 7).unwrap();
    let image_with = |edit: &Edit| {
        let mut frames = frames.clone();
        frames[at] = refit(&frames[at], edit);
        both(&sealed(&header, &frames), floor)
    };
    let cases: [(&str, &Edit, &str); 4] = [
        ("restated settings", &|_| {}, "ok"),
        (
            "settings for an unknown collection",
            &|bytes| set_u64(bytes, 16, 7),
            UNSET,
        ),
        (
            "settings of an unsupported version",
            &|bytes| bytes[PREFIX_SIZE] = 2,
            "unsupported collection settings version",
        ),
        (
            "truncated settings",
            &|bytes| bytes.truncate(PREFIX_SIZE + 5),
            "truncated collection settings",
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
fn readers_load_graph_snapshots_as_replay_loads_them() {
    let _gate = Activation::on();
    let mut db = graph_store();
    // Record 2 of `c` expires.
    db.collection_mut("c")
        .unwrap()
        .put_with_expiry(2, &[0.5, 0.25], b"p", &Metadata::new(), Some(FAR))
        .unwrap();
    let (image, floor) = imaged(&db);
    let (header, mut frames) = split(&image);
    frames.pop();
    let at = frames.iter().position(|frame| frame[4] == 15).unwrap();
    let snapshot = engine::decode_body(body(&frames[at]), u32_at(&frames[at], 24)).unwrap();
    let find = |wanted: fn(&GraphEntry) -> bool| snapshot.entries.iter().position(wanted).unwrap();
    // The checkpoint wrote, after `g`'s create: `a` and its deadline, `b`,
    // `e` and its deadline, `b`'s attachment and the high-water.
    let node_a = find(|entry| matches!(entry, GraphEntry::PutNode { key, .. } if key == "a"));
    let node_b = find(|entry| matches!(entry, GraphEntry::PutNode { key, .. } if key == "b"));
    let edge = find(|entry| matches!(entry, GraphEntry::PutEdge { .. }));
    let attach = find(|entry| matches!(entry, GraphEntry::Attach { .. }));
    let high = snapshot.entries.len() - 1;
    let tags: Vec<u8> = snapshot.entries.iter().map(GraphEntry::tag).collect();
    assert_eq!(
        (node_a, node_b, edge, attach, high),
        (1, 3, 4, 6, 7),
        "{tags:?}"
    );
    let image_with = |edit: &Entries| {
        let mut snapshot = snapshot.clone();
        edit(&mut snapshot.entries);
        let encoded = engine::encode_body(&snapshot).unwrap();
        let mut frames = frames.clone();
        frames[at] = rebodied(&frames[at], snapshot.entries.len() as u32, &encoded);
        both(&sealed(&header, &frames), floor)
    };
    let schema = || GraphEntry::PutSchema {
        schema: GraphSchema::default(),
    };
    let attach_to = |entries: &mut Vec<GraphEntry>, to: u16, record: u64| {
        if let GraphEntry::Attach {
            collection,
            record_id,
            ..
        } = &mut entries[attach]
        {
            (*collection, *record_id) = (to, record);
        }
    };
    // Node and edge IDs keep their order, so revision coverage matches.
    let cases: [(&str, &Entries, &str); 11] = [
        ("the snapshot", &|_| {}, "ok"),
        (
            "a schema after the graph's create",
            &|entries| entries.insert(1, schema()),
            "ok",
        ),
        (
            "a schema after a node",
            &|entries| entries.insert(node_b, schema()),
            NON_CANONICAL,
        ),
        (
            "a node deadline apart from its node",
            &|entries| {
                let deadline = entries.remove(node_a + 1);
                entries.insert(node_b, deadline);
            },
            NON_CANONICAL,
        ),
        (
            "an edge before its endpoint",
            &|entries| {
                let deadline = entries.remove(edge + 1);
                let put = entries.remove(edge);
                entries.insert(node_b, deadline);
                entries.insert(node_b, put);
            },
            NON_CANONICAL,
        ),
        (
            "a duplicate node key",
            &|entries| {
                if let GraphEntry::PutNode { key, .. } = &mut entries[node_b] {
                    *key = "a".into();
                }
            },
            NON_CANONICAL,
        ),
        (
            "a high-water below the graph's IDs",
            &|entries| {
                if let GraphEntry::GraphHighWater { next_node_id, .. } = &mut entries[high] {
                    *next_node_id = 1;
                }
            },
            NON_CANONICAL,
        ),
        (
            "a graph without its high-water",
            &|entries| {
                entries.pop();
            },
            NON_CANONICAL,
        ),
        (
            "an attachment to a missing record",
            &|entries| attach_to(entries, 1, 99),
            UNATTACHABLE,
        ),
        (
            "an attachment to an expiring record",
            &|entries| attach_to(entries, 1, 2),
            UNATTACHABLE,
        ),
        (
            "an attachment in an unknown collection",
            &|entries| attach_to(entries, 9, 1),
            UNATTACHABLE,
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
fn readers_track_records_jobs_and_names_as_replay_does() {
    let _gate = Activation::on();
    let (image, floor) = imaged(&store(1));
    let after = |write: &Write| committed(&image, write);
    let mut metadata = Metadata::new();
    metadata.insert("k".into(), MetadataValue::Integer(1));
    let update = after(&|db| {
        db.collection_mut("c")?
            .update_metadata(1, &metadata)
            .map(|_| ())
    });
    let expiring = after(&|db| {
        db.collection_mut("c")?
            .put_with_expiry(2, &[0.5, 0.25], b"p", &Metadata::new(), Some(FAR))
            .map(|_| ())
    });
    assert_eq!((update[4], expiring[4]), (8, 9));
    let chunk = after(&|db| {
        let job = db.next_import_job_id()?;
        db.collection_mut("c")?
            .import_chunk(job, 1, &[record(2)], POLICY)
            .map(|_| ())
    });
    let delete = after(&|db| db.collection_mut("c")?.delete(1).map(|_| ()));
    let named = frames_of(&after(&|db| {
        let mut c = db.collection_mut("c")?;
        let spec = VectorSpec {
            dimensions: 2,
            metric: DistanceMetric::SquaredL2,
            active_version: "v1".into(),
            history: 1,
        };
        c.configure_vector("n", spec)?;
        c.put_vector_version(1, "n", "v1", vec![0.5, 0.25])
    }));
    let operations: Vec<u8> = named.iter().map(|frame| frame[4]).collect();
    assert_eq!(operations, [10, 10]);
    let create = |name| {
        let settings = CollectionSettings::default();
        format::precise_create_frame(floor + 1, 2, name, 2, &settings, F32)
    };
    let suffix = |frames: &[&[u8]]| with(&image, frames);
    let cases = [
        ("a metadata update", suffix(&[&update]), "ok"),
        (
            "a metadata update of a missing record",
            suffix(&[&refit(&update, |bytes| set_u64(bytes, 16, 99))]),
            "metadata update references missing record",
        ),
        (
            "a malformed metadata update",
            suffix(&[&refit(&update, |bytes| bytes.truncate(PREFIX_SIZE + 5))]),
            "invalid metadata update",
        ),
        ("an expiring put", suffix(&[&expiring]), "ok"),
        (
            "an invalid expiry flag",
            suffix(&[&refit(&expiring, |bytes| bytes[PREFIX_SIZE] = 2)]),
            "invalid expiry marker",
        ),
        (
            "a deadline without its flag",
            suffix(&[&refit(&expiring, |bytes| bytes[PREFIX_SIZE] = 0)]),
            "invalid expiry marker",
        ),
        ("an import chunk", suffix(&[&chunk]), "ok"),
        (
            "an import chunk repeated",
            suffix(&[&chunk, &at(&chunk, floor + 2)]),
            "non-contiguous import chunk number",
        ),
        ("named vectors", suffix(&[&named[0], &named[1]]), "ok"),
        (
            "named vectors of a deleted record",
            suffix(&[
                &named[0],
                &at(&delete, floor + 2),
                &at(&named[1], floor + 3),
            ]),
            "vector extension references missing record",
        ),
        ("a create", suffix(&[&create("d")]), "ok"),
        (
            "a create of a live name",
            suffix(&[&create("c")]),
            "duplicate collection name",
        ),
    ];
    let mut failed = failures(&cases, floor);
    // An image whose second record block restates its first block's
    // records again.
    let (image, floor) = imaged(&store(4097));
    let (header, mut frames) = split(&image);
    frames.pop();
    let blocks: Vec<usize> = (0..frames.len())
        .filter(|&at| frames[at][4] == 14)
        .collect();
    assert_eq!(blocks.len(), 2);
    let restated = both(&sealed(&header, &frames), floor);
    frames[blocks[1]] = frames[blocks[0]].clone();
    let overlapping = both(&sealed(&header, &frames), floor);
    failed.extend(mismatches([
        ("record blocks", restated, "ok"),
        (
            "record blocks that overlap",
            overlapping,
            "invalid record block",
        ),
    ]));
    assert!(failed.is_empty(), "{failed:#?}");
}
