//! EPIC-015 ITEM-051 graph query text integration test (REQ-070, REQ-071,
//! AC-023), built only with the `graph` feature.
//!
//! Every accepted text must parse to exactly the typed `GraphQuery` it stands
//! for, and `GraphRef::query_text` must return exactly what
//! `GraphRef::query` returns for that typed query on seeded random graphs.
//! Rejected texts fail with typed errors and byte offsets; random and mutated
//! inputs never panic. The vector query language (`SEARCH ...`) and the graph
//! text keep separate entry points and reject each other's texts.

use std::path::Path;

use tempfile::tempdir;
use vlite::property_graph::{
    Direction, EdgeRef, FindNodes, GraphLimits, GraphOp, GraphOutput, GraphQuery,
    GraphQueryErrorKind, GraphTextError, Neighbors, NodeFilter, NodeSelector, PropertyProjection,
    ReadOptions, ShortestPath, Traverse, parse_graph_query,
};
use vlite::{Condition, CreateOptions, Database, Error, Metadata, MetadataValue};

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

const LABELS: [&str; 2] = ["A", "B"];
const TYPES: [&str; 3] = ["R", "S", "T"];

fn no_params() -> Metadata {
    Metadata::new()
}

fn params(entries: &[(&str, MetadataValue)]) -> Metadata {
    entries
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

fn string(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

fn eq(field: &str, value: MetadataValue) -> Condition {
    Condition::Eq {
        field: field.into(),
        value,
    }
}

fn range(field: &str, min: Option<i64>, max: Option<i64>) -> Condition {
    Condition::Range {
        field: field.into(),
        min: min.map(MetadataValue::Integer),
        max: max.map(MetadataValue::Integer),
    }
}

fn parsed(text: &str, params: &Metadata) -> vlite::property_graph::GraphTextQuery {
    parse_graph_query(text, params).unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn rejected(text: &str, params: &Metadata) -> (GraphQueryErrorKind, usize) {
    let error = parse_graph_query(text, params).expect_err(text);
    assert!(error.position <= text.len(), "{text}: {error}");
    assert!(error.to_string().contains(&error.message));
    (error.kind, error.position)
}

#[test]
fn texts_parse_to_the_typed_queries_they_stand_for() {
    let none = no_params();
    let query = parsed(r#"MATCH (n KEY "a") RETURN n"#, &none);
    assert_eq!(query.query, GraphQuery::GetNode(NodeSelector::key("a")));
    assert_eq!(query.projection, PropertyProjection::All);
    assert_eq!((query.max_hops, query.max_output_nodes), (None, None));

    let query = parsed(
        "MATCH (n ID $id) RETURN n",
        &params(&[("id", MetadataValue::Integer(7))]),
    );
    assert_eq!(query.query, GraphQuery::GetNode(NodeSelector::Id(7)));

    let query = parsed(r#"MATCH ()-[r KEY "ab"]-() RETURN r"#, &none);
    assert_eq!(query.query, GraphQuery::GetEdge(EdgeRef::Key("ab".into())));
    let query = parsed("MATCH ()-[r ID 3]-() RETURN r", &none);
    assert_eq!(query.query, GraphQuery::GetEdge(EdgeRef::Id(3)));

    let query = parsed(
        r#"match (n:A {s: "x"}) where n.w >= $lo and n.w <= 3 return n.w, n.s limit 5"#,
        &params(&[("lo", MetadataValue::Integer(1))]),
    );
    assert_eq!(
        query.query,
        GraphQuery::FindNodes(FindNodes {
            label: Some("A".into()),
            conditions: vec![
                eq("s", string("x")),
                range("w", Some(1), None),
                range("w", None, Some(3)),
            ],
            limit: Some(5),
            cursor: None,
        })
    );
    assert_eq!(
        query.projection,
        PropertyProjection::Only(vec!["s".into(), "w".into()])
    );

    let query = parsed(
        r#"MATCH (a KEY $k)<-[r:R|S {w: 1}]-(b) WHERE r.s = "y" RETURN r, b LIMIT 4"#,
        &params(&[("k", string("n1"))]),
    );
    assert_eq!(
        query.query,
        GraphQuery::Neighbors(Neighbors {
            node: NodeSelector::key("n1"),
            direction: Direction::In,
            types: Some(vec!["R".into(), "S".into()]),
            edge_conditions: vec![eq("w", MetadataValue::Integer(1)), eq("s", string("y"))],
            limit: Some(4),
            cursor: None,
        })
    );
    let query = parsed(r#"MATCH (a KEY "n1")--(b) RETURN b"#, &none);
    assert_eq!(
        query.query,
        GraphQuery::Neighbors(Neighbors {
            direction: Direction::Both,
            ..Neighbors::new(NodeSelector::key("n1"))
        })
    );

    let query = parsed(
        r#"MATCH p = (a KEY "n1")-[r:T*1..3 {w: 2}]->(b:A:B {s: "x"}) WHERE b.w >= 1 AND r.s = "y" RETURN b, p LIMIT 9"#,
        &none,
    );
    assert_eq!(
        query.query,
        GraphQuery::Traverse(Traverse {
            types: Some(vec!["T".into()]),
            return_filter: NodeFilter {
                labels: vec!["A".into(), "B".into()],
                conditions: vec![eq("s", string("x")), range("w", Some(1), None)],
            },
            edge_filter: vec![eq("w", MetadataValue::Integer(2)), eq("s", string("y"))],
            include_paths: true,
            ..Traverse::new(vec![NodeSelector::key("n1")])
        })
    );
    assert_eq!((query.max_hops, query.max_output_nodes), (Some(3), Some(9)));
    assert_eq!(
        query.limits(GraphLimits::default()),
        GraphLimits {
            max_hops: 3,
            max_output_nodes: 9,
            ..GraphLimits::default()
        }
    );

    let query = parsed(
        r#"MATCH p = SHORTEST PATH (a KEY "n1")-[*..4]-(b ID 9) RETURN p"#,
        &none,
    );
    assert_eq!(
        query.query,
        GraphQuery::ShortestPath(ShortestPath {
            direction: Direction::Both,
            ..ShortestPath::new(NodeSelector::key("n1"), NodeSelector::Id(9))
        })
    );
    assert_eq!(query.max_hops, Some(4));
}

#[test]
fn rejected_texts_are_typed_with_byte_offsets() {
    use GraphQueryErrorKind::{Limit, Parameter, Syntax, Unsupported};
    let none = no_params();
    let cases: &[(&str, GraphQueryErrorKind, usize)] = &[
        ("", Syntax, 0),
        ("SEARCH docs NEAR $q LIMIT 3", Unsupported, 0),
        ("CREATE (n:A) RETURN n", Unsupported, 0),
        ("MATCH (n:A) DELETE n", Unsupported, 12),
        ("MATCH (n:A) RETURN n;", Unsupported, 20),
        ("MATCH (n:A) WHERE n.w > 1 RETURN n", Unsupported, 22),
        (
            "MATCH (n:A) WHERE n.w = 1 OR n.w = 2 RETURN n",
            Unsupported,
            26,
        ),
        ("MATCH (n:A) RETURN n ORDER BY n.w", Unsupported, 21),
        ("MATCH (n:A:B) RETURN n", Unsupported, 10),
        ("MATCH (a KEY \"a\")-[*1..5]->(b) RETURN b", Limit, 23),
        ("MATCH (a KEY \"a\")-[*2..3]->(b) RETURN b", Unsupported, 20),
        ("MATCH (a)-[:R]->(b) RETURN b", Unsupported, 6),
        ("MATCH (n:A) RETURN m", Syntax, 19),
        ("MATCH (n:A) RETURN n LIMIT -1", Syntax, 27),
        ("MATCH (n KEY 'a') RETURN n", Syntax, 13),
        ("MATCH (n KEY $k) RETURN n", Parameter, 13),
    ];
    let mismatches: Vec<String> = cases
        .iter()
        .filter_map(|(text, kind, position)| {
            let (got, at) = rejected(text, &none);
            ((got, at) != (*kind, *position)).then(|| format!("{text}: {got:?} at {at}"))
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
    // Unused and mistyped parameters.
    let extra = params(&[("unused", MetadataValue::Integer(1))]);
    let text = "MATCH (n:A) RETURN n";
    assert_eq!(rejected(text, &extra), (Parameter, text.len()));
    let wrong = params(&[("k", MetadataValue::Integer(1))]);
    assert_eq!(rejected("MATCH (n KEY $k) RETURN n", &wrong).0, Parameter);
    // Size caps.
    let long = format!(
        "MATCH (n:A) WHERE n.s = \"{}\" RETURN n",
        "x".repeat(20_000)
    );
    assert_eq!(rejected(&long, &none).0, Limit);
    let many = format!(
        "MATCH (n:A) WHERE {} RETURN n",
        vec!["n.w = 1"; 400].join(" AND ")
    );
    assert_eq!(rejected(&many, &none).0, Limit);
}

#[test]
fn the_vector_and_graph_languages_keep_separate_entry_points() {
    assert!(vlite::query_language::parse("MATCH (n:A) RETURN n").is_err());
    assert!(vlite::query_language::parse("SEARCH docs NEAR $q LIMIT 3").is_ok());
    assert_eq!(
        rejected("SEARCH docs NEAR $q LIMIT 3", &no_params()).0,
        GraphQueryErrorKind::Unsupported
    );
}

#[test]
fn garbage_and_mutated_texts_never_panic() {
    let seeds = [
        r#"MATCH p = (a KEY $k)-[r:R|S*1..3 {w: 1}]->(b:A {s: "x"}) WHERE b.w >= 1 AND r.w <= 2.5 RETURN b, p LIMIT 9"#,
        r#"MATCH p = SHORTEST PATH (a ID 1)<-[:T*..4]-(b KEY "z") RETURN p"#,
        r#"MATCH (n:`A b`) WHERE n.flag = true AND n.v >= -1e3 RETURN n.v LIMIT 2"#,
        r#"MATCH ()-[r KEY "e\"1"]-() RETURN r"#,
    ];
    let alphabet: Vec<char> = "()[]{}<>-=*.:,|$`\"'\\ aZ09_;!éMATCHRETURNWHERE\n\t\u{0}"
        .chars()
        .collect();
    let with_k = params(&[("k", string("n1"))]);
    let mut rng = Rng(0x5eed);
    for seed in seeds {
        let params = if seed.contains("$k") {
            with_k.clone()
        } else {
            no_params()
        };
        if let Err(error) = parse_graph_query(seed, &params) {
            panic!("{seed}: {error}");
        }
        for end in 0..=seed.len() {
            if seed.is_char_boundary(end) {
                let _ = parse_graph_query(&seed[..end], &params);
            }
        }
        for _ in 0..500 {
            let mut chars: Vec<char> = seed.chars().collect();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(chars.len() as u64) as usize;
                let replacement = alphabet[rng.below(alphabet.len() as u64) as usize];
                match rng.below(3) {
                    0 => chars[at] = replacement,
                    1 => chars.insert(at, replacement),
                    _ => {
                        chars.remove(at);
                    }
                }
            }
            let text: String = chars.into_iter().collect();
            if let Err(error) = parse_graph_query(&text, &params) {
                assert!(error.position <= text.len(), "{text}: {error}");
            }
        }
    }
    for _ in 0..2_000 {
        let text: String = (0..rng.below(80))
            .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
            .collect();
        if let Err(error) = parse_graph_query(&text, &no_params()) {
            assert!(error.position <= text.len(), "{text}: {error}");
        }
    }
}

// Semantic equivalence on seeded graphs.

fn seeded_graph(path: &Path, rng: &mut Rng) -> (Database, usize) {
    let mut db = Database::create_with_graph(path, 2, CreateOptions::default()).unwrap();
    db.create_graph("g").unwrap();
    let nodes = 10 + rng.below(15) as usize;
    let mut ops = Vec::new();
    for index in 0..nodes {
        let labels = LABELS
            .iter()
            .filter(|_| rng.chance(2))
            .map(|label| label.to_string())
            .collect();
        ops.push(GraphOp::CreateNode {
            key: format!("n{index}"),
            labels,
            properties: properties(rng),
        });
    }
    for index in 0..nodes * 3 {
        ops.push(GraphOp::CreateEdge {
            key: Some(format!("e{index}")),
            source: format!("n{}", rng.below(nodes as u64)),
            relationship: TYPES[rng.below(3) as usize].into(),
            destination: format!("n{}", rng.below(nodes as u64)),
            properties: properties(rng),
        });
    }
    db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    (db, nodes)
}

fn properties(rng: &mut Rng) -> Metadata {
    let mut properties = Metadata::new();
    if !rng.chance(4) {
        properties.insert("w".into(), MetadataValue::Integer(rng.below(4) as i64));
    }
    if rng.chance(2) {
        let value = if rng.chance(2) { "x" } else { "y" };
        properties.insert("s".into(), string(value));
    }
    properties
}

/// A random text, its parameters, and the typed query and options it must
/// be equivalent to.
fn random_case(
    rng: &mut Rng,
    nodes: usize,
) -> (String, Metadata, GraphQuery, ReadOptions<'static>) {
    let mut bound = Metadata::new();
    let key = format!("n{}", rng.below(nodes as u64));
    bound.insert("k".into(), string(&key));
    let (arrow_left, arrow_right, direction) = match rng.below(3) {
        0 => ("-", "->", Direction::Out),
        1 => ("<-", "-", Direction::In),
        _ => ("-", "-", Direction::Both),
    };
    let types: Vec<String> = TYPES
        .iter()
        .filter(|_| rng.chance(2))
        .map(|name| name.to_string())
        .collect();
    let type_text = if types.is_empty() {
        String::new()
    } else {
        format!(":{}", types.join("|"))
    };
    let typed_types = (!types.is_empty()).then(|| types.clone());
    let w = rng.below(4) as i64;
    let limit = 1 + rng.below(30);
    let base = ReadOptions {
        limits: GraphLimits::default(),
        ..ReadOptions::default()
    };
    match rng.below(4) {
        0 => {
            let label = LABELS[rng.below(2) as usize];
            bound.remove("k");
            bound.insert("w".into(), MetadataValue::Integer(w));
            let text = format!("MATCH (n:{label}) WHERE n.w >= $w RETURN n LIMIT {limit}");
            let query = GraphQuery::FindNodes(FindNodes {
                label: Some(label.into()),
                conditions: vec![range("w", Some(w), None)],
                limit: Some(limit),
                cursor: None,
            });
            (text, bound, query, base)
        }
        1 => {
            let text = format!(
                "MATCH (a KEY $k){arrow_left}[r{type_text}]{arrow_right}(b) WHERE r.w <= {w} RETURN r.w, b.w LIMIT {limit}"
            );
            let query = GraphQuery::Neighbors(Neighbors {
                direction,
                types: typed_types,
                edge_conditions: vec![range("w", None, Some(w))],
                limit: Some(limit),
                ..Neighbors::new(NodeSelector::Key(key))
            });
            let options = ReadOptions {
                projection: PropertyProjection::Only(vec!["w".into()]),
                ..base
            };
            (text, bound, query, options)
        }
        2 => {
            let hops = 1 + rng.below(4);
            let label = LABELS[rng.below(2) as usize];
            let text = format!(
                "MATCH p = (a KEY $k){arrow_left}[r{type_text}*1..{hops}]{arrow_right}(b:{label}) WHERE b.w >= {w} RETURN b, p LIMIT {limit}"
            );
            let query = GraphQuery::Traverse(Traverse {
                direction,
                types: typed_types,
                return_filter: NodeFilter {
                    labels: vec![label.into()],
                    conditions: vec![range("w", Some(w), None)],
                },
                include_paths: true,
                ..Traverse::new(vec![NodeSelector::Key(key)])
            });
            let options = ReadOptions {
                limits: GraphLimits {
                    max_hops: hops,
                    max_output_nodes: limit,
                    ..GraphLimits::default()
                },
                ..base
            };
            (text, bound, query, options)
        }
        _ => {
            let hops = 1 + rng.below(4);
            let destination = format!("n{}", rng.below(nodes as u64));
            bound.insert("d".into(), string(&destination));
            let text = format!(
                "MATCH p = SHORTEST PATH (a KEY $k){arrow_left}[{type_text}*..{hops}]{arrow_right}(b KEY $d) RETURN p"
            );
            let query = GraphQuery::ShortestPath(ShortestPath {
                direction,
                types: typed_types,
                ..ShortestPath::new(NodeSelector::Key(key), NodeSelector::Key(destination))
            });
            let options = ReadOptions {
                limits: GraphLimits {
                    max_hops: hops,
                    ..GraphLimits::default()
                },
                ..base
            };
            (text, bound, query, options)
        }
    }
}

#[test]
fn query_text_returns_exactly_what_the_typed_query_returns() {
    let dir = tempdir().unwrap();
    let mut kinds = [0usize; 4];
    for seed in 0..12 {
        let mut rng = Rng(seed);
        let path = dir.path().join(format!("g{seed}.vlite"));
        let (db, nodes) = seeded_graph(&path, &mut rng);
        let graph = db.graph("g").unwrap();
        for _ in 0..40 {
            let (text, bound, typed, options) = random_case(&mut rng, nodes);
            let parsed = parsed(&text, &bound);
            assert_eq!(parsed.query, typed, "{text}");
            assert_eq!(parsed.projection, options.projection, "{text}");
            assert_eq!(
                parsed.limits(GraphLimits::default()),
                options.limits,
                "{text}"
            );
            let expected = graph.query(&typed, &options).unwrap();
            let actual = graph
                .query_text(&text, &bound, &GraphLimits::default())
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(actual, expected, "{text}");
            kinds[match actual.output {
                GraphOutput::Nodes(_) => 0,
                GraphOutput::Neighbors(_) => 1,
                GraphOutput::Traversal(_) => 2,
                _ => 3,
            }] += 1;
            let explained = graph
                .explain_text(&text, &bound, &GraphLimits::default())
                .unwrap();
            assert_eq!(
                explained,
                graph.explain(&typed, &options).unwrap(),
                "{text}"
            );
        }
    }
    assert!(kinds.iter().all(|count| *count > 10), "{kinds:?}");
}

#[test]
fn lookups_and_errors_through_the_graph_handle() {
    let dir = tempdir().unwrap();
    let mut rng = Rng(99);
    let (db, _) = seeded_graph(&dir.path().join("g.vlite"), &mut rng);
    let graph = db.graph("g").unwrap();
    let none = no_params();
    let limits = GraphLimits::default();
    let read = graph
        .query_text(r#"MATCH (n KEY "n0") RETURN n"#, &none, &limits)
        .unwrap();
    assert_eq!(
        read.output,
        GraphOutput::Node(graph.get_node("n0").unwrap().output)
    );
    let read = graph
        .query_text(r#"MATCH ()-[r KEY "e0"]-() RETURN r"#, &none, &limits)
        .unwrap();
    assert_eq!(
        read.output,
        GraphOutput::Edge(graph.get_edge_by_key("e0").unwrap().output)
    );
    let read = graph
        .query_text(r#"MATCH (n KEY "missing") RETURN n"#, &none, &limits)
        .unwrap();
    assert_eq!(read.output, GraphOutput::Node(None));

    // Text errors come before any graph access; read errors are the typed
    // read's errors.
    assert!(matches!(
        graph.query_text("MATCH", &none, &limits),
        Err(GraphTextError::Query(_))
    ));
    let over = GraphLimits {
        max_hops: 9,
        ..GraphLimits::default()
    };
    assert!(matches!(
        graph.query_text(r#"MATCH (n KEY "n0") RETURN n"#, &none, &over),
        Err(GraphTextError::Read(Error::GraphLimitExceedsCeiling { .. }))
    ));
    let bad_value = params(&[("x", MetadataValue::Float(f64::NAN))]);
    assert!(matches!(
        graph.query_text("MATCH (n:A) WHERE n.w >= $x RETURN n", &bad_value, &limits),
        Err(GraphTextError::Read(Error::InvalidMetadata(_)))
    ));
}
