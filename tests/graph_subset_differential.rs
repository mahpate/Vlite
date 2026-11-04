//! Mechanical consumer of the independent, hand-authored front-end corpus.

#[allow(dead_code, unused_imports)]
#[path = "../src/property_graph/engine/mod.rs"]
mod engine;

use engine::model::PropertyValue;
use engine::subset::plan::{ParameterEntry, PlanError, plan};
use engine::subset::{
    Assignment, Creation, Mutation, Name, ParameterRef, Predicate, ReturnValue, Statement,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn value_prefix(text: &str) -> (Value, usize) {
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    let value = stream
        .next()
        .expect("JSON value")
        .expect("valid fixture JSON");
    (value, stream.byte_offset())
}

// Retain source order and duplicate root entries without serde's map normalization.
fn object_entries(mut text: &str) -> Vec<(String, &str)> {
    text = text.trim_start();
    assert!(text.starts_with('{'));
    text = &text[1..];
    let mut entries = Vec::new();
    loop {
        text = text.trim_start();
        if text.starts_with('}') {
            break;
        }
        let (key, consumed) = value_prefix(text);
        let key = key.as_str().expect("object key").to_owned();
        text = text[consumed..].trim_start();
        assert!(text.starts_with(':'));
        text = text[1..].trim_start();
        let (_, consumed) = value_prefix(text);
        entries.push((key, &text[..consumed]));
        text = text[consumed..].trim_start();
        if text.starts_with(',') {
            text = &text[1..];
        } else {
            assert!(text.starts_with('}'));
        }
    }
    entries
}

fn expand(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    format!(
        "{}{}{}",
        value["prefix"].as_str().expect("long prefix"),
        value["unit"]
            .as_str()
            .expect("long unit")
            .repeat(usize::try_from(value["count"].as_u64().expect("long count")).unwrap()),
        value["suffix"].as_str().expect("long suffix")
    )
}

fn parameter_value(value: &Value) -> PropertyValue {
    match value["tag"].as_str().expect("typed tag") {
        "String" => PropertyValue::String(expand(&value["value"])),
        "Integer" => PropertyValue::Integer(
            value["value"]
                .as_str()
                .expect("decimal i64 string")
                .parse()
                .expect("i64"),
        ),
        "Float" => PropertyValue::Float(value["value"].as_f64().expect("binary64")),
        "Boolean" => PropertyValue::Boolean(value["value"].as_bool().expect("Boolean")),
        "GeoPoint" => PropertyValue::GeoPoint {
            latitude: value["latitude"].as_f64().expect("latitude"),
            longitude: value["longitude"].as_f64().expect("longitude"),
        },
        tag => panic!("unrepresentable tag was not accounted for: {tag}"),
    }
}

#[derive(Default)]
struct Surface<'a> {
    references: Vec<&'a ParameterRef>,
    declarations: Vec<(&'a Name, &'static str)>,
}

impl<'a> Surface<'a> {
    fn node(&mut self, node: &'a engine::subset::NodePattern) {
        if let Some(variable) = &node.variable {
            self.declarations.push((variable, "node"));
        }
        if let Some(anchor) = &node.anchor {
            self.references.push(&anchor.parameter);
        }
        self.references
            .extend(node.properties.iter().map(|pair| &pair.parameter));
    }

    fn predicate(&mut self, predicate: &'a Predicate) {
        match predicate {
            Predicate::And(items) | Predicate::Or(items) => {
                for item in items {
                    self.predicate(item);
                }
            }
            Predicate::Not(item) => self.predicate(item),
            Predicate::Compare { parameter, .. } => self.references.push(parameter),
            Predicate::IsNull { .. } => {}
        }
    }

    fn creation(&mut self, creation: &'a Creation) {
        match creation {
            Creation::Node(node) => {
                self.declarations.push((&node.variable, "node"));
                self.references.push(&node.key);
                self.references
                    .extend(node.properties.iter().map(|pair| &pair.parameter));
            }
            Creation::Edge(edge) => {
                self.declarations.push((&edge.variable, "rel"));
                self.references.extend(edge.key.iter());
                self.references
                    .extend(edge.properties.iter().map(|pair| &pair.parameter));
            }
        }
    }
}

