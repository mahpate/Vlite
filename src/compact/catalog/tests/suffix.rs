//! Replay's graph and attachment state after the image for every reader
//! (the P3a review of 6e26f685). Full replay, the change feed and log
//! export of a complete precision log, plaintext or authenticated, apply
//! each graph frame after the image to the graph state the image restates,
//! as replay does, so they refuse alike a graph frame that state refuses;
//! and they refuse alike a record delete or expiry, by any operation, that
//! would leave an attachment without a live record without expiry, and an
//! attachment of a missing or expiring record. Valid graph updates and
//! atomic attachment and vector changes pass every reader.

use super::limits::{body, committed, graph_store, imaged, rebodied, record, set_u64, with};
use super::replayed::{at, failures};
use super::*;
use crate::MetadataValue;
use crate::property_graph::engine::{self, GraphEntry};
use crate::property_graph::{EdgeRef, GraphSchema, NodePatch};

const VIOLATES: &str = "graph frame violates graph state";
const UNATTACHABLE: &str = "graph attachment references a missing or expiring record";

/// The op-12 `frame` with the entries of its body edited.
fn reentered(frame: &[u8], edit: impl Fn(&mut Vec<GraphEntry>)) -> Vec<u8> {
    assert_eq!(frame[4], 12);
    let mut decoded = engine::decode_body(body(frame), u32_at(frame, 24)).unwrap();
    edit(&mut decoded.entries);
    let encoded = engine::encode_body(&decoded).unwrap();
    rebodied(frame, decoded.entries.len() as u32, &encoded)
}

