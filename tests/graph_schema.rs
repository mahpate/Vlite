//! TEST-010 (EPIC-008, ITEM-031..ITEM-033): declared graph schemas.
//!
//! Declared label and relationship rules are enforced at commit and a
//! rejected write leaves the file byte-identical (FM-013); schemas replay on
//! reopen and survive compaction, backup and reencryption; the observed
//! summary marks declared/observed items within the PD-011 budget; declared
//! property indexes return exactly what a scan returns; `ensure_edge` is
//! limited to `UniqueTriple` types; files without a schema behave as before.

mod graph_support;

use std::fs;

use graph_support::{KINDS, Kind, live_bytes, open};
use vlite::maintenance;
use vlite::property_graph::{
    EdgeRef, FindNodes, GraphLimits, GraphOp, GraphOpOutcome, GraphQuery, GraphSchema, IndexChoice,
    LabelDefinition, Multiplicity, NodePatch, NodeView, PropertyDefinition, PropertyIndexKind,
    PropertyType, ReadOptions, RelationshipDefinition, SchemaOrigin,
};
use vlite::{Condition, Database, Error, Expected, Metadata, MetadataValue};

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

fn int(value: i64) -> MetadataValue {
    MetadataValue::Integer(value)
}

fn float(value: f64) -> MetadataValue {
    MetadataValue::Float(value)
}

