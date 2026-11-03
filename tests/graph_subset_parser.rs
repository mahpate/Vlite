//! Golden syntax, static planning, parameter and boundary tests for the
//! private `vlite-cypher-1` parser. This includes the same engine source used
//! by the library; it does not expose a public parser entry point.

#[allow(dead_code, unused_imports)]
#[path = "../src/property_graph/engine/mod.rs"]
mod engine;

use engine::language::{LanguageError, LanguageErrorKind};
use engine::model::PropertyValue;
use engine::subset::plan::{ParameterEntry, PlanError, plan};
use engine::subset::{
    Anchor, AnchorKind, Compare, Creation, CreationNode, Direction, FieldTarget, Mutation, Name,
    NodePattern, ParameterRef, Pattern, Plan, Predicate, RelationshipPattern, ReturnClause,
    ReturnItem, ReturnValue, Statement,
};

fn name(text: &str, position: usize) -> Name {
    Name {
        text: text.into(),
        position,
        quoted: false,
    }
}

#[test]
fn undirected_relationship_ast_is_golden() {
    let text = "MATCH (a KEY $ak)-[:R]-(b) RETURN b";
    let planned = successful(text, &[param("ak", PropertyValue::String("a".into()))]);
    let (relationship, _) = planned
        .statement
        .pattern
        .as_ref()
        .unwrap()
        .relationship
        .as_ref()
        .unwrap();
    assert_eq!(
        relationship,
        &RelationshipPattern {
            variable: None,
            types: vec![name("R", text.find("R]").unwrap())],
            anchor: None,
            max_hops: None,
            hop_position: None,
            properties: vec![],
            direction: Direction::Both,
            position: text.find("-[").unwrap(),
        }
    );
}

#[test]
fn relationship_anchor_without_variable_is_golden_for_key_and_id() {
    for (text, kind, parameter, value) in [
        (
            "MATCH (a KEY $ak)-[KEY $rk]->(b) RETURN b",
            AnchorKind::Key,
            "rk",
            PropertyValue::String("r".into()),
        ),
        (
            "MATCH (a KEY $ak)-[ID $rid]->(b) RETURN b",
            AnchorKind::Id,
            "rid",
            PropertyValue::Integer(7),
        ),
    ] {
        let planned = successful(
            text,
            &[
                param("ak", PropertyValue::String("a".into())),
                param(parameter, value),
            ],
        );
        let (relationship, _) = planned
            .statement
            .pattern
            .as_ref()
            .unwrap()
            .relationship
            .as_ref()
            .unwrap();
        let relationship_start = text.find("-[").unwrap();
        let anchor_keyword = if kind == AnchorKind::Key { "KEY" } else { "ID" };
        assert_eq!(
            relationship,
            &RelationshipPattern {
                variable: None,
                types: vec![],
                anchor: Some(Anchor {
                    kind,
                    parameter: parameter_ref(
                        parameter,
                        text.find(&format!("${parameter}")).unwrap()
                    ),
                    position: relationship_start
                        + text[relationship_start..].find(anchor_keyword).unwrap(),
                }),
                max_hops: None,
                hop_position: None,
                properties: vec![],
                direction: Direction::Out,
                position: relationship_start,
            }
        );
    }

    let text = "MATCH (KEY $k) RETURN n";
    assert_query_error_at(
        text,
        &[param("k", PropertyValue::String("k".into()))],
        LanguageErrorKind::Syntax,
        text.find("KEY").unwrap(),
    );
}

#[test]
fn incoming_second_relationship_hop_is_rejected_at_its_introducer() {
    for text in [
        "MATCH (a)-[:R]->(b)<-[:S]-(c) RETURN c",
        "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c",
    ] {
        let error = query_error(text, &[]);
        assert_eq!(
            error.kind,
            LanguageErrorKind::Unsupported,
            "{text}: {error}"
        );
        assert_eq!(error.position, 19, "{text}: {error}");
        assert!(
            error.message.contains("chained relationships"),
            "{text}: {error}"
        );
    }
}

fn parameter_ref(name: &str, position: usize) -> ParameterRef {
    ParameterRef {
        name: name.into(),
        position,
    }
}

fn param(name: &str, value: PropertyValue) -> ParameterEntry {
    ParameterEntry {
        name: name.into(),
        value,
        position: 0,
    }
}