#[test]
fn readers_follow_graph_and_attachments_after_the_image_as_replay_does() {
    let _gate = Activation::on();
    // Graph `g`: nodes `a` and `b` joined by edge `e`, `b` attached to
    // record 1 of `c`; record 2 of `c` is not attached.
    let (image, floor) = imaged(&graph_store());
    let after = |write: &dyn Fn(&mut Database) -> Result<()>| committed(&image, write);
    let graph =
        |ops: Vec<GraphOp>| after(&move |db| db.graph_mut("g")?.write_batch(&ops).map(drop));
    let vector = |mutation: Mutation| GraphOp::Vector {
        collection: "c".into(),
        mutation,
    };
    let attach = |node: &str, record: u64| GraphOp::Attach {
        node: node.into(),
        collection: "c".into(),
        record,
    };
    let tagged = Metadata::from([("k".to_owned(), MetadataValue::Integer(1))]);
    let patch_a = after(&|db| {
        let patch = NodePatch {
            set: tagged.clone(),
            ..NodePatch::default()
        };
        db.graph_mut("g")?
            .patch_node("a", &patch, Expected::Any)
            .map(drop)
    });
    let drop_a = graph(vec![
        GraphOp::DeleteEdge {
            edge: EdgeRef::Key("e".into()),
            expected: Expected::Any,
        },
        GraphOp::DeleteNode {
            key: "a".into(),
            expected: Expected::Any,
        },
    ]);
    let create_x = after(&|db| db.create_graph("x").map(drop));
    let schema = after(&|db| {
        db.graph_mut("g")?
            .put_schema(&GraphSchema::default())
            .map(drop)
    });
    // `a` attached to a new record 3 put in the same frame.
    let attach_put = graph(vec![vector(record(3)), attach("a", 3)]);
    let attach_2 = graph(vec![attach("a", 2)]);
    let put_1 = graph(vec![vector(record(1))]);
    // `b` detached from record 1 and the record deleted in one frame.
    let detach_delete_1 = graph(vec![
        GraphOp::Detach {
            node: "b".into(),
            collection: "c".into(),
        },
        vector(Mutation::Delete { id: 1 }),
    ]);
    let vector_delete_2 = graph(vec![vector(Mutation::Delete { id: 2 })]);
    let delete_2 = after(&|db| db.collection_mut("c")?.delete(2).map(drop));
    let expire_2 = after(&|db| {
        db.collection_mut("c")?
            .put_with_expiry(2, &[0.5, 0.25], b"p", &Metadata::new(), Some(FAR))
            .map(drop)
    });
    let batch_delete_2 = after(&|db| {
        db.collection_mut("c")?
            .write_batch(&[record(4), Mutation::Delete { id: 2 }])
    });
    assert_eq!(
        [delete_2[4], expire_2[4], batch_delete_2[4]],
        [2, 9, 3],
        "a delete, an expiring put and a batch"
    );
    // The same frames with record 1, which `b` holds, instead of 2.
    let delete_1 = refit(&delete_2, |bytes| set_u64(bytes, 16, 1));
    let expire_1 = refit(&expire_2, |bytes| set_u64(bytes, 16, 1));
    let mut entries = Vec::new();
    format::encode_batch_entries(&[record(4), Mutation::Delete { id: 2 }], &mut entries);
    assert_eq!(body(&batch_delete_2), entries, "the batch body");
    entries.clear();
    format::encode_batch_entries(&[record(4), Mutation::Delete { id: 1 }], &mut entries);
    let batch_delete_1 = rebodied(&batch_delete_2, 2, &entries);
    let vector_delete_1 = reentered(&vector_delete_2, |entries| {
        entries[0] = crate::property_graph::frame::vector_entry(1, &Mutation::Delete { id: 1 });
    });
    // Valid sequences the live API commits.
    let detached = |then: &dyn Fn(&mut Database) -> Result<()>| {
        with(
            &image,
            &[&after(&|db| {
                db.graph_mut("g")?.detach("b", "c")?;
                then(db)
            })],
        )
    };
    let suffix = |frames: &[&[u8]]| with(&image, frames);
    let second = |frame: &[u8]| at(frame, floor + 2);
    let cases = [
        ("a node patch", suffix(&[&patch_a]), "ok"),
        ("a node and edge delete", suffix(&[&drop_a]), "ok"),
        ("a graph create", suffix(&[&create_x]), "ok"),
        ("a schema", suffix(&[&schema]), "ok"),
        (
            "an attachment of a record put in its frame",
            suffix(&[&attach_put]),
            "ok",
        ),
        ("an attachment", suffix(&[&attach_2]), "ok"),
        ("a graph put of an attached record", suffix(&[&put_1]), "ok"),
        (
            "an atomic detach and graph delete of an attached record",
            suffix(&[&detach_delete_1]),
            "ok",
        ),
        (
            "a graph delete of an unattached record",
            suffix(&[&vector_delete_2]),
            "ok",
        ),
        (
            "a delete of an unattached record",
            suffix(&[&delete_2]),
            "ok",
        ),
        (
            "an expiry of an unattached record",
            suffix(&[&expire_2]),
            "ok",
        ),
        (
            "a batch delete of an unattached record",
            suffix(&[&batch_delete_2]),
            "ok",
        ),
        (
            "a delete of a detached record",
            detached(&|db| db.collection_mut("c")?.delete(1).map(drop)),
            "ok",
        ),
        (
            "an expiry of a detached record",
            detached(&|db| {
                db.collection_mut("c")?
                    .put_with_expiry(1, &[0.5, 0.25], b"p", &Metadata::new(), Some(FAR))
                    .map(drop)
            }),
            "ok",
        ),
        (
            "a patch of a deleted node",
            suffix(&[&drop_a, &second(&patch_a)]),
            VIOLATES,
        ),
        (
            "a graph created twice",
            suffix(&[&create_x, &second(&create_x)]),
            VIOLATES,
        ),
        (
            "a delete of a deleted edge",
            suffix(&[
                &drop_a,
                &second(&reentered(&patch_a, |entries| {
                    entries[0] = GraphEntry::DeleteEdge {
                        edge_id: engine::EdgeId(0),
                    };
                })),
            ]),
            VIOLATES,
        ),
        (
            "a delete of an attached record",
            suffix(&[&delete_1]),
            UNATTACHABLE,
        ),
        (
            "an expiry of an attached record",
            suffix(&[&expire_1]),
            UNATTACHABLE,
        ),
        (
            "a batch delete of an attached record",
            suffix(&[&batch_delete_1]),
            UNATTACHABLE,
        ),
        (
            "a graph delete of an attached record",
            suffix(&[&vector_delete_1]),
            UNATTACHABLE,
        ),
        (
            "an attachment of a deleted record",
            suffix(&[&delete_2, &second(&attach_2)]),
            UNATTACHABLE,
        ),
        (
            "an attachment of an expiring record",
            suffix(&[&expire_2, &second(&attach_2)]),
            UNATTACHABLE,
        ),
        (
            "a delete of a record attached after the image",
            suffix(&[&attach_2, &second(&delete_2)]),
            UNATTACHABLE,
        ),
        (
            "an expiry of a record attached after the image",
            suffix(&[&attach_2, &second(&expire_2)]),
            UNATTACHABLE,
        ),
        (
            "a batch delete of a record attached after the image",
            suffix(&[&attach_2, &second(&batch_delete_2)]),
            UNATTACHABLE,
        ),
        (
            "a graph delete of a record attached after the image",
            suffix(&[&attach_2, &second(&vector_delete_2)]),
            UNATTACHABLE,
        ),
    ];
    let failed = failures(&cases, floor);
    assert!(failed.is_empty(), "{failed:#?}");
}