fn props(fields: &[(&str, MetadataValue)]) -> Metadata {
    fields
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

/// Person: `name` required unique String, `age` Integer with a range index;
/// Doc: `title` String with an exact index; KNOWS: Person → Person, unique
/// triple, `since` Integer; WROTE: Person → Doc, `role` required String.
fn schema() -> GraphSchema {
    GraphSchema::default()
        .label(
            "Person",
            LabelDefinition::new("A human being.")
                .property(
                    "name",
                    PropertyDefinition::new(PropertyType::String)
                        .required()
                        .unique(),
                )
                .property(
                    "age",
                    PropertyDefinition::new(PropertyType::Integer).index(PropertyIndexKind::Range),
                ),
        )
        .label(
            "Doc",
            LabelDefinition::new("A document.").property(
                "title",
                PropertyDefinition::new(PropertyType::String).index(PropertyIndexKind::Exact),
            ),
        )
        .relationship(
            "KNOWS",
            RelationshipDefinition::new("Acquaintance.")
                .endpoint("Person", "Person")
                .multiplicity(Multiplicity::UniqueTriple)
                .property("since", PropertyDefinition::new(PropertyType::Integer)),
        )
        .relationship(
            "WROTE",
            RelationshipDefinition::new("")
                .endpoint("Person", "Doc")
                .property(
                    "role",
                    PropertyDefinition::new(PropertyType::String).required(),
                ),
        )
}

fn schema_db(kind: Kind, dir: &std::path::Path) -> (std::path::PathBuf, Database) {
    let path = dir.join(format!("schema-{kind:?}.vlite"));
    let mut db = Database::create_with_graph(&path, 2, kind.options()).unwrap();
    db.create_graph("g").unwrap();
    let mut graph = db.graph_mut("g").unwrap();
    graph.put_schema(&schema()).unwrap();
    graph
        .create_node(
            "ada",
            &["Person"],
            &props(&[("name", text("Ada")), ("age", int(36))]),
        )
        .unwrap();
    graph
        .create_node("bob", &["Person"], &props(&[("name", text("Bob"))]))
        .unwrap();
    graph
        .create_node("doc", &["Doc"], &props(&[("title", text("notes"))]))
        .unwrap();
    graph
        .create_node("misc", &["Other"], &props(&[("free", int(1))]))
        .unwrap();
    graph
        .create_edge(None, "ada", "KNOWS", "bob", &props(&[("since", int(2020))]))
        .unwrap();
    graph
        .create_edge(
            None,
            "ada",
            "WROTE",
            "doc",
            &props(&[("role", text("author"))]),
        )
        .unwrap();
    (path, db)
}

#[track_caller]
fn assert_violation(
    result: vlite::Result<impl std::fmt::Debug>,
    rule: &str,
    element: &str,
    property: Option<&str>,
) {
    match result {
        Err(Error::GraphSchemaViolation {
            rule: r,
            element: e,
            property: p,
        }) => {
            assert_eq!(
                (r, e.as_str(), p.as_deref()),
                (rule, element, property),
                "violation fields"
            );
        }
        other => panic!("expected a {rule} schema violation, got {other:?}"),
    }
}

#[test]
fn declared_rules_are_enforced_and_rejections_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = schema_db(Kind::Plain, dir.path());
    let before = live_bytes(&db);
    let mut graph = db.graph_mut("g").unwrap();

    assert_violation(
        graph.create_node("x", &["Person"], &Metadata::new()),
        "required",
        "Person",
        Some("name"),
    );
    assert_violation(
        graph.create_node("x", &["Person"], &props(&[("name", int(1))])),
        "type",
        "Person",
        Some("name"),
    );
    assert_violation(
        graph.create_node(
            "x",
            &["Person"],
            &props(&[("name", text("Ada")), ("age", int(1))]),
        ),
        "unique",
        "Person",
        Some("name"),
    );
    assert_violation(
        graph.upsert_node(
            "bob",
            &["Person"],
            &props(&[("name", text("Bob")), ("age", text("old"))]),
            Expected::Any,
        ),
        "type",
        "Person",
        Some("age"),
    );
    assert_violation(
        graph.patch_node(
            "bob",
            &NodePatch {
                unset: vec!["name".into()],
                ..NodePatch::default()
            },
            Expected::Any,
        ),
        "required",
        "Person",
        Some("name"),
    );
    assert_violation(
        graph.patch_node(
            "misc",
            &NodePatch {
                add_labels: vec!["Person".into()],
                ..NodePatch::default()
            },
            Expected::Any,
        ),
        "required",
        "Person",
        Some("name"),
    );
    // Removing a label breaks the declared endpoints of an incident edge.
    assert_violation(
        graph.patch_node(
            "bob",
            &NodePatch {
                remove_labels: vec!["Person".into()],
                ..NodePatch::default()
            },
            Expected::Any,
        ),
        "endpoints",
        "KNOWS",
        None,
    );
    assert_violation(
        graph.create_edge(None, "ada", "KNOWS", "doc", &Metadata::new()),
        "endpoints",
        "KNOWS",
        None,
    );
    assert_violation(
        graph.create_edge(None, "ada", "KNOWS", "bob", &Metadata::new()),
        "unique_triple",
        "KNOWS",
        None,
    );
    assert_violation(
        graph.create_edge(
            None,
            "bob",
            "KNOWS",
            "ada",
            &props(&[("since", text("2020"))]),
        ),
        "type",
        "KNOWS",
        Some("since"),
    );
    assert_violation(
        graph.create_edge(None, "bob", "WROTE", "doc", &Metadata::new()),
        "required",
        "WROTE",
        Some("role"),
    );
    assert_violation(
        graph.patch_edge(
            EdgeRef::Id(0),
            &props(&[("since", float(1.0))]),
            &[],
            Expected::Any,
        ),
        "type",
        "KNOWS",
        Some("since"),
    );
    assert_violation(
        graph.patch_edge(EdgeRef::Id(1), &Metadata::new(), &["role"], Expected::Any),
        "required",
        "WROTE",
        Some("role"),
    );
    // A batch fails as a whole when a later operation violates the schema.
    assert_violation(
        graph.write_batch(&[
            GraphOp::CreateNode {
                key: "carol".into(),
                labels: vec!["Person".into()],
                properties: props(&[("name", text("Carol"))]),
            },
            GraphOp::CreateNode {
                key: "dave".into(),
                labels: vec!["Person".into()],
                properties: props(&[("name", text("Carol"))]),
            },
        ]),
        "unique",
        "Person",
        Some("name"),
    );
    // A new schema must hold for the existing data.
    let stricter = schema().label(
        "Person",
        LabelDefinition::new("A human being.")
            .property(
                "name",
                PropertyDefinition::new(PropertyType::String)
                    .required()
                    .unique(),
            )
            .property(
                "age",
                PropertyDefinition::new(PropertyType::Integer).required(),
            ),
    );
    assert_violation(
        graph.put_schema(&stricter),
        "required",
        "Person",
        Some("age"),
    );
    // Invalid definitions are input errors.
    let invalid = GraphSchema::default().label(
        "Person",
        LabelDefinition::new("").property(
            "name",
            PropertyDefinition::new(PropertyType::String).index(PropertyIndexKind::Range),
        ),
    );
    assert!(matches!(
        graph.put_schema(&invalid),
        Err(Error::InvalidQuery(_))
    ));
    let long = GraphSchema::default().label("Person", LabelDefinition::new("x".repeat(1025)));
    assert!(matches!(
        graph.put_schema(&long),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(graph.info().node_count, 4);
    assert_eq!(live_bytes(&db), before, "rejected writes changed the file");

    // Undeclared labels, types and properties stay open-world, and declared
    // rules accept conforming writes.
    let mut graph = db.graph_mut("g").unwrap();
    graph
        .create_node(
            "carol",
            &["Person", "Extra"],
            &props(&[("name", text("Carol")), ("note", float(0.5))]),
        )
        .unwrap();
    graph
        .create_edge(None, "carol", "LIKES", "doc", &Metadata::new())
        .unwrap();
    graph
        .create_edge(None, "carol", "KNOWS", "ada", &Metadata::new())
        .unwrap();
    graph.put_schema(&GraphSchema::default()).unwrap();
    // With the schema cleared, nothing is enforced any more.
    graph
        .create_node("anon", &["Person"], &Metadata::new())
        .unwrap();
    graph
        .create_edge(None, "carol", "KNOWS", "ada", &Metadata::new())
        .unwrap();
}

#[test]
fn schemas_replay_on_reopen_and_survive_maintenance() {
    let dir = tempfile::tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let (path, db) = schema_db(kind, dir.path());
        let expected_nodes = all_nodes(&db, &FindNodes::default());
        db.close().unwrap();

        let check = |path: &std::path::Path, key: Option<&vlite::EncryptionKey>| {
            let mut db = open(path, key);
            let graph = db.graph("g").unwrap();
            assert_eq!(graph.schema(), schema(), "{kind:?} {path:?}");
            let stripped: Vec<_> = all_nodes(&db, &FindNodes::default())
                .into_iter()
                .map(|mut node| {
                    node.revision = 0;
                    node
                })
                .collect();
            let expected: Vec<_> = expected_nodes
                .iter()
                .cloned()
                .map(|mut node| {
                    node.revision = 0;
                    node
                })
                .collect();
            assert_eq!(stripped, expected);
            // The replayed schema is enforced and its indexes are rebuilt.
            assert_violation(
                db.graph_mut("g").unwrap().create_node(
                    "x",
                    &["Person"],
                    &props(&[("name", text("Bob"))]),
                ),
                "unique",
                "Person",
                Some("name"),
            );
            let read = db
                .graph("g")
                .unwrap()
                .find_nodes(
                    &FindNodes {
                        label: Some("Doc".into()),
                        conditions: vec![Condition::Eq {
                            field: "title".into(),
                            value: text("notes"),
                        }],
                        ..FindNodes::default()
                    },
                    &ReadOptions::default(),
                )
                .unwrap();
            assert_eq!(read.output.len(), 1);
            assert_eq!(read.stats.indexes, vec![IndexChoice::PropertyExactIndex]);
        };
        check(&path, key.as_ref());

        let compacted = dir.path().join(format!("compacted-{kind:?}.vlite"));
        match &key {
            Some(key) => maintenance::compact_with_key(&path, &compacted, key),
            None => maintenance::compact_at(&path, &compacted, 5_000),
        }
        .unwrap();
        check(&compacted, key.as_ref());

        let backup = dir.path().join(format!("backup-{kind:?}.vlite"));
        match &key {
            Some(key) => maintenance::backup_with_key(&path, &backup, key),
            None => maintenance::backup(&path, &backup),
        }
        .unwrap();
        assert_eq!(fs::read(&backup).unwrap(), fs::read(&path).unwrap());
        check(&backup, key.as_ref());

        let reencrypted = dir.path().join(format!("reencrypted-{kind:?}.vlite"));
        let target = graph_support::other_key();
        maintenance::reencrypt(&path, key.as_ref(), &reencrypted, Some(&target)).unwrap();
        check(&reencrypted, Some(&target));
    }
}

#[test]
fn files_without_a_schema_behave_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = graph_support::graph_file(dir.path(), "plain", Kind::Plain);
    let mut db = open(&path, None);
    for info in db.list_graphs().unwrap() {
        assert_eq!(
            db.graph(&info.name).unwrap().schema(),
            GraphSchema::default()
        );
    }
    let read = db
        .graph("alpha")
        .unwrap()
        .find_nodes(
            &FindNodes {
                label: Some("Person".into()),
                conditions: vec![Condition::Eq {
                    field: "name".into(),
                    value: text("Ada"),
                }],
                ..FindNodes::default()
            },
            &ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(read.output.len(), 1);
    assert_eq!(read.stats.indexes, vec![IndexChoice::LabelIndexScan]);
    let before = live_bytes(&db);
    // ensure_edge needs a declared UniqueTriple relationship type.
    assert!(matches!(
        db.graph_mut("alpha")
            .unwrap()
            .ensure_edge("ada", "KNOWS", "bob", &Metadata::new()),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(live_bytes(&db), before);
    let observed = db.graph("alpha").unwrap().observed_schema(None).unwrap();
    assert!(observed.complete);
    assert!(
        observed
            .labels
            .iter()
            .all(|label| label.origin == SchemaOrigin::Observed)
    );
}

#[test]
fn ensure_edge_creates_returns_existing_and_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = schema_db(Kind::Plain, dir.path());
    let mut graph = db.graph_mut("g").unwrap();
    graph
        .create_node("cy", &["Person"], &props(&[("name", text("Cy"))]))
        .unwrap();

    let created = graph
        .ensure_edge("bob", "KNOWS", "cy", &props(&[("since", int(1))]))
        .unwrap();
    assert!(created.created);
    let again = graph
        .ensure_edge("bob", "KNOWS", "cy", &props(&[("since", int(1))]))
        .unwrap();
    assert_eq!((again.id, again.created), (created.id, false));
    assert_eq!(again.revision, created.revision);
    // The existing ada→bob edge from setup is found as well.
    let existing = graph
        .ensure_edge("ada", "KNOWS", "bob", &props(&[("since", int(2020))]))
        .unwrap();
    assert_eq!((existing.id, existing.created), (0, false));
    let before = live_bytes(&db);
    let mut graph = db.graph_mut("g").unwrap();
    assert!(matches!(
        graph.ensure_edge("bob", "KNOWS", "cy", &props(&[("since", int(2))])),
        Err(Error::GraphKeyConflict)
    ));
    assert!(matches!(
        graph.ensure_edge("ada", "WROTE", "doc", &props(&[("role", text("x"))])),
        Err(Error::InvalidQuery(_))
    ));
    assert!(matches!(
        graph.ensure_edge("ada", "UNDECLARED", "doc", &Metadata::new()),
        Err(Error::InvalidQuery(_))
    ));
    // Declared rules still apply to created edges.
    assert_violation(
        graph.ensure_edge("ada", "KNOWS", "doc", &Metadata::new()),
        "endpoints",
        "KNOWS",
        None,
    );
    // Idempotent retries inside one batch observe the earlier creation.
    let result = graph
        .write_batch(&[
            GraphOp::EnsureEdge {
                source: "cy".into(),
                relationship: "KNOWS".into(),
                destination: "ada".into(),
                properties: Metadata::new(),
            },
            GraphOp::EnsureEdge {
                source: "cy".into(),
                relationship: "KNOWS".into(),
                destination: "ada".into(),
                properties: Metadata::new(),
            },
        ])
        .unwrap();
    let [GraphOpOutcome::Edge(first), GraphOpOutcome::Edge(second)] = result.outcomes[..] else {
        panic!("edge outcomes expected: {:?}", result.outcomes);
    };
    assert!(first.created && !second.created && first.id == second.id);
    assert_ne!(live_bytes(&db), before);
    // A batch of only idempotent retries writes nothing.
    let before = live_bytes(&db);
    let result = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&[GraphOp::EnsureEdge {
            source: "cy".into(),
            relationship: "KNOWS".into(),
            destination: "ada".into(),
            properties: Metadata::new(),
        }])
        .unwrap();
    assert_eq!(result.revision, None);
    assert_eq!(live_bytes(&db), before);
}

#[test]
fn observed_schema_marks_origins_and_respects_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = schema_db(Kind::Plain, dir.path());
    db.graph_mut("g")
        .unwrap()
        .put_schema(&schema().label("Ghost", LabelDefinition::new("Never used.")))
        .unwrap();
    let graph = db.graph("g").unwrap();
    let observed = graph.observed_schema(None).unwrap();
    assert!(observed.complete);
    assert_eq!(observed.entities_scanned, 6);
    let labels: Vec<_> = observed
        .labels
        .iter()
        .map(|label| (label.name.as_str(), label.origin, label.count))
        .collect();
    assert_eq!(
        labels,
        vec![
            ("Doc", SchemaOrigin::Both, 1),
            ("Ghost", SchemaOrigin::Declared, 0),
            ("Other", SchemaOrigin::Observed, 1),
            ("Person", SchemaOrigin::Both, 2),
        ]
    );
    let person = &observed.labels[3];
    let fields: Vec<_> = person
        .properties
        .iter()
        .map(|p| (p.name.as_str(), p.origin, p.count, p.types.clone()))
        .collect();
    assert_eq!(
        fields,
        vec![
            ("age", SchemaOrigin::Both, 1, vec![PropertyType::Integer]),
            ("name", SchemaOrigin::Both, 2, vec![PropertyType::String]),
        ]
    );
    let other = &observed.labels[2];
    assert_eq!(other.properties[0].origin, SchemaOrigin::Observed);
    let relationships: Vec<_> = observed
        .relationships
        .iter()
        .map(|r| (r.name.as_str(), r.origin, r.count))
        .collect();
    assert_eq!(
        relationships,
        vec![
            ("KNOWS", SchemaOrigin::Both, 1),
            ("WROTE", SchemaOrigin::Both, 1),
        ]
    );
    let knows = &observed.relationships[0];
    assert_eq!(knows.endpoints.len(), 1);
    assert_eq!(
        (
            knows.endpoints[0].source.as_str(),
            knows.endpoints[0].destination.as_str(),
            knows.endpoints[0].origin,
            knows.endpoints[0].count
        ),
        ("Person", "Person", SchemaOrigin::Both, 1)
    );

    // The budget bounds the entities scanned; the summary reports truncation.
    let truncated = graph.observed_schema(Some(2)).unwrap();
    assert!(!truncated.complete);
    assert_eq!(truncated.entities_scanned, 2);
    assert_eq!(
        truncated
            .labels
            .iter()
            .find(|label| label.name == "Person")
            .map(|label| label.count),
        Some(2)
    );
    assert!(
        truncated
            .relationships
            .iter()
            .all(|r| r.origin == SchemaOrigin::Declared && r.count == 0)
    );
    assert!(matches!(
        graph.observed_schema(Some(1_000_001)),
        Err(Error::GraphLimitExceedsCeiling { .. })
    ));
    assert!(matches!(
        graph.observed_schema(Some(0)),
        Err(Error::InvalidQuery(_))
    ));
}

