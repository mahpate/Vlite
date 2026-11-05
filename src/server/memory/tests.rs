use super::*;
use crate::memory::ReceiptOperation;
use crate::memory::binding::parse;
use crate::{CollectionSpec, CreateOptions, FormatVersion, Metadata, property_graph::GraphOp};

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
