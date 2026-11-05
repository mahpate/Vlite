//! Expectations come from the 0.7.1 design note (section 2): the reserved
//! labels and keys are the literal lists below, not what the code derives.

use super::*;
use crate::memory::binding::RECORD_LABEL;
use crate::property_graph::{EdgeRef, GraphSchema, NodePatch};
use crate::{Expected, Metadata, MetadataValue, Mutation};

/// The documented reserved labels: `VliteMemory` and everything after it.
const LABELS: [&str; 9] = [
    "VliteMemory",
    "VliteMemoryScope",
    "VliteMemoryReceipt",
    "VliteMemoryReceiptOutcome",
    "VliteMemoryContent",
    "VliteMemoryErasureJob",
    "VliteMemoryBinding",
    "VliteMemoryFuture",
    "VliteMemory_",
];

/// Near misses: other case, other spelling, the prefix in the middle.
const OPEN_LABELS: [&str; 9] = [
    "",
    "Vlite",
    "VliteMemor",
    "vlitememory",
    "vliteMemory",
    "VLITEMEMORY",
    "Memory",
    "MyVliteMemory",
    "Vlite Memory",
];

/// The documented reserved key prefixes and the one reserved exact key.
const PREFIXES: [&str; 5] = [
    "scope:v1:",
    "mem:v1:",
    "receipt:v1:",
    "receipt-outcome:v1:",
    "content:v1:",
];
const RECORD: &str = "binding:v1";

fn keys_with(prefix: &str) -> [String; 3] {
    [
        prefix.to_owned(),
        format!("{prefix}0"),
        format!("{prefix}00ff:0001"),
    ]
}

fn create(key: &str, labels: &[&str]) -> GraphOp {
    GraphOp::CreateNode {
        key: key.into(),
        labels: labels.iter().map(|label| (*label).into()).collect(),
        properties: Metadata::new(),
    }
}

fn upsert(key: &str, labels: &[&str]) -> GraphOp {
    GraphOp::UpsertNode {
        key: key.into(),
        labels: labels.iter().map(|label| (*label).into()).collect(),
        properties: Metadata::new(),
        expected: Expected::Any,
    }
}

fn patch(key: &str, patch: NodePatch) -> GraphOp {
    GraphOp::PatchNode {
        key: key.into(),
        patch,
        expected: Expected::Any,
    }
}

fn adding(labels: &[&str]) -> NodePatch {
    NodePatch {
        add_labels: labels.iter().map(|label| (*label).into()).collect(),
        ..NodePatch::default()
    }
}

fn removing(labels: &[&str]) -> NodePatch {
    NodePatch {
        remove_labels: labels.iter().map(|label| (*label).into()).collect(),
        ..NodePatch::default()
    }
}

#[test]
fn every_label_that_starts_with_vlitememory_is_reserved() {
    for label in LABELS {
        assert!(reserved_label(label), "{label}");
    }
}

#[test]
fn no_other_label_is_reserved() {
    for label in OPEN_LABELS {
        assert!(!reserved_label(label), "{label:?}");
    }
    assert!(!reserved_label("Person"));
}

#[test]
fn every_key_with_a_reserved_prefix_is_reserved_and_so_is_the_record_key() {
    for prefix in PREFIXES {
        for key in keys_with(prefix) {
            assert!(reserved_key(&key), "{key}");
        }
    }
    assert!(reserved_key(RECORD));
}

#[test]
fn near_misses_of_the_reserved_keys_are_open() {
    for key in [
        "",
        "scope:v1",
        "scope:v2:x",
        "scope::v1:x",
        "xscope:v1:x",
        "Scope:v1:x",
        "mem:v1",
        "mem:v2:x",
        "receipt:v1",
        "receipt:v2:x",
        "receipt-outcome:v1",
        "content:v1",
        "content:v2:x",
        "binding:v1:x",
        "binding:v10",
        "binding:v2",
        "binding",
        "Binding:v1",
        " binding:v1",
        // Edge keys are not node keys.
        "content-edge:v1:00:0000",
        "reference:v1:00",
        "memory",
        "person:1",
    ] {
        assert!(!reserved_key(key), "{key:?}");
    }
}

#[test]
fn every_name_the_memory_core_writes_is_reserved() {
    for (label, prefix) in MEMORY_NODE_KINDS {
        assert!(reserved_label(label), "{label}");
        assert!(reserved_key(&format!("{prefix}x")), "{prefix}");
    }
    assert!(reserved_label(RECORD_LABEL));
    assert!(reserved_key(RECORD_KEY));
}

#[test]
fn creating_or_replacing_a_node_with_a_reserved_name_is_refused() {
    for label in LABELS {
        for op in [create("k", &[label]), upsert("k", &[label])] {
            assert!(refuses_raw(&op), "{op:?}");
        }
        // One reserved label among ordinary ones is enough.
        assert!(refuses_raw(&create("k", &["Person", label, "Thing"])));
        assert!(refuses_raw(&upsert("k", &["Person", label])));
    }
    for prefix in PREFIXES {
        for key in keys_with(prefix) {
            for op in [create(&key, &[]), upsert(&key, &["Person"])] {
                assert!(refuses_raw(&op), "{op:?}");
            }
        }
    }
    for op in [create(RECORD, &[]), upsert(RECORD, &["Person"])] {
        assert!(refuses_raw(&op), "{op:?}");
    }
}