fn all_nodes(db: &Database, request: &FindNodes) -> Vec<NodeView> {
    pages(db, request, &ReadOptions::default()).0
}

/// Every page of `request` concatenated, with the index choices reported.
fn pages(
    db: &Database,
    request: &FindNodes,
    options: &ReadOptions<'_>,
) -> (Vec<NodeView>, Vec<IndexChoice>) {
    let graph = db.graph("g").unwrap();
    let mut request = request.clone();
    let mut nodes = Vec::new();
    loop {
        let page = graph.find_nodes(&request, options).unwrap();
        nodes.extend(page.output);
        match page.cursor {
            Some(cursor) => request.cursor = Some(cursor),
            None => return (nodes, page.stats.indexes),
        }
    }
}

fn item(index: i64) -> (String, Vec<&'static str>, Metadata) {
    let labels = match index % 5 {
        0 => vec!["Item"],
        1 => vec!["Item", "Sale"],
        2 => vec!["Other"],
        3 => vec!["Item"],
        _ => vec![],
    };
    let mut properties = Metadata::new();
    if index % 7 != 0 {
        properties.insert("sku".into(), text(&format!("sku-{}", index % 11)));
    }
    if index % 6 != 1 {
        properties.insert("price".into(), int((index * 37) % 101 - 20));
    }
    if index % 4 != 2 {
        let weight = ((index * 13) % 17) as f64 / 4.0 - 1.0;
        let weight = if index % 9 == 0 { -0.0 } else { weight };
        properties.insert("weight".into(), float(weight));
    }
    (format!("n{index}"), labels, properties)
}