fn accepted(statement: &Statement) -> Value {
    let mut surface = Surface::default();
    if let Some(pattern) = &statement.pattern {
        if let Some(path) = &pattern.path_variable {
            surface.declarations.push((path, "path"));
        }
        surface.node(&pattern.start);
        if let Some((relationship, end)) = &pattern.relationship {
            if let Some(variable) = &relationship.variable {
                surface.declarations.push((variable, "rel"));
            }
            if let Some(anchor) = &relationship.anchor {
                surface.references.push(&anchor.parameter);
            }
            surface
                .references
                .extend(relationship.properties.iter().map(|pair| &pair.parameter));
            surface.node(end);
        }
    }
    surface.references.extend(statement.as_of.iter());
    if let Some(predicate) = &statement.predicate {
        surface.predicate(predicate);
    }
    for mutation in &statement.mutations {
        match mutation {
            Mutation::Create { creations, .. } => {
                for creation in creations {
                    surface.creation(creation);
                }
            }
            Mutation::Merge { creation, .. } => surface.creation(creation),
            Mutation::Set { assignments, .. } => {
                for assignment in assignments {
                    if let Assignment::Property { parameter, .. } = assignment {
                        surface.references.push(parameter);
                    }
                }
            }
            Mutation::Remove { .. } | Mutation::Delete { .. } => {}
        }
    }
    let mut columns = Vec::new();
    if let Some(returning) = &statement.returning {
        for item in &returning.items {
            columns.push(if let Some(alias) = &item.alias {
                alias.text.clone()
            } else {
                match &item.value {
                    ReturnValue::Variable(variable) => variable.text.clone(),
                    ReturnValue::Field(field) => {
                        format!("{}.{}", field.variable.text, field.field.text)
                    }
                }
            });
        }
        surface.references.extend(returning.skip.iter());
        surface.references.extend(returning.limit.iter());
    }
    surface
        .references
        .sort_by_key(|reference| reference.position);
    surface.declarations.sort_by_key(|(name, _)| name.position);
    let mut seen = BTreeSet::new();
    let bindings: Vec<Value> = surface
        .declarations
        .into_iter()
        .filter(|(name, _)| seen.insert(name.text.as_str()))
        .map(|(name, kind)| json!([name.text, kind]))
        .collect();
    let references: Vec<Value> = surface
        .references
        .into_iter()
        .map(|reference| json!([reference.name, reference.position]))
        .collect();
    json!({
        "outcome": "accept",
        "kind": if statement.mutations.is_empty() { "read" } else { "write" },
        "columns": columns,
        "references": references,
        "bindings": bindings
    })
}

#[derive(Default)]
struct Counts {
    total: usize,
    compared: usize,
    matched: usize,
    mismatched: usize,
    not_representable: usize,
}

impl Counts {
    fn json(&self) -> Value {
        json!({
            "total": self.total, "compared": self.compared, "matched": self.matched,
            "mismatched": self.mismatched, "not_representable": self.not_representable
        })
    }
}

#[test]
fn independent_front_end_corpus() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("graph_subset")
        .join("parse-cases.json");
    let source = std::fs::read_to_string(path).expect("independent corpus");
    let fields = object_entries(&source);
    let mut cases = fields
        .iter()
        .find(|(key, _)| key == "cases")
        .unwrap()
        .1
        .trim_start();
    assert!(cases.starts_with('['));
    cases = &cases[1..];
    let mut counts = Counts::default();
    let mut groups = BTreeMap::<String, Counts>::new();
    loop {
        cases = cases.trim_start();
        if cases.starts_with(']') {
            break;
        }
        let (case, consumed) = value_prefix(cases);
        let raw_case = &cases[..consumed];
        cases = cases[consumed..].trim_start();
        if cases.starts_with(',') {
            cases = &cases[1..];
        } else {
            assert!(cases.starts_with(']'));
        }
        let id = case["id"].as_str().expect("id");
        let group = case["group"].as_str().expect("group");
        let group_counts = groups.entry(group.to_owned()).or_default();
        counts.total += 1;
        group_counts.total += 1;
        let expected = &case["expect"];
        let kind = expected["error"]["kind"].as_str();
        if matches!(
            kind,
            Some("InvalidMetadata" | "InvalidQuery" | "GraphLimitExceedsCeiling")
        ) {
            counts.not_representable += 1;
            group_counts.not_representable += 1;
            println!(
                "NOT_REPRESENTABLE {}",
                json!({
                    "id": id, "group": group,
                    "reason": format!("section 7 {kind:?} envelope/execution outcome is outside plan(text, parameters)")
                })
            );
            continue;
        }
        let text = expand(&case["text"]);
        let case_fields = object_entries(raw_case);
        let raw_params = case_fields.iter().find(|(key, _)| key == "params");
        let params_source = if let Some((_, raw)) = raw_params {
            *raw
        } else {
            case["params_raw"].as_str().expect("raw parameter envelope")
        };
        let parameters: Vec<ParameterEntry> = object_entries(params_source)
            .into_iter()
            .map(|(name, raw)| ParameterEntry {
                name,
                value: parameter_value(&serde_json::from_str::<Value>(raw).expect("typed value")),
                // The typed API cannot supply JSON envelope coordinates; zero is its origin.
                position: 0,
            })
            .collect();
        // plan has no caller-limit argument. Envelope outcomes above are not fabricated here.
        let max_hops = case.get("max_hops").and_then(Value::as_u64).unwrap_or(2);
        let actual = match plan(&text, &parameters) {
            Ok(planned) => accepted(&planned.statement),
            Err(PlanError::Query(error)) => {
                let mut diagnostic = json!({
                    "kind": format!("{:?}", error.kind), "offset": error.position
                });
                if expected["error"].get("reason").is_some() {
                    diagnostic["reason"] = json!(error.message);
                }
                json!({"outcome": "reject", "error": diagnostic})
            }
            Err(PlanError::InvalidMetadata(error)) => json!({
                "outcome": "reject", "error": {"kind": "InvalidMetadata", "detail": format!("{error:?}")}
            }),
        };
        counts.compared += 1;
        group_counts.compared += 1;
        if actual == *expected {
            counts.matched += 1;
            group_counts.matched += 1;
        } else {
            counts.mismatched += 1;
            group_counts.mismatched += 1;
            println!(
                "MISMATCH {}",
                json!({
                    "id": id, "group": group, "oracle": expected, "rust": actual, "max_hops": max_hops
                })
            );
        }
    }
    for (group, counts) in groups {
        println!("GROUP {}", json!({"group": group, "counts": counts.json()}));
    }
    println!("TOTAL {}", counts.json());
    assert_eq!(
        counts.mismatched, 0,
        "all corpus outcomes collected; disagreements above"
    );
}
