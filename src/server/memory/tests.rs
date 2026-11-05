use super::*;
use crate::memory::binding::parse;
use crate::memory::{MemoryRef, ReceiptOperation};
use crate::{CollectionSpec, CreateOptions, FormatVersion, Metadata, property_graph::GraphOp};

/// The validation clock of these starts: `retired_at_ms` below may not precede it.
const NOW: u64 = 5;

/// A start at the fixed clock. The records of entries the file does not name
/// are not what these tests look at.
fn prepare(db: &mut Database, bindings: &reserved::Bindings) -> Result<(), &'static str> {
    super::prepare(db, bindings, NOW).map(|_| ())
}

const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"},{"agent":"b","user":"u"}]},{"principal":"bcdefghij234","scopes":[{"agent":"a"}]}]}]}"#;

fn bindings(text: &str) -> reserved::Bindings {
    reserved::Bindings::new(parse(text.as_bytes()).unwrap())
}

fn spec() -> CollectionSpec {
    CollectionSpec {
        dimensions: 2,
        settings: Default::default(),
    }
}

fn database(graph: &str, evidence: &str) -> Database {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_graph(graph).unwrap();
    db.create_collection(evidence, spec()).unwrap();
    db
}

fn scope(tenant: &str, agent: &str, user: Option<&str>) -> MemoryScope {
    MemoryScope {
        tenant: tenant.into(),
        agent: agent.into(),
        user: user.map(str::to_owned),
        session: None,
    }
}

/// An unprovisioned scope is `InvalidScope`; a provisioned one has no
/// receipt under an unused key.
fn provisioned(db: &Database, graph: &str, scope: &MemoryScope) -> bool {
    MemoryRef::bind(db, graph, None)
        .unwrap()
        .receipt(ReceiptOperation::Forget, scope, "unused", 1)
        .map(|receipt| assert!(receipt.is_none()))
        .is_ok()
}

#[test]
fn granted_scopes_are_provisioned_idempotently() {
    let mut db = database("notes_graph", "notes_ev");
    let bindings = bindings(BINDINGS);
    assert_eq!(prepare(&mut db, &bindings), Ok(()));
    let sequence = db.sequence;
    assert_eq!(prepare(&mut db, &bindings), Ok(()));
    assert_eq!(db.sequence, sequence, "a second startup writes nothing");
    assert!(provisioned(&db, "notes_graph", &scope("@root", "a", None)));
    assert!(provisioned(
        &db,
        "notes_graph",
        &scope("@root", "b", Some("u"))
    ));
    assert!(!provisioned(&db, "notes_graph", &scope("@root", "b", None)));
}

#[test]
fn retired_bindings_are_checked_but_never_provisioned() {
    let mut db = database("notes_graph", "notes_ev");
    let retired = BINDINGS.replace(
        r#""state":"active""#,
        r#""state":"retired","retired_at_ms":5"#,
    );
    assert_eq!(prepare(&mut db, &bindings(&retired)), Ok(()));
    assert!(!provisioned(&db, "notes_graph", &scope("@root", "a", None)));
    let missing = retired.replace("\"notes_graph\"", "\"absent_graph\"");
    assert_eq!(prepare(&mut db, &bindings(&missing)), Err(GRAPH_MISSING));
}

#[test]
fn a_read_only_database_is_checked_but_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read-only.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.create_graph("notes_graph").unwrap();
    db.create_collection("notes_ev", spec()).unwrap();
    drop(db);
    let mut db = Database::open_read_only(&path).unwrap();
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Ok(()));
    assert!(!provisioned(&db, "notes_graph", &scope("@root", "a", None)));
    let missing = BINDINGS.replace("\"notes_ev\"", "\"absent_ev\"");
    assert_eq!(prepare(&mut db, &bindings(&missing)), Err(EVIDENCE_MISSING));
}

#[test]
fn missing_resources_are_refused_before_any_write() {
    let mut db = database("other_graph", "notes_ev");
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Err(GRAPH_MISSING));
    let mut db = database("notes_graph", "other_ev");
    let sequence = db.sequence;
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Err(EVIDENCE_MISSING));
    assert_eq!(db.sequence, sequence);
    let without_evidence = BINDINGS.replace("\"notes_ev\"", "null");
    assert_eq!(prepare(&mut db, &bindings(&without_evidence)), Ok(()));
}