fn indexed_schema() -> GraphSchema {
    GraphSchema::default().label(
        "Item",
        LabelDefinition::new("Indexed items.")
            .property(
                "sku",
                PropertyDefinition::new(PropertyType::String).index(PropertyIndexKind::Exact),
            )
            .property(
                "price",
                PropertyDefinition::new(PropertyType::Integer).index(PropertyIndexKind::Range),
            )
            .property(
                "weight",
                PropertyDefinition::new(PropertyType::Float).index(PropertyIndexKind::Range),
            ),
    )
}

fn twin(dir: &std::path::Path, name: &str, declared: bool) -> Database {
    let path = dir.join(format!("{name}.vlite"));
    let mut db = Database::create_with_graph(&path, 2, Kind::Plain.options()).unwrap();
    db.create_graph("g").unwrap();
    let mut graph = db.graph_mut("g").unwrap();
    if declared {
        graph.put_schema(&indexed_schema()).unwrap();
    }
    let ops: Vec<GraphOp> = (0..300)
        .map(|index| {
            let (key, labels, properties) = item(index);
            GraphOp::CreateNode {
                key,
                labels: labels.into_iter().map(str::to_owned).collect(),
                properties,
            }
        })
        .collect();
    graph.write_batch(&ops).unwrap();
    // Patches and deletes keep the postings exact.
    for index in (0..300).step_by(13) {
        graph
            .patch_node(
                &format!("n{index}"),
                &NodePatch {
                    set: props(&[("price", int(42))]),
                    unset: vec!["sku".into()],
                    ..NodePatch::default()
                },
                Expected::Any,
            )
            .unwrap();
    }
    for index in (5..300).step_by(29) {
        graph
            .patch_node(
                &format!("n{index}"),
                &NodePatch {
                    remove_labels: vec!["Item".into()],
                    ..NodePatch::default()
                },
                Expected::Any,
            )
            .unwrap();
    }
    for index in (3..300).step_by(31) {
        graph
            .delete_node(&format!("n{index}"), Expected::Any)
            .unwrap();
    }
    db
}