#[test]
fn creating_or_replacing_an_ordinary_node_is_allowed() {
    for label in OPEN_LABELS {
        assert!(!refuses_raw(&create("k", &[label])), "{label:?}");
        assert!(!refuses_raw(&upsert("k", &[label])), "{label:?}");
    }
    assert!(!refuses_raw(&create("person:1", &["Person", "Employee"])));
    assert!(!refuses_raw(&upsert("person:1", &[])));
    assert!(!refuses_raw(&create("binding:v1:x", &["Person"])));
}

#[test]
fn patching_a_reserved_node_or_adding_a_reserved_label_is_refused() {
    for label in LABELS {
        assert!(refuses_raw(&patch("k", adding(&[label]))), "{label}");
        assert!(refuses_raw(&patch("k", adding(&["Person", label]))));
    }
    for prefix in PREFIXES {
        for key in keys_with(prefix) {
            assert!(refuses_raw(&patch(&key, NodePatch::default())), "{key}");
            assert!(refuses_raw(&patch(&key, adding(&["Person"]))), "{key}");
            // The key decides, whatever the patch does: a node with a reserved
            // key is deleted, not patched.
            for label in LABELS {
                assert!(refuses_raw(&patch(&key, removing(&[label]))), "{key}");
            }
        }
    }
    assert!(refuses_raw(&patch(RECORD, NodePatch::default())));
}

#[test]
fn an_ordinary_patch_and_the_removal_of_a_reserved_label_are_allowed() {
    assert!(!refuses_raw(&patch("person:1", NodePatch::default())));
    assert!(!refuses_raw(&patch("person:1", adding(&["Person"]))));
    // A node planted by an earlier release under an ordinary key can be
    // cleaned up. The guard reads the request only, so a patch that adds no
    // label is allowed whatever labels the node already carries.
    for label in LABELS {
        assert!(
            !refuses_raw(&patch("planted", removing(&[label]))),
            "{label}"
        );
    }
    // Setting a property whose name looks reserved is not a label or a key.
    let mut set = Metadata::new();
    set.insert("VliteMemory".into(), MetadataValue::String("x".into()));
    let named = NodePatch {
        set,
        unset: vec!["VliteMemoryScope".into()],
        ..NodePatch::default()
    };
    assert!(!refuses_raw(&patch("person:1", named)));
}

#[test]
fn deleting_a_reserved_node_stays_allowed_so_a_planted_one_can_be_removed() {
    for key in ["scope:v1:00", "mem:v1:00", "receipt:v1:00", RECORD] {
        let op = GraphOp::DeleteNode {
            key: key.into(),
            expected: Expected::Any,
        };
        assert!(!refuses_raw(&op), "{key}");
        let expiry = GraphOp::SetNodeExpiry {
            key: key.into(),
            expires_at_ms: Some(1),
            expected: Expected::Any,
        };
        assert!(!refuses_raw(&expiry), "{key}");
    }
}

#[test]
fn edges_attachments_vectors_and_schemas_are_not_node_creations() {
    let edge = |source: &str, destination: &str| GraphOp::CreateEdge {
        key: Some("mem:v1:edge".into()),
        source: source.into(),
        relationship: "VliteMemoryRelated".into(),
        destination: destination.into(),
        properties: Metadata::new(),
    };
    for op in [
        edge("mem:v1:a", "mem:v1:b"),
        GraphOp::EnsureEdge {
            source: "scope:v1:a".into(),
            relationship: "VliteMemory".into(),
            destination: "scope:v1:b".into(),
            properties: Metadata::new(),
        },
        GraphOp::PatchEdge {
            edge: EdgeRef::Key("mem:v1:edge".into()),
            set: Metadata::new(),
            unset: Vec::new(),
            expected: Expected::Any,
        },
        GraphOp::DeleteEdge {
            edge: EdgeRef::Key("mem:v1:edge".into()),
            expected: Expected::Any,
        },
        GraphOp::Attach {
            node: "mem:v1:a".into(),
            collection: "VliteMemory".into(),
            record: 1,
        },
        GraphOp::Detach {
            node: "mem:v1:a".into(),
            collection: "VliteMemory".into(),
        },
        GraphOp::Vector {
            collection: "VliteMemory".into(),
            mutation: Mutation::Delete { id: 1 },
        },
        GraphOp::PutSchema {
            schema: GraphSchema::default(),
        },
    ] {
        assert!(!refuses_raw(&op), "{op:?}");
    }
}

#[test]
fn one_refused_operation_refuses_the_whole_batch_wherever_it_stands() {
    let ordinary = || create("person:1", &["Person"]);
    assert!(!refuses_raw_batch(&[]));
    assert!(!refuses_raw_batch(&[ordinary(), ordinary()]));
    let refused = [
        create("scope:v1:00", &[]),
        create("k", &["VliteMemoryScope"]),
        patch("k", adding(&["VliteMemory"])),
    ];
    for bad in refused {
        assert!(refuses_raw_batch(std::slice::from_ref(&bad)));
        assert!(refuses_raw_batch(&[bad.clone(), ordinary()]));
        assert!(refuses_raw_batch(&[ordinary(), bad.clone()]));
        assert!(refuses_raw_batch(&[ordinary(), bad, ordinary()]));
    }
}