#[test]
fn ordinary_data_is_never_adopted() {
    let mut db = database("notes_graph", "notes_ev");
    db.graph_mut("notes_graph")
        .unwrap()
        .write_batch(&[GraphOp::CreateNode {
            key: "ordinary".into(),
            labels: vec!["Person".into()],
            properties: Metadata::new(),
        }])
        .unwrap();
    let sequence = db.sequence;
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Err(IN_USE));
    assert_eq!(db.sequence, sequence);
    let mut db = database("notes_graph", "notes_ev");
    db.collection_mut("notes_ev")
        .unwrap()
        .put(1, &[1.0, 0.0], b"")
        .unwrap();
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Err(IN_USE));
}

#[test]
fn tenant_bindings_use_tenant_qualified_resources() {
    let tenant = BINDINGS.replace(
        r#""tenant":"@root""#,
        r#""tenant":"0123456789abcdef0123456789abcdef""#,
    );
    let mut db = database("notes_graph", "notes_ev");
    assert_eq!(prepare(&mut db, &bindings(&tenant)), Err(GRAPH_MISSING));
    let mut db = database(
        "t0123456789abcdef0123456789abcdef__notes_graph",
        "t0123456789abcdef0123456789abcdef__notes_ev",
    );
    assert_eq!(prepare(&mut db, &bindings(&tenant)), Ok(()));
    assert!(provisioned(
        &db,
        "t0123456789abcdef0123456789abcdef__notes_graph",
        &scope("0123456789abcdef0123456789abcdef", "a", None)
    ));
}

#[test]
fn a_vector_only_database_refuses_bindings() {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V4),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Err(GRAPH_DISABLED));
}

#[test]
fn a_later_refused_binding_leaves_earlier_scopes_unwritten() {
    let entry = &BINDINGS[BINDINGS.find("[{").unwrap() + 1..BINDINGS.len() - 2];
    let other = entry
        .replace(r#""binding":"notes""#, r#""binding":"other""#)
        .replace("\"notes_graph\"", "\"other_graph\"")
        .replace("\"notes_ev\"", "null");
    let both = BINDINGS.replace(entry, &format!("{entry},{other}"));
    let mut db = database("notes_graph", "notes_ev");
    let sequence = db.sequence;
    assert_eq!(prepare(&mut db, &bindings(&both)), Err(GRAPH_MISSING));
    assert_eq!(db.sequence, sequence);
    db.create_graph("other_graph").unwrap();
    assert_eq!(prepare(&mut db, &bindings(&both)), Ok(()));
    assert!(provisioned(&db, "notes_graph", &scope("@root", "a", None)));
    assert!(provisioned(&db, "other_graph", &scope("@root", "a", None)));
}

#[test]
fn a_scope_that_cannot_be_provisioned_refuses_startup() {
    use crate::Expected;
    use crate::property_graph::{FindNodes, ReadOptions};
    let mut db = database("notes_graph", "notes_ev");
    let bindings = bindings(BINDINGS);
    assert_eq!(prepare(&mut db, &bindings), Ok(()));
    let scopes = db
        .graph("notes_graph")
        .unwrap()
        .find_nodes(
            &FindNodes {
                label: Some("VliteMemoryScope".into()),
                limit: Some(1),
                ..FindNodes::default()
            },
            &ReadOptions::default(),
        )
        .unwrap()
        .output;
    let mut properties = Metadata::new();
    properties.insert("tuple".into(), crate::MetadataValue::String("00".into()));
    db.graph_mut("notes_graph")
        .unwrap()
        .write_batch(&[GraphOp::UpsertNode {
            key: scopes[0].key.clone(),
            labels: vec!["VliteMemoryScope".into()],
            properties,
            expected: Expected::Any,
        }])
        .unwrap();
    assert_eq!(prepare(&mut db, &bindings), Err(PROVISION_FAILED));
}

// ---------------------------------------------------------------- records

const KEY: &str = "binding:v1";
const DAY: u64 = 86_400_000;

fn record(db: &Database, graph: &str) -> Option<crate::property_graph::NodeView> {
    db.graph(graph).unwrap().get_node(KEY).unwrap().output
}

fn started(text: &str) -> (Database, reserved::Bindings) {
    let mut db = database("notes_graph", "notes_ev");
    let bindings = bindings(text);
    assert_eq!(prepare(&mut db, &bindings), Ok(()));
    (db, bindings)
}

#[test]
fn startup_writes_the_record_of_every_named_binding_once() {
    let (mut db, bindings) = started(BINDINGS);
    let node = record(&db, "notes_graph").expect("the record");
    let binding = bindings.iter().next().unwrap();
    assert_eq!(node.properties, Recorded::of(binding).properties());
    let sequence = db.sequence;
    assert_eq!(prepare(&mut db, &bindings), Ok(()));
    assert_eq!(record(&db, "notes_graph"), Some(node));
    assert_eq!(db.sequence, sequence, "the same file adds no commit");
}

#[test]
fn a_retired_binding_is_recorded_but_not_provisioned() {
    let retired = BINDINGS.replace(
        r#""state":"active""#,
        r#""state":"retired","retired_at_ms":5"#,
    );
    let (db, _) = started(&retired);
    let node = record(&db, "notes_graph").expect("the record");
    assert_eq!(
        node.properties["state"],
        crate::MetadataValue::String("retired".into())
    );
    assert_eq!(
        node.properties["retired_at_ms"],
        crate::MetadataValue::String("5".into())
    );
    assert!(!provisioned(&db, "notes_graph", &scope("@root", "a", None)));
}

#[test]
fn a_read_only_database_writes_no_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read-only-record.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.create_graph("notes_graph").unwrap();
    db.create_collection("notes_ev", spec()).unwrap();
    drop(db);
    let mut db = Database::open_read_only(&path).unwrap();
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Ok(()));
    assert!(record(&db, "notes_graph").is_none());
}