fn queries() -> Vec<(FindNodes, Option<IndexChoice>)> {
    let find = |conditions: Vec<Condition>| FindNodes {
        label: Some("Item".into()),
        conditions,
        ..FindNodes::default()
    };
    let eq = |field: &str, value: MetadataValue| Condition::Eq {
        field: field.into(),
        value,
    };
    let range =
        |field: &str, min: Option<MetadataValue>, max: Option<MetadataValue>| Condition::Range {
            field: field.into(),
            min,
            max,
        };
    let exact = Some(IndexChoice::PropertyExactIndex);
    let ranged = Some(IndexChoice::PropertyRangeIndex);
    vec![
        (find(vec![eq("sku", text("sku-3"))]), exact),
        (find(vec![eq("sku", text("missing"))]), exact),
        (find(vec![eq("sku", int(3))]), exact),
        (
            find(vec![
                eq("sku", text("sku-4")),
                range("price", Some(int(0)), None),
            ]),
            exact,
        ),
        (
            find(vec![range("price", Some(int(10)), Some(int(50)))]),
            ranged,
        ),
        (find(vec![range("price", None, Some(int(0)))]), ranged),
        (
            find(vec![range("price", Some(int(42)), Some(int(42)))]),
            ranged,
        ),
        (find(vec![range("price", Some(float(1.0)), None)]), ranged),
        (
            find(vec![range("weight", Some(float(-0.0)), Some(float(1.5)))]),
            ranged,
        ),
        (
            find(vec![range("weight", Some(float(0.0)), Some(float(0.0)))]),
            ranged,
        ),
        (find(vec![range("weight", None, Some(int(2)))]), ranged),
        (find(vec![eq("weight", float(0.0))]), None),
        (find(vec![eq("price", int(42))]), None),
        (
            FindNodes {
                label: Some("Sale".into()),
                conditions: vec![eq("sku", text("sku-3"))],
                ..FindNodes::default()
            },
            None,
        ),
        (
            FindNodes {
                label: None,
                conditions: vec![eq("sku", text("sku-3"))],
                ..FindNodes::default()
            },
            None,
        ),
    ]
}