fn successful(text: &str, parameters: &[ParameterEntry]) -> Plan {
    plan(text, parameters).unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn query_error(text: &str, parameters: &[ParameterEntry]) -> LanguageError {
    match plan(text, parameters).expect_err(text) {
        PlanError::Query(error) => {
            assert!(error.position <= text.len(), "{text}: {error}");
            error
        }
        other => panic!("expected query error, got {other:?}"),
    }
}

fn assert_query_error_at(
    text: &str,
    parameters: &[ParameterEntry],
    kind: LanguageErrorKind,
    position: usize,
) {
    let error = query_error(text, parameters);
    assert_eq!(error.kind, kind, "{text}: {error}");
    assert_eq!(error.position, position, "{text}: {error}");
}

fn limit_map(count: usize) -> String {
    let properties = (0..count)
        .map(|index| format!("p{index}:$value"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("MATCH (n {{{properties}}}) RETURN n")
}

#[test]
fn read_ast_matches_the_versioned_grammar_golden() {
    let text = "MATCH (n:Person KEY $key) RETURN n.name AS name";
    let plan = successful(text, &[param("key", PropertyValue::String("p-1".into()))]);
    assert_eq!(
        plan.statement,
        Statement {
            pattern: Some(Pattern {
                path_variable: None,
                start: NodePattern {
                    variable: Some(name("n", 7)),
                    labels: vec![name("Person", 9)],
                    anchor: Some(Anchor {
                        kind: AnchorKind::Key,
                        parameter: parameter_ref("key", 20),
                        position: 16,
                    }),
                    properties: vec![],
                    position: 6,
                },
                relationship: None,
                position: 6,
            }),
            as_of: None,
            predicate: None,
            mutations: vec![],
            returning: Some(ReturnClause {
                items: vec![ReturnItem {
                    value: ReturnValue::Field(FieldTarget {
                        variable: name("n", 33),
                        field: name("name", 35),
                    }),
                    alias: Some(name("name", 43)),
                    position: 33,
                }],
                order_by: vec![],
                skip: None,
                limit: None,
                position: 26,
            }),
        }
    );
    assert_eq!(plan.parameters["key"], PropertyValue::String("p-1".into()));
}

#[test]
fn write_ast_and_variable_length_pattern_goldens_are_private_and_exact() {
    let create_text = "CREATE (n:Person KEY $key)";
    let create_plan = successful(
        create_text,
        &[param("key", PropertyValue::String("p-1".into()))],
    );
    assert_eq!(
        create_plan.statement,
        Statement {
            pattern: None,
            as_of: None,
            predicate: None,
            mutations: vec![Mutation::Create {
                creations: vec![Creation::Node(CreationNode {
                    variable: name("n", 8),
                    labels: vec![name("Person", 10)],
                    key: parameter_ref("key", 21),
                    properties: vec![],
                    position: 7,
                })],
                position: 0,
            }],
            returning: None,
        }
    );

    let path_text = "MATCH p = (a KEY $k)-[:R*1..3]->(b) RETURN p";
    let path_plan = successful(path_text, &[param("k", PropertyValue::String("a".into()))]);
    let pattern = path_plan.statement.pattern.as_ref().unwrap();
    assert_eq!(pattern.path_variable, Some(name("p", 6)));
    let (relationship, _) = pattern.relationship.as_ref().unwrap();
    assert_eq!(relationship.types, vec![name("R", 23)]);
    assert_eq!(relationship.max_hops, Some(3));
    assert_eq!(relationship.direction, Direction::Out);
}

#[test]
fn match_predicate_projection_and_mutation_clause_goldens_cover_the_grammar() {
    let read = "MATCH (a KEY $a)<-[r:R|S KEY $r {x:$x}]-(b) WHERE a.age >= $age AND r.x IS NOT NULL RETURN r.x AS score ORDER BY score DESC SKIP $skip LIMIT $limit";
    let read_plan = successful(
        read,
        &[
            param("a", PropertyValue::String("a".into())),
            param("r", PropertyValue::String("r".into())),
            param("x", PropertyValue::Integer(1)),
            param("age", PropertyValue::Integer(21)),
            param("skip", PropertyValue::Integer(2)),
            param("limit", PropertyValue::Integer(7)),
        ],
    );
    let pattern = read_plan.statement.pattern.as_ref().unwrap();
    assert_eq!(
        pattern.relationship.as_ref().unwrap().0.direction,
        Direction::In
    );
    assert_eq!(
        pattern.relationship.as_ref().unwrap().0.types,
        vec![name("R", 21), name("S", 23)]
    );
    assert!(matches!(
        read_plan.statement.predicate,
        Some(Predicate::And(terms)) if terms.len() == 2
    ));
    let returning = read_plan.statement.returning.as_ref().unwrap();
    assert_eq!(returning.order_by[0].name.text, "score");
    assert_eq!(
        returning.skip,
        Some(parameter_ref("skip", read.find("$skip").unwrap()))
    );
    assert_eq!(
        returning.limit,
        Some(parameter_ref("limit", read.find("$limit").unwrap()))
    );

    let write = "CREATE (a:Person KEY $a {name:$name}), (b:Person KEY $b), (a)-[r:KNOWS]->(b) MERGE (c:Person KEY $c) MERGE (a)-[m:KNOWS KEY $edge]->(b) SET a.age = $age, a:Adult, m.since = $year REMOVE a.nickname, a:Former, m.old DELETE m DETACH DELETE b RETURN a.name AS name";
    let write_plan = successful(
        write,
        &[
            param("a", PropertyValue::String("a".into())),
            param("name", PropertyValue::String("Ada".into())),
            param("b", PropertyValue::String("b".into())),
            param("c", PropertyValue::String("c".into())),
            param("edge", PropertyValue::String("e".into())),
            param("age", PropertyValue::Integer(37)),
            param("year", PropertyValue::Integer(2026)),
        ],
    );
    assert_eq!(write_plan.statement.mutations.len(), 7);
    assert!(matches!(
        &write_plan.statement.mutations[0],
        Mutation::Create { creations, .. } if creations.len() == 3
    ));
    assert!(matches!(
        &write_plan.statement.mutations[2],
        Mutation::Merge { creation: Creation::Edge(edge), .. } if edge.key.is_some()
    ));
    assert!(matches!(
        &write_plan.statement.mutations[6],
        Mutation::Delete { detach: true, .. }
    ));
}

#[test]
fn reserved_identifiers_are_distinct_from_names_and_parameters() {
    let text = "MATCH (n:MATCH {RETURN:$WHERE}) RETURN n.MATCH";
    successful(text, &[param("WHERE", PropertyValue::Integer(9))]);

    let quoted = "MATCH (`MATCH`) RETURN `MATCH`";
    let plan = successful(quoted, &[]);
    assert_eq!(
        plan.statement.pattern.as_ref().unwrap().start.variable,
        Some(Name {
            text: "MATCH".into(),
            position: 7,
            quoted: true,
        })
    );

    let text = "MATCH (n) RETURN n AS MATCH";
    let error = query_error(text, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Syntax);
    assert_eq!(error.position, text.rfind("MATCH").unwrap());
}

#[test]
fn parser_rejections_pin_kind_and_utf8_byte_offset() {
    let cases = [
        (
            "MATCH (n) OPTIONAL MATCH (m) RETURN n",
            LanguageErrorKind::Unsupported,
            10,
        ),
        ("MATCH (n) RETURN n;", LanguageErrorKind::Unsupported, 18),
        ("MATCH (n) RETURN n trailing", LanguageErrorKind::Syntax, 19),
        ("MATCH (MATCH) RETURN MATCH", LanguageErrorKind::Syntax, 7),
        (
            "MATCH (a KEY $k)-[r:R*1..2]->(b) RETURN b",
            LanguageErrorKind::Unsupported,
            21,
        ),
        (
            "MATCH (a KEY $k)-[:R*1..0002]->(b) RETURN b",
            LanguageErrorKind::Syntax,
            24,
        ),
        (
            "MATCH (a KEY $k)-[:R*1..0]->(b) RETURN b",
            LanguageErrorKind::Syntax,
            24,
        ),
        (
            "MATCH (a KEY $k)-[:R*1..5]->(b) RETURN b",
            LanguageErrorKind::Limit,
            24,
        ),
        (
            "MATCH (n) RETURN n != $x",
            LanguageErrorKind::Unsupported,
            19,
        ),
        (
            "MATCH (n) RETURN COUNT(n)",
            LanguageErrorKind::Unsupported,
            17,
        ),
        (
            "MATCH (n) WHERE n.x IN $values RETURN n",
            LanguageErrorKind::Unsupported,
            20,
        ),
        (
            "MATCH (n) WHERE n.x = make($value) RETURN n",
            LanguageErrorKind::Unsupported,
            22,
        ),
        (
            "MATCH (n) RETURN EXISTS {}",
            LanguageErrorKind::Unsupported,
            17,
        ),
        (
            "MATCH (n) RETURN COUNT {}",
            LanguageErrorKind::Unsupported,
            17,
        ),
        ("MATCH (n) RETURN TRUE", LanguageErrorKind::Unsupported, 17),
    ];
    for (text, kind, position) in cases {
        let parameters = if text.contains("$k") {
            vec![param("k", PropertyValue::String("k".into()))]
        } else {
            vec![]
        };
        let error = query_error(text, &parameters);
        assert_eq!(error.kind, kind, "{text}: {error}");
        assert_eq!(error.position, position, "{text}: {error}");
    }
}

#[test]
fn excluded_grammar_forms_pin_kinds_and_introducer_offsets() {
    let unsupported_clauses = [
        "OPTIONAL",
        "WITH",
        "UNWIND",
        "CALL",
        "YIELD",
        "UNION",
        "FOREACH",
        "LOAD",
        "CSV",
        "USE",
        "INSERT",
        "UPDATE",
        "DROP",
        "ALTER",
        "INDEX",
        "CONSTRAINT",
        "EXPLAIN",
        "PROFILE",
        "SHORTEST",
        "SEARCH",
        "NEAR",
        "DISTINCT",
        "IN",
        "EXISTS",
    ];
    for keyword in unsupported_clauses {
        let text = format!("MATCH (n) RETURN {keyword}");
        assert_query_error_at(
            &text,
            &[],
            LanguageErrorKind::Unsupported,
            text.find(keyword).unwrap(),
        );
    }

    let excluded_forms = [
        ("MATCH (n) RETURN DISTINCT n", "DISTINCT"),
        ("MATCH (a) MATCH (b) RETURN b", "MATCH"),
        (
            "MATCH shortestPath((a KEY $k)-[:R*1..2]->(b)) RETURN a",
            "shortestPath",
        ),
        (
            "MATCH p = shortestPath((a KEY $k)-[:R*1..2]->(b)) RETURN p",
            "shortestPath",
        ),
        ("MATCH (n) WHERE n.x IN $values RETURN n", "IN"),
        ("MATCH (n) WHERE n.x = make($value) RETURN n", "make"),
        ("MATCH (n) RETURN EXISTS {}", "EXISTS"),
        ("MATCH (n) RETURN COUNT {}", "COUNT"),
        ("MATCH (n) RETURN n != $x", "!="),
        ("MATCH (n) WHERE n.x = $x + $y RETURN n", "+"),
        ("MATCH (n) WHERE n.x = [$x] RETURN n", "["),
        ("MATCH (n) // comment\nRETURN n", "//"),
        ("MATCH (n) RETURN n;", ";"),
    ];
    for (text, introducer) in excluded_forms {
        let parameters = [
            param("k", PropertyValue::String("k".into())),
            param("values", PropertyValue::Integer(1)),
            param("value", PropertyValue::Integer(1)),
            param("x", PropertyValue::Integer(1)),
            param("y", PropertyValue::Integer(2)),
        ];
        if text == "MATCH (a) MATCH (b) RETURN b" {
            let error = query_error(text, &parameters);
            assert_eq!(error.kind, LanguageErrorKind::Unsupported);
            assert_eq!(
                error.position,
                text.match_indices("MATCH").nth(1).unwrap().0
            );
            assert_eq!(
                error.message,
                "a second MATCH clause is not supported by vlite-cypher-1"
            );
            continue;
        }
        assert_query_error_at(
            text,
            &parameters,
            LanguageErrorKind::Unsupported,
            text.find(introducer).unwrap(),
        );
    }

    let malformed = [
        ("", 0),
        ("WHERE n.x = $v", 0),
        ("MATCH (n) RETURN 1", 17),
        ("MATCH (n {x: 1}) RETURN n", 13),
        ("MATCH (n) RETURN *", 17),
        ("MATCH (a), (b) RETURN a", 9),
        ("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a", 19),
    ];
    for (text, position) in malformed {
        let expected = if text.contains("RETURN 1")
            || text.contains("{x: 1}")
            || text.contains("RETURN *")
            || text.contains("), (")
            || text.contains("->(b)-")
        {
            LanguageErrorKind::Unsupported
        } else {
            LanguageErrorKind::Syntax
        };
        assert_query_error_at(text, &[], expected, position);
    }

    for suffix in ["*2..3", "*", "*..3"] {
        let text = format!("MATCH (a KEY $k)-[:R{suffix}]->(b) RETURN b");
        assert_query_error_at(
            &text,
            &[param("k", PropertyValue::String("k".into()))],
            LanguageErrorKind::Unsupported,
            text.find('*').unwrap(),
        );
    }
    for (suffix, kind) in [
        ("*1..02", LanguageErrorKind::Syntax),
        ("*1..0", LanguageErrorKind::Syntax),
        ("*1..5", LanguageErrorKind::Limit),
        ("*1..99999999999999999999", LanguageErrorKind::Limit),
    ] {
        let text = format!("MATCH (a KEY $k)-[:R{suffix}]->(b) RETURN b");
        assert_query_error_at(
            &text,
            &[param("k", PropertyValue::String("k".into()))],
            kind,
            text.find(suffix).unwrap() + suffix.find("..").unwrap() + 2,
        );
    }
    let text = "MATCH (a KEY $k)-[:R*1..1]->(b) RETURN b";
    successful(text, &[param("k", PropertyValue::String("k".into()))]);

    let flat_predicates = std::iter::repeat_n("(n.x = $v)", 17)
        .collect::<Vec<_>>()
        .join(" AND ");
    let text = format!("MATCH (n) WHERE {flat_predicates} RETURN n");
    successful(&text, &[param("v", PropertyValue::Integer(1))]);

    let nested_predicate = format!("{}n.x = $v{}", "(".repeat(17), ")".repeat(17));
    let text = format!("MATCH (n) WHERE {nested_predicate} RETURN n");
    assert_query_error_at(
        &text,
        &[param("v", PropertyValue::Integer(1))],
        LanguageErrorKind::Limit,
        text.find("WHERE").unwrap() + 6 + 16,
    );

    let merge_without_key = "CREATE (a KEY $a), (b KEY $b) MERGE (a)-[r:R]->(b)";
    assert_query_error_at(
        merge_without_key,
        &[
            param("a", PropertyValue::String("a".into())),
            param("b", PropertyValue::String("b".into())),
        ],
        LanguageErrorKind::Syntax,
        merge_without_key.find(']').unwrap(),
    );
}

#[test]
fn second_match_is_unsupported_across_clause_positions() {
    let cases = [
        ("MATCH (a) MATCH (b) RETURN b", vec![]),
        ("MATCH (a) MATCH p = (b) RETURN p", vec![]),
        (
            "MATCH (a KEY $ak) WHERE a.x = $p MATCH p = (b)",
            vec![
                param("ak", PropertyValue::String("a".into())),
                param("p", PropertyValue::Integer(1)),
            ],
        ),
        ("MATCH (a) WHERE a.x = MATCH (b)", vec![]),
        ("MATCH (a) RETURN a MATCH (b)", vec![]),
        ("MATCH (a) RETURN a MATCH p = (b)", vec![]),
        (
            "MATCH (a) SET a.x = $p MATCH (b) RETURN a",
            vec![param("p", PropertyValue::Integer(1))],
        ),
    ];
    for (text, parameters) in cases {
        let error = query_error(text, &parameters);
        assert_eq!(
            error.kind,
            LanguageErrorKind::Unsupported,
            "{text}: {error}"
        );
        assert_eq!(
            error.position,
            text.match_indices("MATCH").nth(1).unwrap().0,
            "{text}: {error}"
        );
        assert_eq!(
            error.message, "a second MATCH clause is not supported by vlite-cypher-1",
            "{text}: {error}"
        );
    }

    let identifier_slots = [
        "MATCH (n) WHERE MATCH (b).x = $v RETURN n",
        "MATCH (n) WHERE MATCH p = (b).x = $v RETURN n",
        "MATCH (n) WHERE n.x = MATCH (b)",
        "MATCH (n) WHERE n.x = MATCH p = (b)",
        "MATCH (n) RETURN MATCH (b)",
        "MATCH (n) RETURN MATCH p = (b)",
        "MATCH (n) RETURN n AS MATCH (b)",
        "MATCH (n) RETURN n AS MATCH p = (b)",
        "MATCH (n) RETURN n ORDER BY MATCH (b)",
        "MATCH (n) RETURN n ORDER BY MATCH p = (b)",
        "MATCH (n) DELETE MATCH (b)",
        "MATCH (n) DELETE MATCH p = (b)",
    ];
    for text in identifier_slots {
        let error = query_error(text, &[]);
        assert_eq!(
            error.kind,
            LanguageErrorKind::Unsupported,
            "{text}: {error}"
        );
        assert_eq!(
            error.position,
            text.match_indices("MATCH").nth(1).unwrap().0,
            "{text}: {error}"
        );
        assert_eq!(
            error.message, "a second MATCH clause is not supported by vlite-cypher-1",
            "{text}: {error}"
        );
    }

    for text in ["MATCH (MATCH (n)) RETURN n", "CREATE (MATCH (n))"] {
        let error = query_error(text, &[]);
        assert_eq!(error.kind, LanguageErrorKind::Syntax, "{text}: {error}");
        assert_eq!(
            error.position,
            text.rfind("MATCH").unwrap(),
            "{text}: {error}"
        );
        assert_eq!(
            error.message, "reserved words must be backquoted when used as variables or aliases",
            "{text}: {error}"
        );
    }

    assert_query_error_at(
        "MATCH MATCH (n)",
        &[],
        LanguageErrorKind::Syntax,
        "MATCH ".len(),
    );
}

#[test]
fn clause_keywords_followed_by_parentheses_keep_clause_classification() {
    let supported_clause = "MATCH (n) RETURN n CREATE (m KEY $k)";
    assert_query_error_at(
        supported_clause,
        &[],
        LanguageErrorKind::Syntax,
        supported_clause.find("CREATE").unwrap(),
    );
    let error = query_error(supported_clause, &[]);
    assert_eq!(
        error.message,
        "expected the end of the statement, found word"
    );

    let excluded_clause = "MATCH (n) RETURN n DISTINCT (m)";
    let error = query_error(excluded_clause, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Unsupported);
    assert_eq!(error.position, excluded_clause.find("DISTINCT").unwrap());
    assert_eq!(error.message, "DISTINCT is not supported by vlite-cypher-1");

    let variable_slot = "MATCH (n) RETURN CREATE (m)";
    let error = query_error(variable_slot, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Syntax);
    assert_eq!(error.position, variable_slot.find("CREATE").unwrap());
    assert_eq!(
        error.message,
        "reserved words must be backquoted when used as variables or aliases"
    );

    let parameter_slot = "MATCH (n) AS OF DISTINCT (m) RETURN n";
    let error = query_error(parameter_slot, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Unsupported);
    assert_eq!(error.position, parameter_slot.find("DISTINCT").unwrap());
    assert_eq!(error.message, "DISTINCT is not supported by vlite-cypher-1");
}

#[test]
fn reserved_function_names_remain_unsupported_at_expression_sites() {
    for function in ["id", "all"] {
        for spacing in ["", " "] {
            let cases = [
                format!("MATCH (n) RETURN {function}{spacing}(n)"),
                format!("MATCH (n) WHERE {function}{spacing}(n).x = $v RETURN n"),
                format!("MATCH (n) RETURN n ORDER BY {function}{spacing}(n)"),
            ];
            for text in cases {
                let error = query_error(&text, &[]);
                assert_eq!(
                    error.kind,
                    LanguageErrorKind::Unsupported,
                    "{text}: {error}"
                );
                assert_eq!(
                    error.position,
                    text.find(function).unwrap(),
                    "{text}: {error}"
                );
                assert_eq!(
                    error.message, "function invocation is not supported",
                    "{text}: {error}"
                );
            }
        }
    }

    let not_a_clause_start = "MATCH (n) WHERE MATCH 1 = n.x RETURN n";
    let error = query_error(not_a_clause_start, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Syntax);
    assert_eq!(
        error.position,
        not_a_clause_start.match_indices("MATCH").nth(1).unwrap().0
    );
    assert_eq!(
        error.message,
        "reserved words must be backquoted when used as variables or aliases"
    );

    let function_invocation = "MATCH (n) RETURN count(n)";
    let error = query_error(function_invocation, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Unsupported);
    assert_eq!(error.position, function_invocation.find("count").unwrap());
    assert_eq!(error.message, "function invocation is not supported");
}

#[test]
fn reserved_function_names_remain_unsupported_at_clause_positions() {
    for function in ["ID", "ALL", "id", "all"] {
        for spacing in ["", " "] {
            let cases = [
                format!("MATCH (n) {function}{spacing}(n)"),
                format!("{function}{spacing}(n)"),
                format!("MATCH (n) RETURN n {function}{spacing}(n)"),
                format!("MATCH (n) WHERE n.x = $v {function}{spacing}(n)"),
                format!("MATCH (n) RETURN n ORDER BY n {function}{spacing}(n)"),
                format!("MATCH (n) AS OF {function}{spacing}(n) RETURN n"),
            ];
            for text in cases {
                let error = query_error(&text, &[]);
                assert_eq!(
                    error.kind,
                    LanguageErrorKind::Unsupported,
                    "{text}: {error}"
                );
                assert_eq!(
                    error.position,
                    text.find(function).unwrap(),
                    "{text}: {error}"
                );
                assert_eq!(
                    error.message, "function invocation is not supported",
                    "{text}: {error}"
                );
            }
        }
    }
}

#[test]
fn return_star_and_pagination_literals_pin_unsupported_diagnostics() {
    let text = "MATCH (n) RETURN *";
    let error = query_error(text, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Unsupported);
    assert_eq!(error.position, text.find('*').unwrap());
    assert_eq!(
        error.message,
        "RETURN * is not supported; name each projected variable or field"
    );

    for text in ["MATCH (n) RETURN n SKIP 5", "MATCH (n) RETURN n LIMIT 5"] {
        let error = query_error(text, &[]);
        assert_eq!(
            error.kind,
            LanguageErrorKind::Unsupported,
            "{text}: {error}"
        );
        assert_eq!(error.position, text.find('5').unwrap(), "{text}: {error}");
        assert_eq!(
            error.message, "numeric literals are not supported; bind data with a typed parameter",
            "{text}: {error}"
        );
    }
}

#[test]
fn lexer_rejections_pin_categories_and_utf8_offsets() {
    successful("match (n)\treturn n", &[]);
    successful("match (n)\rreturn n", &[]);
    let cases = [
        ("MATCH (n)\u{000c} RETURN n", 9, LanguageErrorKind::Syntax),
        ("MATCH (n)\u{000b} RETURN n", 9, LanguageErrorKind::Syntax),
        ("MATCH (n)\u{00a0} RETURN n", 9, LanguageErrorKind::Syntax),
        ("MATCH (`\u{007f}`) RETURN n", 8, LanguageErrorKind::Syntax),
        ("MATCH (`\u{0085}`) RETURN n", 8, LanguageErrorKind::Syntax),
        ("MATCH (`\0`) RETURN n", 8, LanguageErrorKind::Syntax),
        ("MATCH (``) RETURN n", 7, LanguageErrorKind::Syntax),
        (
            "MATCH (`unterminated) RETURN n",
            7,
            LanguageErrorKind::Syntax,
        ),
        ("MATCH (n) RETURN $", 17, LanguageErrorKind::Syntax),
        ("MATCH (n) RETURN $1x", 17, LanguageErrorKind::Syntax),
        ("MATCH (é) RETURN n", 7, LanguageErrorKind::Syntax),
        ("MATCH (aé) RETURN n", 8, LanguageErrorKind::Syntax),
        ("MATCH (n) RETURN n ! $x", 19, LanguageErrorKind::Syntax),
        (
            "MATCH (n) RETURN n != $x",
            19,
            LanguageErrorKind::Unsupported,
        ),
        (
            "MATCH (n) RETURN n ^ $x",
            19,
            LanguageErrorKind::Unsupported,
        ),
        ("MATCH (n) RETURN \"a\"", 17, LanguageErrorKind::Unsupported),
        ("match (match) return match", 7, LanguageErrorKind::Syntax),
    ];
    for (text, position, kind) in cases {
        assert_query_error_at(text, &[], kind, position);
    }

    for text in [
        "MATCH (a) WHERE a.x = $1abc RETURN a",
        "MATCH (a) WHERE a.x = $ RETURN a",
    ] {
        let error = query_error(text, &[]);
        assert_eq!(error.kind, LanguageErrorKind::Syntax, "{text}: {error}");
        assert_eq!(error.position, text.find('$').unwrap(), "{text}: {error}");
        assert!(
            error
                .message
                .contains("expected a bare parameter name after $"),
            "{text}: {error}"
        );
    }
}

#[test]
fn double_not_is_syntax_but_parenthesized_negation_is_legal() {
    let text = "MATCH (n) WHERE NOT NOT n.x = $v RETURN n";
    let second_not = text.find("NOT").unwrap() + "NOT ".len();
    assert_query_error_at(
        text,
        &[param("v", PropertyValue::Integer(1))],
        LanguageErrorKind::Syntax,
        second_not,
    );

    let text = "MATCH (n) WHERE NOT (NOT n.x = $v) RETURN n";
    let planned = successful(text, &[param("v", PropertyValue::Integer(1))]);
    assert!(matches!(
        &planned.statement.predicate,
        Some(Predicate::Not(inner)) if matches!(inner.as_ref(), Predicate::Not(_))
    ));
}

#[test]
fn long_not_chain_is_rejected_on_a_256k_stack() {
    let text = format!("MATCH (n) WHERE {}n.x = $v RETURN n", "NOT ".repeat(1_012));
    let parameters = vec![param("v", PropertyValue::Integer(1))];
    let worker = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || plan(&text, &parameters))
        .unwrap();
    let error = match worker.join().expect("bounded parser worker") {
        Err(PlanError::Query(error)) => error,
        other => panic!("expected a syntax error for chained NOT prefixes: {other:?}"),
    };
    assert_eq!(error.kind, LanguageErrorKind::Syntax);
    assert_eq!(error.position, "MATCH (n) WHERE ".len() + "NOT ".len());
}

#[test]
fn predicate_ast_operator_polarity_precedence_and_write_items_are_golden() {
    let operators = [
        ("=", Compare::Equal),
        ("<>", Compare::NotEqual),
        ("<=", Compare::LessEqual),
        (">=", Compare::GreaterEqual),
        ("<", Compare::Less),
        (">", Compare::Greater),
    ];
    for (operator, expected) in operators {
        let text = format!("MATCH (n) WHERE n.x {operator} $v RETURN n");
        let planned = successful(&text, &[param("v", PropertyValue::Integer(1))]);
        assert!(
            matches!(
                &planned.statement.predicate,
                Some(Predicate::Compare { operation, .. }) if *operation == expected
            ),
            "{text}"
        );
    }

    for (suffix, expected_negated) in [("IS NULL", false), ("IS NOT NULL", true)] {
        let text = format!("MATCH (n) WHERE n.x {suffix} RETURN n");
        let planned = successful(&text, &[]);
        assert!(
            matches!(
                &planned.statement.predicate,
                Some(Predicate::IsNull { negated, .. }) if *negated == expected_negated
            ),
            "{text}"
        );
    }

    let text = "MATCH (n) WHERE n.a = $a OR n.b = $b AND NOT (n.c = $c) RETURN n";
    let planned = successful(
        text,
        &[
            param("a", PropertyValue::Integer(1)),
            param("b", PropertyValue::Integer(2)),
            param("c", PropertyValue::Integer(3)),
        ],
    );
    assert!(matches!(
        &planned.statement.predicate,
        Some(Predicate::Or(terms))
            if terms.len() == 2
                && matches!(&terms[0], Predicate::Compare { .. })
                && matches!(&terms[1], Predicate::And(conjunction)
                    if conjunction.len() == 2
                        && matches!(&conjunction[0], Predicate::Compare { .. })
                        && matches!(&conjunction[1], Predicate::Not(_)))
    ));

    let text = "CREATE (n KEY $k) SET n.x = $x, n:New REMOVE n.y, n:Old RETURN n";
    let planned = successful(
        text,
        &[
            param("k", PropertyValue::String("k".into())),
            param("x", PropertyValue::Integer(1)),
        ],
    );
    assert!(matches!(
        &planned.statement.mutations[1],
        Mutation::Set { assignments, .. } if assignments.len() == 2
    ));
    assert!(matches!(
        &planned.statement.mutations[2],
        Mutation::Remove { removals, .. } if removals.len() == 2
    ));
}

#[test]
fn planner_scope_parameter_and_token_boundaries_are_pinned() {
    for (text, marker) in [
        ("MATCH (a KEY $k)-[:R]->() RETURN a", ")"),
        ("MATCH (:L KEY $k)-[:R]->(b) RETURN b", ":"),
    ] {
        assert_query_error_at(
            text,
            &[param("k", PropertyValue::String("k".into()))],
            LanguageErrorKind::Syntax,
            if marker == ")" {
                text.rfind(marker).unwrap()
            } else {
                text.find(marker).unwrap()
            },
        );
    }
    assert_query_error_at(
        "MATCH () RETURN n",
        &[],
        LanguageErrorKind::Syntax,
        "MATCH () RETURN n".find(')').unwrap(),
    );

    for (text, marker) in [
        ("MATCH (a)-[:R]->(b) RETURN b", "-[:"),
        ("MATCH p = (n) RETURN p", "p"),
        (
            "MATCH p = (a KEY $k)-[:R]->(b) WHERE p.x = $x RETURN p",
            "p.x",
        ),
        ("MATCH (a KEY $a)-[r:R]->(b) DETACH DELETE r", "r"),
        ("MATCH p = (a KEY $a)-[:R]->(b) DELETE p", "p"),
        ("MATCH (n) RETURN n, n", "n, n"),
        ("MATCH (n) RETURN n ORDER BY missing", "missing"),
        ("MATCH (n) WHERE NOT m.x = $v RETURN n", "m"),
    ] {
        let parameters = [
            param("k", PropertyValue::String("k".into())),
            param("a", PropertyValue::String("a".into())),
            param("x", PropertyValue::Integer(1)),
            param("v", PropertyValue::Integer(1)),
        ];
        let position = match marker {
            "-[:" => text.find(marker).unwrap(),
            "MATCH" if text.matches("MATCH").count() > 1 => {
                text.match_indices("MATCH").nth(1).unwrap().0
            }
            "n, n" => text.rfind("n").unwrap(),
            "r" if text.contains("DETACH") => text.rfind('r').unwrap(),
            "p" if text.contains("WHERE") => text.find("p.x").unwrap(),
            "p" if text.contains("DELETE") => text.rfind('p').unwrap(),
            _ => text.find(marker).unwrap(),
        };
        assert_query_error_at(text, &parameters, LanguageErrorKind::Syntax, position);
    }

    let text = "MATCH p = (a KEY $a)-[:R]->(b) RETURN p.x";
    assert_query_error_at(
        text,
        &[param("a", PropertyValue::String("a".into()))],
        LanguageErrorKind::Syntax,
        text.rfind("p.x").unwrap(),
    );

    let text = "CREATE (a KEY $a), (b KEY $b), (c KEY $c), (a)-[r:R]->(b), (r)-[s:S]->(c)";
    assert_query_error_at(
        text,
        &[
            param("a", PropertyValue::String("a".into())),
            param("b", PropertyValue::String("b".into())),
            param("c", PropertyValue::String("c".into())),
        ],
        LanguageErrorKind::Syntax,
        text.rfind("(r)").unwrap() + 1,
    );

    let text = "MATCH (a KEY $ak)-[:R KEY $rk *1..3]->(b) RETURN b";
    assert_query_error_at(
        text,
        &[
            param("ak", PropertyValue::String("a".into())),
            param("rk", PropertyValue::String("r".into())),
        ],
        LanguageErrorKind::Unsupported,
        text.find('*').unwrap(),
    );

    let variable_alias = "MATCH (n) RETURN n AS m ORDER BY m";
    assert_query_error_at(
        variable_alias,
        &[],
        LanguageErrorKind::Syntax,
        variable_alias.rfind('m').unwrap(),
    );
    let variable_order = "MATCH (a KEY $ak) RETURN a ORDER BY a";
    assert_query_error_at(
        variable_order,
        &[param("ak", PropertyValue::String("a".into()))],
        LanguageErrorKind::Syntax,
        variable_order.rfind('a').unwrap(),
    );

    for (text, position) in [
        (
            "MATCH (a KEY $ak)-[r:R]->(b) CREATE (a)-[e:S KEY $k]->(r)",
            55,
        ),
        (
            "MATCH (a KEY $ak)-[r:R]->(b) CREATE (r)-[e:S KEY $k]->(b)",
            37,
        ),
    ] {
        assert_query_error_at(
            text,
            &[
                param("ak", PropertyValue::String("a".into())),
                param("k", PropertyValue::String("edge".into())),
            ],
            LanguageErrorKind::Syntax,
            position,
        );
    }

    let skip_limit = "MATCH (n) RETURN n SKIP $skip LIMIT $limit";
    successful(
        skip_limit,
        &[
            param("skip", PropertyValue::Integer(0)),
            param("limit", PropertyValue::Integer(0)),
        ],
    );
    let negative_pagination = "MATCH (n) RETURN n SKIP $skip";
    assert_query_error_at(
        negative_pagination,
        &[param("skip", PropertyValue::Integer(-1))],
        LanguageErrorKind::Parameter,
        negative_pagination.find("$skip").unwrap(),
    );
    let negative_limit = "MATCH (n) RETURN n LIMIT $limit";
    assert_query_error_at(
        negative_limit,
        &[param("limit", PropertyValue::Integer(-1))],
        LanguageErrorKind::Parameter,
        negative_limit.find("$limit").unwrap(),
    );
    let id_zero = "MATCH (n ID $id) RETURN n";
    successful(id_zero, &[param("id", PropertyValue::Integer(0))]);
    assert_query_error_at(
        id_zero,
        &[param("id", PropertyValue::Integer(-1))],
        LanguageErrorKind::Parameter,
        id_zero.find("$id").unwrap(),
    );
    let text = "MATCH (n KEY $k) RETURN n";
    for invalid_name in ["1x", "a-b"] {
        let parameter = ParameterEntry {
            name: invalid_name.into(),
            value: PropertyValue::String("k".into()),
            position: 0,
        };
        assert_query_error_at(text, &[parameter], LanguageErrorKind::Parameter, 0);
    }

    let where_terms = (0..64)
        .map(|_| "n.x = $v")
        .collect::<Vec<_>>()
        .join(" AND ");
    let text = format!("MATCH (n) WHERE {where_terms} RETURN n");
    successful(&text, &[param("v", PropertyValue::Integer(1))]);
    let where_terms = (0..65)
        .map(|_| "n.x = $v")
        .collect::<Vec<_>>()
        .join(" AND ");
    let text = format!("MATCH (n) WHERE {where_terms} RETURN n");
    let excess_position = text.match_indices("n.x").nth(64).unwrap().0;
    assert_query_error_at(
        &text,
        &[param("v", PropertyValue::Integer(1))],
        LanguageErrorKind::Limit,
        excess_position,
    );

    let prefix = "MATCH (n) RETURN n ";
    let text = format!("{prefix}{}", "x ".repeat(1_019));
    assert_query_error_at(
        &text,
        &[],
        LanguageErrorKind::Limit,
        text.rfind('x').unwrap(),
    );
}

#[test]
fn planner_detects_duplicates_before_names_are_collapsed_and_checks_scope() {
    let text = "MATCH (n {x:$v, x:$v}) RETURN n";
    let error = query_error(text, &[param("v", PropertyValue::Integer(1))]);
    assert_eq!(error.kind, LanguageErrorKind::Syntax);
    assert_eq!(error.position, text.rfind("x:").unwrap());

    for (text, offending) in [
        ("MATCH (n) WHERE m.x = $v RETURN n", "m"),
        ("MATCH (n) RETURN missing", "missing"),
        ("MATCH (n) RETURN n ORDER BY n", "last:n"),
        ("CREATE (n KEY $k) CREATE (n KEY $k)", "last:n"),
        ("CREATE (a KEY $k) CREATE (a)-[r:R]->(missing)", "missing"),
    ] {
        let parameters = [
            param("v", PropertyValue::Integer(1)),
            param("k", PropertyValue::String("k".into())),
        ];
        let expected_position = if let Some(name) = offending.strip_prefix("last:") {
            text.rfind(name).unwrap()
        } else {
            text.find(offending).unwrap()
        };
        let error = query_error(text, &parameters);
        assert_eq!(error.kind, LanguageErrorKind::Syntax, "{text}: {error}");
        assert_eq!(error.position, expected_position, "{text}: {error}");
    }

    let duplicate_label = "MATCH (n:A:A) RETURN n";
    let error = query_error(duplicate_label, &[]);
    assert_eq!(error.position, duplicate_label.rfind('A').unwrap());
}

#[test]
fn parameters_are_typed_and_missing_unused_or_duplicate_values_are_rejected() {
    let missing_text = "MATCH (n KEY $key) RETURN n";
    let error = query_error(missing_text, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Parameter);
    assert_eq!(error.position, missing_text.find("$key").unwrap());

    let wrong_text = "MATCH (n ID $id) RETURN n";
    let error = query_error(
        wrong_text,
        &[param("id", PropertyValue::String("7".into()))],
    );
    assert_eq!(error.kind, LanguageErrorKind::Parameter);
    assert_eq!(error.position, wrong_text.find("$id").unwrap());

    let error = query_error(
        "MATCH (n) RETURN n",
        &[param("unused", PropertyValue::Boolean(true))],
    );
    assert_eq!(error.kind, LanguageErrorKind::Parameter);
    assert_eq!(error.position, "MATCH (n) RETURN n".len());

    let partially_used_text = "MATCH (a KEY $ak) WHERE a.x = $p RETURN a";
    assert_query_error_at(
        partially_used_text,
        &[
            param("ak", PropertyValue::String("a".into())),
            param("p", PropertyValue::Integer(1)),
            param("q", PropertyValue::Boolean(true)),
        ],
        LanguageErrorKind::Parameter,
        partially_used_text.len(),
    );

    let duplicate = [
        ParameterEntry {
            name: "key".into(),
            value: PropertyValue::String("first".into()),
            position: 11,
        },
        ParameterEntry {
            name: "key".into(),
            value: PropertyValue::String("second".into()),
            position: 29,
        },
    ];
    let error = match plan("MATCH (n KEY $key) RETURN n", &duplicate).unwrap_err() {
        PlanError::Query(error) => error,
        other => panic!("expected duplicate parameter error, got {other:?}"),
    };
    assert_eq!(error.kind, LanguageErrorKind::Parameter);
    assert_eq!(error.position, 29);

    let text = "MATCH (n KEY $key) RETURN n";
    let error = query_error(text, &[param("key", PropertyValue::Integer(3))]);
    assert_eq!(error.kind, LanguageErrorKind::Parameter);
    assert_eq!(error.position, text.find("$key").unwrap());

    let as_of = "MATCH (n) AS OF $t RETURN n";
    assert!(plan(as_of, &[param("t", PropertyValue::Integer(-1))]).is_ok());
    let error = query_error(as_of, &[param("t", PropertyValue::Boolean(false))]);
    assert_eq!(error.kind, LanguageErrorKind::Parameter);
    assert_eq!(error.position, as_of.find("$t").unwrap());
}

#[test]
fn statement_bytes_predicates_depth_and_hops_obey_inclusive_caps() {
    let base = "MATCH (n) RETURN n";
    for size in [16_383, 16_384] {
        let text = format!("{base}{}", " ".repeat(size - base.len()));
        assert!(plan(&text, &[]).is_ok(), "size={size}");
    }
    let too_large = format!("{base}{}", " ".repeat(16_385 - base.len()));
    let error = query_error(&too_large, &[]);
    assert_eq!(error.kind, LanguageErrorKind::Limit);
    assert_eq!(error.position, 16_384);

    for count in [63, 64] {
        assert!(
            plan(
                &limit_map(count),
                &[param("value", PropertyValue::Integer(1))]
            )
            .is_ok(),
            "predicate count={count}"
        );
    }
    let too_many = limit_map(65);
    let error = query_error(&too_many, &[param("value", PropertyValue::Integer(1))]);
    assert_eq!(error.kind, LanguageErrorKind::Limit);
    assert_eq!(error.position, too_many.find("p64").unwrap());

    for depth in [15, 16] {
        let predicate = format!("{}n.x = $v{}", "(".repeat(depth), ")".repeat(depth));
        let text = format!("MATCH (n) WHERE {predicate} RETURN n");
        assert!(plan(&text, &[param("v", PropertyValue::Integer(1))]).is_ok());
    }
    let depth = 17;
    let predicate = format!("{}n.x = $v{}", "(".repeat(depth), ")".repeat(depth));
    let text = format!("MATCH (n) WHERE {predicate} RETURN n");
    let error = query_error(&text, &[param("v", PropertyValue::Integer(1))]);
    assert_eq!(error.kind, LanguageErrorKind::Limit);
    assert_eq!(error.position, text.find("WHERE").unwrap() + 6 + 16);

    for hops in [3, 4] {
        let text = format!("MATCH (a KEY $k)-[:R*1..{hops}]->(b) RETURN b");
        assert!(plan(&text, &[param("k", PropertyValue::String("a".into()))]).is_ok());
    }
}

#[test]
fn parameter_count_name_and_encoded_byte_caps_have_exact_boundaries() {
    for property_count in [62, 63] {
        let props = (0..property_count)
            .map(|index| format!("p{index}:$v{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!("CREATE (n KEY $k {{{props}}})");
        let mut parameters = vec![param("k", PropertyValue::String("k".into()))];
        parameters.extend(
            (0..property_count).map(|index| param(&format!("v{index}"), PropertyValue::Integer(1))),
        );
        assert_eq!(parameters.len(), property_count + 1);
        if parameters.len() <= 64 {
            assert!(
                plan(&text, &parameters).is_ok(),
                "count={}",
                parameters.len()
            );
        }
    }
    let props = (0..64)
        .map(|index| format!("p{index}:$v{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("CREATE (n KEY $k {{{props}}})");
    let mut parameters = vec![param("k", PropertyValue::String("k".into()))];
    parameters.extend((0..64).map(|index| param(&format!("v{index}"), PropertyValue::Integer(1))));
    let error = query_error(&text, &parameters);
    assert_eq!(error.kind, LanguageErrorKind::Limit);
    assert_eq!(error.position, text.len());

    let names = std::iter::once("k".to_owned())
        .chain((0..16).map(|index| format!("p{index}")))
        .collect::<Vec<_>>();
    let properties = (0..16)
        .map(|index| format!("f{index}:${}", names[index + 1]))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("CREATE (n KEY $k {{{properties}}})");
    let overhead = 4 + names
        .iter()
        .map(|name| 2 + name.len() + 1 + 4)
        .sum::<usize>();
    let desired_values = 65_536 - overhead;
    let key_value_len = 1;
    let mut value_lengths = vec![4_096usize; 15];
    let final_length = desired_values - key_value_len - value_lengths.iter().sum::<usize>();
    value_lengths.push(final_length);
    let make_parameters = |extra: usize| {
        let mut values = vec![param("k", PropertyValue::String("k".into()))];
        values.extend(
            names
                .iter()
                .skip(1)
                .zip(value_lengths.iter())
                .enumerate()
                .map(|(index, (name, length))| {
                    let length = length + usize::from(index == 15) * extra;
                    param(name, PropertyValue::String("x".repeat(length)))
                }),
        );
        values
    };
    let mut below_limit = make_parameters(0);
    if let PropertyValue::String(value) = &mut below_limit[16].value {
        value.pop();
    }
    assert!(plan(&text, &below_limit).is_ok());
    assert!(plan(&text, &make_parameters(0)).is_ok());
    let error = query_error(&text, &make_parameters(1));
    assert_eq!(error.kind, LanguageErrorKind::Limit);
    assert_eq!(error.position, text.len());

    let name = "x".repeat(255);
    let text = format!("MATCH (n KEY ${name}) RETURN n");
    assert!(plan(&text, &[param(&name, PropertyValue::String("k".into()))]).is_ok());
    let name = "x".repeat(256);
    let text = format!("MATCH (n KEY ${name}) RETURN n");
    assert!(plan(&text, &[param(&name, PropertyValue::String("k".into()))]).is_ok());
    let name = "x".repeat(257);
    let text = format!("MATCH (n KEY ${name}) RETURN n");
    let error = query_error(&text, &[param(&name, PropertyValue::String("k".into()))]);
    assert_eq!(error.kind, LanguageErrorKind::Limit);
    assert_eq!(error.position, text.len());
}

#[test]
fn parameter_values_are_validated_without_reinterpreting_their_text() {
    let text = "MATCH (n KEY $key) RETURN n";
    let planned = successful(
        text,
        &[param(
            "key",
            PropertyValue::String("`MATCH`); CREATE (x)".into()),
        )],
    );
    assert_eq!(
        planned.parameters["key"],
        PropertyValue::String("`MATCH`); CREATE (x)".into())
    );

    let error = plan(text, &[param("key", PropertyValue::Float(f64::NAN))])
        .expect_err("non-finite property value");
    assert!(matches!(error, PlanError::InvalidMetadata(_)));
}

#[test]
fn repeated_endpoint_binding_and_reserved_model_names_are_preserved() {
    let text = "MATCH (n:Person KEY $a)-[:R]->(n:Person) RETURN n.key, n.id";
    successful(text, &[param("a", PropertyValue::String("a".into()))]);

    let text = "MATCH (n:MATCH {RETURN:$WHERE}) RETURN n.MATCH";
    successful(text, &[param("WHERE", PropertyValue::Integer(1))]);
}

#[test]
fn unicode_offsets_remain_utf8_bytes_and_diagnostics_do_not_echo_inputs() {
    let text = "MATCH (`é`) RETURN missing";
    let error = query_error(text, &[]);
    assert_eq!(error.position, text.find("missing").unwrap());
    assert!(!error.message.contains("missing"));

    let text = "MATCH (n) RETURN $private_value";
    let error = query_error(text, &[]);
    assert!(!error.message.contains("private_value"));
    assert!(!error.to_string().contains(text));
}

#[test]
fn excluded_forms_are_unsupported_at_their_introducers() {
    let cases = [
        ("MATCH (n) RETURN COUNT(n)", "COUNT"),
        ("MATCH (n) WHERE n.x = [$x] RETURN n", "["),
        ("MATCH (n) WHERE n.x = $x + $y RETURN n", "+"),
        ("MATCH (n) // comment\nRETURN n", "//"),
        ("MATCH (n) AS OF $t CREATE (m KEY $k)", "AS"),
        ("MATCH (a KEY $k)-[:R*1..2]->(b) CREATE (x KEY $x)", "*"),
    ];
    for (text, introducer) in cases {
        let parameters = [
            param("k", PropertyValue::String("k".into())),
            param("x", PropertyValue::Integer(1)),
            param("y", PropertyValue::Integer(2)),
            param("t", PropertyValue::Integer(1)),
        ];
        let error = query_error(text, &parameters);
        assert_eq!(
            error.kind,
            LanguageErrorKind::Unsupported,
            "{text}: {error}"
        );
        assert_eq!(
            error.position,
            text.find(introducer).unwrap(),
            "{text}: {error}"
        );
    }
}

#[test]
fn malformed_inputs_never_panic() {
    let atoms = [
        "MATCH", "RETURN", "(", ")", "[", "]", "{", "}", ":", ".", "$", "->", "WHERE", "AND", "OR",
        "NOT", "IN", "CREATE", "MERGE", "SET", "DELETE", "`", "é",
    ];
    for first in atoms {
        for second in atoms {
            let text = format!("{first} {second}");
            let result = std::panic::catch_unwind(|| plan(&text, &[]));
            assert!(result.is_ok(), "parser panicked for {text:?}");
            let outcome = result.unwrap();
            let error = match outcome {
                Err(PlanError::Query(error)) => error,
                other => {
                    panic!("malformed input did not produce a query error: {text:?}: {other:?}")
                }
            };
            assert!(
                error.position <= text.len(),
                "error offset out of bounds for {text:?}: {error}"
            );
        }
    }
}