#[test]
fn a_read_only_replica_still_refuses_a_file_that_contradicts_its_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replica-record.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.create_graph("notes_graph").unwrap();
    db.create_collection("notes_ev", spec()).unwrap();
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Ok(()));
    drop(db);
    let mut db = Database::open_read_only(&path).unwrap();
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Ok(()));
    let longer = BINDINGS.replace("86400000", "86400001");
    assert_eq!(
        prepare(&mut db, &bindings(&longer)),
        Err("a memory binding changed its graph, evidence collection or receipt window")
    );
}

#[test]
fn rotation_refusals_use_fixed_messages_that_name_nothing() {
    let renamed = BINDINGS
        .replace("notes_graph", "zz_hidden_graph")
        .replace("notes_ev", "zz_hidden_ev")
        .replace(r#""binding":"notes""#, r#""binding":"zz_hidden""#);
    let mut db = database("zz_hidden_graph", "zz_hidden_ev");
    db.create_graph("other_graph").unwrap();
    db.create_graph("zz_other_graph").unwrap();
    db.create_collection("zz_other_ev", spec()).unwrap();
    let first = bindings(&renamed);
    assert_eq!(prepare(&mut db, &first), Ok(()));
    let cases = [
        (
            renamed.replace("86400000", "86400001"),
            "a memory binding changed its graph, evidence collection or receipt window",
        ),
        (
            renamed.replace(r#""zz_hidden_graph""#, r#""other_graph""#),
            "a memory binding changed its graph, evidence collection or receipt window",
        ),
        (
            renamed.replace(r#""zz_hidden_ev""#, "null"),
            "a memory binding changed its graph, evidence collection or receipt window",
        ),
        (
            renamed.replace(r#"[{"agent":"a"}]}]"#, r#"[{"agent":"c"}]}]"#),
            "a memory binding changed without a higher generation",
        ),
        (
            renamed.replace(r#""generation":1"#, r#""generation":2"#),
            "a memory binding changed its generation without another change",
        ),
        (
            renamed
                .replace(r#""generation":1"#, r#""generation":2"#)
                .replace(
                    r#""state":"active""#,
                    r#""state":"retired","retired_at_ms":1"#,
                ),
            "a memory binding retirement time precedes this start",
        ),
        (
            renamed.replace("zz_hidden", "zz_other"),
            "a memory binding was removed before it was retired and its receipt window ended",
        ),
    ];
    for (file, message) in cases {
        let refused = prepare(&mut db, &bindings(&file));
        assert_eq!(refused, Err(message), "{file}");
        for name in ["zz_hidden", "other_graph", "abcdefghij27", "@root"] {
            assert!(!message.contains(name), "{message} names {name}");
        }
    }
    assert_eq!(
        prepare(&mut db, &first),
        Ok(()),
        "the database is unchanged"
    );
}

#[test]
fn a_retired_binding_cannot_return_and_cannot_move_its_retirement() {
    let retired = BINDINGS.replace(
        r#""state":"active""#,
        r#""state":"retired","retired_at_ms":5"#,
    );
    let (mut db, _) = started(&retired);
    let again = BINDINGS.replace(r#""generation":1"#, r#""generation":2"#);
    assert_eq!(
        prepare(&mut db, &bindings(&again)),
        Err("a retired memory binding cannot become active again")
    );
    let moved = retired
        .replace(r#""generation":1"#, r#""generation":2"#)
        .replace(r#""retired_at_ms":5"#, r#""retired_at_ms":6"#);
    assert_eq!(
        prepare(&mut db, &bindings(&moved)),
        Err("a retired memory binding changed its retirement time")
    );
}

#[test]
fn a_removed_bindings_resources_cannot_be_reused_by_another_binding() {
    let retired = BINDINGS.replace(
        r#""state":"active""#,
        r#""state":"retired","retired_at_ms":5"#,
    );
    let (mut db, _) = started(&retired);
    let reuse = retired
        .replace(r#""binding":"notes""#, r#""binding":"fresh""#)
        .replace(
            r#""state":"retired","retired_at_ms":5"#,
            r#""state":"active""#,
        );
    let later = NOW + DAY + 1;
    assert_eq!(
        super::prepare(&mut db, &bindings(&reuse), later).map(|_| ()),
        Err("a removed memory binding's resources are used by another binding")
    );
}

#[test]
fn without_bindings_the_records_are_returned_and_nothing_is_written() {
    let (mut db, _) = started(BINDINGS);
    let sequence = db.sequence;
    let none = reserved::Bindings::default();
    // Far past any window: no rotation rule runs without a file.
    let orphans = super::prepare(&mut db, &none, NOW + DAY * 900).unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].binding, "notes");
    assert_eq!(orphans[0].graph, "notes_graph");
    assert_eq!(orphans[0].evidence.as_deref(), Some("notes_ev"));
    assert_eq!(db.sequence, sequence);
}

#[test]
fn a_read_only_replica_without_bindings_still_returns_the_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replica-orphans.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.create_graph("notes_graph").unwrap();
    db.create_collection("notes_ev", spec()).unwrap();
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Ok(()));
    drop(db);
    let mut db = Database::open_read_only(&path).unwrap();
    let orphans = super::prepare(&mut db, &reserved::Bindings::default(), NOW + DAY * 900).unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].binding, "notes");
    assert_eq!(orphans[0].graph, "notes_graph");
}

#[test]
fn a_record_that_cannot_be_written_refuses_startup_with_its_own_message() {
    use crate::property_graph::{GraphSchema, LabelDefinition, PropertyDefinition, PropertyType};
    let mut db = database("notes_graph", "notes_ev");
    let mut schema = GraphSchema::default();
    schema.labels.insert(
        "VliteMemoryBinding".into(),
        LabelDefinition::new("").property(
            "never_written",
            PropertyDefinition::new(PropertyType::String).required(),
        ),
    );
    db.graph_mut("notes_graph")
        .unwrap()
        .put_schema(&schema)
        .unwrap();
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Err(RECORD_FAILED));
    assert!(record(&db, "notes_graph").is_none());
}

#[test]
fn a_removed_entry_is_returned_as_an_orphan_after_its_window() {
    let retired = BINDINGS.replace(
        r#""state":"active""#,
        r#""state":"retired","retired_at_ms":5"#,
    );
    let (mut db, _) = started(&retired);
    db.create_graph("other_graph").unwrap();
    let other = BINDINGS
        .replace(r#""binding":"notes""#, r#""binding":"other""#)
        .replace("notes_graph", "other_graph")
        .replace("\"notes_ev\"", "null");
    let file = bindings(&other);
    assert_eq!(
        prepare(&mut db, &file),
        Err("a memory binding was removed before it was retired and its receipt window ended")
    );
    let orphans = super::prepare(&mut db, &file, NOW + DAY).unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].binding, "notes");
    assert!(record(&db, "notes_graph").is_some(), "the record is kept");
    assert!(
        record(&db, "other_graph").is_some(),
        "the new entry is recorded"
    );
}

#[test]
fn a_database_that_never_held_memory_starts_unchanged_without_bindings() {
    let none = reserved::Bindings::default();
    let mut db = database("notes_graph", "notes_ev");
    db.graph_mut("notes_graph")
        .unwrap()
        .write_batch(&[GraphOp::CreateNode {
            key: "ordinary".into(),
            labels: vec!["Person".into()],
            properties: Metadata::new(),
        }])
        .unwrap();
    let sequence = db.sequence;
    assert_eq!(super::prepare(&mut db, &none, NOW), Ok(Vec::new()));
    assert_eq!(db.sequence, sequence);
    let mut vector_only = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V4),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(super::prepare(&mut vector_only, &none, NOW), Ok(Vec::new()));
}

#[test]
fn a_corrupt_record_refuses_startup_even_without_bindings() {
    let (mut db, bindings) = started(BINDINGS);
    let node = record(&db, "notes_graph").unwrap();
    let mut properties = node.properties.clone();
    properties.insert("extra".into(), crate::MetadataValue::String("x".into()));
    db.graph_mut("notes_graph")
        .unwrap()
        .write_batch(&[GraphOp::UpsertNode {
            key: KEY.into(),
            labels: vec!["VliteMemoryBinding".into()],
            properties,
            expected: crate::Expected::Revision(node.revision),
        }])
        .unwrap();
    let invalid = "a memory binding record in the database is not valid";
    assert_eq!(prepare(&mut db, &bindings), Err(invalid));
    assert_eq!(
        super::prepare(&mut db, &reserved::Bindings::default(), NOW),
        Err(invalid)
    );
}

#[test]
fn a_forged_scope_marker_does_not_make_a_graph_adoptable() {
    let mut db = database("notes_graph", "notes_ev");
    db.graph_mut("notes_graph")
        .unwrap()
        .write_batch(&[
            GraphOp::CreateNode {
                key: "ordinary".into(),
                labels: vec!["Person".into()],
                properties: Metadata::new(),
            },
            GraphOp::CreateNode {
                key: "scope:v1:forged".into(),
                labels: vec!["VliteMemoryScope".into()],
                properties: Metadata::new(),
            },
        ])
        .unwrap();
    let sequence = db.sequence;
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Err(IN_USE));
    assert_eq!(db.sequence, sequence);
}

#[test]
fn a_graph_adopted_by_the_first_release_is_adopted_once() {
    let mut db = database("notes_graph", "notes_ev");
    MemoryMut::bind(&mut db, "notes_graph", Some("notes_ev"))
        .unwrap()
        .provision_scope(&scope("@root", "a", None))
        .unwrap();
    db.collection_mut("notes_ev")
        .unwrap()
        .put(1, &[1.0, 0.0], b"loaded before it was attached")
        .unwrap();
    assert!(record(&db, "notes_graph").is_none());
    assert_eq!(prepare(&mut db, &bindings(BINDINGS)), Ok(()));
    assert!(record(&db, "notes_graph").is_some());
}

#[tokio::test]
async fn startup_without_a_database_is_only_an_error_when_bindings_are_configured() {
    let service = Service::new(database("notes_graph", "notes_ev"));
    service.close().await.unwrap();
    assert_eq!(startup(&service).unwrap(), Vec::new());
    let configured =
        Service::new(database("notes_graph", "notes_ev")).with_bindings(bindings(BINDINGS));
    configured.close().await.unwrap();
    assert!(startup(&configured).is_err());
}