#[test]
fn declared_indexes_return_exactly_what_scans_return() {
    let dir = tempfile::tempdir().unwrap();
    let indexed = twin(dir.path(), "indexed", true);
    let scanned = twin(dir.path(), "scanned", false);
    let check = |indexed: &Database, scanned: &Database| {
        for (request, index) in queries() {
            for limit in [None, Some(1), Some(7)] {
                let request = FindNodes {
                    limit,
                    ..request.clone()
                };
                let (expected, scan_indexes) = pages(scanned, &request, &ReadOptions::default());
                let (actual, indexes) = pages(indexed, &request, &ReadOptions::default());
                let strip = |nodes: Vec<NodeView>| -> Vec<(u64, String)> {
                    nodes.into_iter().map(|node| (node.id, node.key)).collect()
                };
                assert_eq!(strip(actual), strip(expected), "{request:?}");
                let default = if request.label.is_some() {
                    IndexChoice::LabelIndexScan
                } else {
                    IndexChoice::FullNodeScan
                };
                assert_eq!(scan_indexes, vec![default]);
                assert_eq!(indexes, vec![index.unwrap_or(default)], "{request:?}");
            }
            // `explain` reports the same choice without executing.
            let explanation = indexed
                .graph("g")
                .unwrap()
                .explain(
                    &GraphQuery::FindNodes(request.clone()),
                    &ReadOptions::default(),
                )
                .unwrap();
            assert_eq!(explanation.indexes.len(), 1);
        }
    };
    check(&indexed, &scanned);
    let matching = all_nodes(
        &indexed,
        &FindNodes {
            label: Some("Item".into()),
            conditions: vec![Condition::Range {
                field: "price".into(),
                min: Some(int(10)),
                max: Some(int(50)),
            }],
            ..FindNodes::default()
        },
    );
    assert!(matching.len() > 10, "the workload must exercise paging");

    // An indexed page scans only candidates, so a scan budget that truncates
    // the scan still completes through the index.
    let limits = GraphLimits {
        max_edges_examined: 40,
        ..GraphLimits::default()
    };
    let options = ReadOptions {
        limits,
        ..ReadOptions::default()
    };
    let request = FindNodes {
        label: Some("Item".into()),
        conditions: vec![Condition::Eq {
            field: "sku".into(),
            value: text("sku-5"),
        }],
        ..FindNodes::default()
    };
    let (nodes, _) = pages(&indexed, &request, &options);
    let (expected, _) = pages(&scanned, &request, &options);
    // The schema write adds one revision to the indexed twin.
    let content = |nodes: Vec<NodeView>| -> Vec<_> {
        nodes
            .into_iter()
            .map(|node| (node.id, node.key, node.labels, node.properties))
            .collect()
    };
    assert_eq!(content(nodes), content(expected));

    // Indexes are derived: they are rebuilt on reopen and follow a schema change.
    let indexed_path = dir.path().join("indexed.vlite");
    let scanned_path = dir.path().join("scanned.vlite");
    indexed.close().unwrap();
    scanned.close().unwrap();
    let mut indexed = open(&indexed_path, None);
    let scanned = open(&scanned_path, None);
    check(&indexed, &scanned);
    indexed
        .graph_mut("g")
        .unwrap()
        .put_schema(&GraphSchema::default())
        .unwrap();
    let (_, indexes) = pages(&indexed, &queries()[0].0, &ReadOptions::default());
    assert_eq!(indexes, vec![IndexChoice::LabelIndexScan]);
}

#[test]
fn schema_writes_join_batches_and_report_an_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("batch.vlite");
    let mut db = Database::create_with_graph(&path, 2, Kind::Plain.options()).unwrap();
    db.create_graph("g").unwrap();
    let result = db
        .graph_mut("g")
        .unwrap()
        .write_batch(&[
            GraphOp::PutSchema { schema: schema() },
            GraphOp::CreateNode {
                key: "ada".into(),
                labels: vec!["Person".into()],
                properties: props(&[("name", text("Ada"))]),
            },
        ])
        .unwrap();
    assert!(matches!(result.outcomes[0], GraphOpOutcome::Schema));
    assert!(result.revision.is_some());
    assert_eq!(db.graph("g").unwrap().schema(), schema());
    // Schemas belong to one graph.
    db.create_graph("h").unwrap();
    assert_eq!(db.graph("h").unwrap().schema(), GraphSchema::default());
    db.graph_mut("h")
        .unwrap()
        .create_node("x", &["Person"], &Metadata::new())
        .unwrap();
}
