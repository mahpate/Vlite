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
    Anchor, AnchorKind, Creation, CreationNode, Direction, FieldTarget, Mutation, Name,
    NodePattern, ParameterRef, Pattern, Plan, Predicate, ReturnClause, ReturnItem, ReturnValue,
    Statement,
};

fn name(text: &str, position: usize) -> Name {
    Name {
        text: text.into(),
        position,
        quoted: false,
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
        }
    }
}
