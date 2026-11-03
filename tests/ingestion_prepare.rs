//! Golden conformance tests for the crate-private ingestion preparation slice.

use std::collections::BTreeMap;

use serde_json::{Number, json};

#[allow(dead_code, unused_imports)]
#[path = "../src/ingestion/prepare/mod.rs"]
mod prepare;

#[derive(Clone, Debug)]
enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Number(a), Self::Number(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => a == b,
            (Self::Object(a), Self::Object(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(key, value)| {
                        b.iter()
                            .find(|(other_key, _)| other_key == key)
                            .is_some_and(|(_, other_value)| value == other_value)
                    })
            }
            _ => false,
        }
    }
}

impl Json {
    fn from_value(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(value) => Self::Number(value),
            serde_json::Value::String(value) => Self::String(value),
            serde_json::Value::Array(values) => {
                Self::Array(values.into_iter().map(Self::from_value).collect())
            }
            serde_json::Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from_value(value)))
                    .collect(),
            ),
        }
    }

    fn first_difference(actual: &Json, expected: &Json, path: &str) -> Option<String> {
        match (actual, expected) {
            (Json::Object(actual), Json::Object(expected)) => {
                if actual.len() != expected.len() {
                    return Some(format!(
                        "{path}: object lengths {} != {}",
                        actual.len(),
                        expected.len()
                    ));
                }
                for (key, expected_value) in expected {
                    let Some((_, actual_value)) =
                        actual.iter().find(|(actual_key, _)| actual_key == key)
                    else {
                        return Some(format!("{path}.{key}: missing from actual"));
                    };
                    if let Some(message) = Self::first_difference(
                        actual_value,
                        expected_value,
                        &format!("{path}.{key}"),
                    ) {
                        return Some(message);
                    }
                }
                None
            }
            (Json::Array(actual), Json::Array(expected)) => {
                if actual.len() != expected.len() {
                    return Some(format!(
                        "{path}: array lengths {} != {}",
                        actual.len(),
                        expected.len()
                    ));
                }
                actual
                    .iter()
                    .zip(expected)
                    .enumerate()
                    .find_map(|(index, (actual, expected))| {
                        Self::first_difference(actual, expected, &format!("{path}[{index}]"))
                    })
            }
            _ if actual == expected => None,
            _ => Some(format!(
                "{path}: actual {} != expected {}",
                Self::brief(actual),
                Self::brief(expected)
            )),
        }
    }

    fn brief(value: &Json) -> String {
        match value {
            Json::String(text) if text.len() > 80 => {
                format!(
                    "{:?}… ({} bytes)",
                    text.chars().take(80).collect::<String>(),
                    text.len()
                )
            }
            _ => format!("{value:?}"),
        }
    }

    fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(values) => values
                .iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(value) => value.as_i64().or_else(|| value.as_u64()?.try_into().ok()),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    fn object(entries: impl IntoIterator<Item = (impl Into<String>, Json)>) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }
}

fn jvalue(value: serde_json::Value) -> Json {
    Json::from_value(value)
}

fn text(value: &Json, where_: &str) -> String {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{where_} must be a string"))
        .to_owned()
}

fn number(value: &Json, where_: &str) -> usize {
    usize::try_from(
        value
            .as_i64()
            .unwrap_or_else(|| panic!("{where_} must be a nonnegative integer")),
    )
    .unwrap_or_else(|_| panic!("{where_} must be a nonnegative integer"))
}

fn byte_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    result
}

fn from_hex(value: &str) -> Vec<u8> {
    assert_eq!(value.len() % 2, 0, "golden fixture hex has odd length");
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => panic!("invalid hex in golden fixture"),
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect()
}

fn fixture_source(value: &Json) -> Vec<u8> {
    match value {
        Json::String(text) => text.as_bytes().to_vec(),
        Json::Object(fields) if fields.len() == 1 && value.get("hex").is_some() => {
            from_hex(text(value.get("hex").unwrap(), "source.hex").as_str())
        }
        Json::Object(fields) if fields.len() == 1 && value.get("pieces").is_some() => {
            let pieces = value.get("pieces").unwrap().as_array().unwrap();
            let mut result = Vec::new();
            for piece in pieces {
                if let Some(unit) = piece.get("utf8") {
                    result.extend_from_slice(text(unit, "source piece").as_bytes());
                } else if let Some(unit) = piece.get("repeat_utf8") {
                    let bytes = text(unit, "repeat_utf8");
                    let count = number(piece.get("count").unwrap(), "repeat count");
                    result.reserve(bytes.len().saturating_mul(count));
                    for _ in 0..count {
                        result.extend_from_slice(bytes.as_bytes());
                    }
                } else if let Some(unit) = piece.get("repeat_hex") {
                    let bytes = from_hex(text(unit, "repeat_hex").as_str());
                    let count = number(piece.get("count").unwrap(), "repeat count");
                    result.reserve(bytes.len().saturating_mul(count));
                    for _ in 0..count {
                        result.extend_from_slice(&bytes);
                    }
                } else {
                    panic!("unknown source fixture piece");
                }
            }
            result
        }
        _ => panic!("unknown source fixture"),
    }
}

fn expand_pattern(value: &Json, variables: &BTreeMap<String, i64>) -> Json {
    match value {
        Json::Array(values) => Json::Array(
            values
                .iter()
                .map(|value| expand_pattern(value, variables))
                .collect(),
        ),
        Json::Object(fields) if fields.len() == 1 && value.get("$text").is_some() => {
            let pair = value.get("$text").unwrap().as_array().unwrap();
            let unit = text(&pair[0], "$text unit");
            let count = number(&pair[1], "$text count");
            Json::String(unit.repeat(count))
        }
        Json::Object(fields) if fields.len() == 1 && value.get("$integer").is_some() => {
            let pair = value.get("$integer").unwrap().as_array().unwrap();
            let name = text(&pair[0], "$integer variable");
            let base = variables
                .get(&name)
                .unwrap_or_else(|| panic!("missing golden variable {name}"));
            Json::Number(Number::from(base + pair[1].as_i64().unwrap()))
        }
        Json::Object(fields) if fields.len() == 1 && value.get("$string").is_some() => {
            let pair = value.get("$string").unwrap().as_array().unwrap();
            let name = text(&pair[0], "$string variable");
            let base = variables
                .get(&name)
                .unwrap_or_else(|| panic!("missing golden variable {name}"));
            let template = text(&pair[1], "$string template");
            let value = template.replace("{:03d}", &format!("{base:03}"));
            Json::String(value.replace("{}", &base.to_string()))
        }
        Json::Object(fields) if fields.len() == 1 && value.get("$concat").is_some() => {
            let parts = expand_pattern(value.get("$concat").unwrap(), variables)
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|part| part.as_array().unwrap().iter().cloned())
                .collect();
            Json::Array(parts)
        }
        Json::Object(fields) if fields.len() == 1 && value.get("$rows").is_some() => {
            let rows = value.get("$rows").unwrap();
            let ranges = rows.get("ranges").unwrap();
            let ordered_ranges: Vec<(String, usize, usize)> = match ranges {
                Json::Object(entries) => {
                    let mut ordered = entries.iter().collect::<Vec<_>>();
                    ordered.sort_by_key(|(name, _)| match name.as_str() {
                        "s" => (0, name.as_str()),
                        "o" => (1, name.as_str()),
                        _ => (2, name.as_str()),
                    });
                    ordered
                        .into_iter()
                        .map(|(name, bounds)| {
                            let bounds = bounds.as_array().unwrap();
                            (
                                name.clone(),
                                number(&bounds[0], "row start"),
                                number(&bounds[1], "row end"),
                            )
                        })
                        .collect()
                }
                _ => panic!("golden rows ranges must be an object"),
            };
            let mut output = Vec::new();
            expand_rows(
                &ordered_ranges,
                0,
                &mut variables.clone(),
                rows.get("value").unwrap(),
                &mut output,
            );
            Json::Array(output)
        }
        Json::Object(fields) => Json::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), expand_pattern(value, variables)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn expand_rows(
    ranges: &[(String, usize, usize)],
    depth: usize,
    variables: &mut BTreeMap<String, i64>,
    template: &Json,
    output: &mut Vec<Json>,
) {
    if depth == ranges.len() {
        output.push(expand_pattern(template, variables));
        return;
    }
    let (name, start, end) = &ranges[depth];
    for value in *start..*end {
        variables.insert(name.clone(), value as i64);
        expand_rows(ranges, depth + 1, variables, template, output);
    }
}

fn expand_expected(case: &Json) -> Json {
    let raw = fixture_source(case.get("source").unwrap());
    let mut expected = expand_pattern(case.get("expected").unwrap(), &BTreeMap::new());
    if let Some(pattern) = expected.get("chunks_repeat").cloned() {
        let count = number(pattern.get("count").unwrap(), "chunks_repeat.count");
        let length = number(pattern.get("length").unwrap(), "chunks_repeat.length");
        let stride = number(pattern.get("stride").unwrap(), "chunks_repeat.stride");
        let hash = text(pattern.get("sha256").unwrap(), "chunks_repeat.sha256");
        let chunks = (0..count)
            .map(|index| {
                let start = index * stride;
                let end = start + length;
                chunk_json(index, start, end, &raw, hash.clone())
            })
            .collect::<Vec<_>>();
        if let Json::Object(ref mut fields) = expected {
            fields.retain(|(key, _)| key != "chunks_repeat");
            fields.push(("chunks".to_owned(), Json::Array(chunks)));
        }
    }
    if let Json::Object(fields) = &mut expected
        && let Some(Json::Array(chunks)) = fields
            .iter_mut()
            .find(|(key, _)| key == "chunks")
            .map(|(_, value)| value)
    {
        for chunk in chunks {
            let start = number(chunk.get("start").unwrap(), "chunk.start");
            let end = number(chunk.get("end").unwrap(), "chunk.end");
            let bytes = &raw[start..end];
            let source_text = std::str::from_utf8(bytes).expect("golden chunk must be UTF-8");
            if let Json::Object(properties) = chunk {
                if !properties.iter().any(|(key, _)| key == "text") {
                    properties.push(("text".to_owned(), Json::String(source_text.to_owned())));
                }
                if !properties.iter().any(|(key, _)| key == "text_hex") {
                    properties.push(("text_hex".to_owned(), Json::String(byte_hex(bytes))));
                }
            }
        }
    }
    expected
}

fn chunk_json(index: usize, start: usize, end: usize, raw: &[u8], sha256: String) -> Json {
    let bytes = &raw[start..end];
    Json::object([
        ("ordinal", Json::Number(Number::from(index))),
        ("start", Json::Number(Number::from(start))),
        ("end", Json::Number(Number::from(end))),
        ("sha256", Json::String(sha256)),
        (
            "text",
            Json::String(std::str::from_utf8(bytes).unwrap().to_owned()),
        ),
        ("text_hex", Json::String(byte_hex(bytes))),
    ])
}

fn err_json(error: prepare::PreparationError) -> Json {
    Json::object([("error", Json::String(error.category().to_owned()))])
}

fn source_identity(value: &Json) -> prepare::SourceIdentity {
    prepare::SourceIdentity {
        tenant: text(value.get("tenant").unwrap(), "identity.tenant"),
        source_id: text(value.get("source_id").unwrap(), "identity.source_id"),
        uri: match value.get("uri") {
            None | Some(Json::Null) => None,
            Some(value) => Some(text(value, "identity.uri")),
        },
    }
}

fn source_json(source: prepare::Source) -> Json {
    Json::object([
        ("tenant", Json::String(source.tenant)),
        ("source_id", Json::String(source.source_id)),
        ("uri", source.uri.map_or(Json::Null, Json::String)),
        (
            "byte_length",
            Json::Number(Number::from(source.byte_length)),
        ),
    ])
}

fn chunks_json(chunks: &[prepare::Chunk]) -> Json {
    Json::Array(
        chunks
            .iter()
            .map(|chunk| {
                Json::object([
                    ("ordinal", Json::Number(Number::from(chunk.ordinal))),
                    ("start", Json::Number(Number::from(chunk.start))),
                    ("end", Json::Number(Number::from(chunk.end))),
                    ("text", Json::String(chunk.text.clone())),
                    ("text_hex", Json::String(chunk.text_hex.clone())),
                    ("sha256", Json::String(chunk.sha256.clone())),
                ])
            })
            .collect(),
    )
}

fn chunk_spans_json(chunks: &[prepare::Chunk]) -> Json {
    Json::Array(
        chunks
            .iter()
            .map(|chunk| {
                Json::object([
                    ("ordinal", Json::Number(Number::from(chunk.ordinal))),
                    ("start", Json::Number(Number::from(chunk.start))),
                    ("end", Json::Number(Number::from(chunk.end))),
                    ("sha256", Json::String(chunk.sha256.clone())),
                ])
            })
            .collect(),
    )
}

fn chunk_strategy(value: &Json) -> Result<prepare::ChunkStrategy, prepare::PreparationError> {
    prepare::ChunkStrategy::parse(value.as_str().unwrap_or_default())
}

fn attributes(value: &Json) -> prepare::Attributes {
    let Json::Object(fields) = value else {
        panic!("attributes must be an object");
    };
    fields
        .iter()
        .map(|(key, value)| (key.clone(), attribute(value)))
        .collect()
}

fn attribute(value: &Json) -> prepare::Attribute {
    match value {
        Json::Null => prepare::Attribute::Null,
        Json::Bool(value) => prepare::Attribute::Bool(*value),
        Json::Number(value) => {
            if let Some(value) = value.as_i64() {
                prepare::Attribute::Integer(value)
            } else if let Some(value) = value.as_u64() {
                prepare::Attribute::Unsigned(value)
            } else {
                prepare::Attribute::Float(value.as_f64().unwrap())
            }
        }
        Json::String(value) => prepare::Attribute::String(value.clone()),
        _ => panic!("attribute values must be scalar"),
    }
}

fn attribute_json(value: &prepare::Attribute) -> serde_json::Value {
    match value {
        prepare::Attribute::Null => serde_json::Value::Null,
        prepare::Attribute::Bool(value) => json!(value),
        prepare::Attribute::Integer(value) => json!(value),
        prepare::Attribute::Unsigned(value) => json!(value),
        prepare::Attribute::Float(value) => json!(if *value == 0.0 { 0.0 } else { *value }),
        prepare::Attribute::String(value) => json!(value),
    }
}

fn attributes_value(attributes: &prepare::Attributes) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for (key, value) in attributes {
        object.insert(key.clone(), attribute_json(value));
    }
    serde_json::Value::Object(object)
}

fn confidence(value: Option<&Json>) -> Option<f64> {
    let value = value?;
    if matches!(value, Json::Null) {
        return None;
    }
    if let Some(marker) = value.get("nonfinite").and_then(Json::as_str) {
        return Some(match marker {
            "nan" => f64::NAN,
            "infinity" => f64::INFINITY,
            "-infinity" => f64::NEG_INFINITY,
            _ => f64::NAN,
        });
    }
    match value {
        Json::Number(number) => number.as_f64(),
        _ => Some(f64::NAN),
    }
}

fn span(value: &Json) -> [usize; 2] {
    let values = value.as_array().expect("span must be an array");
    [
        number(&values[0], "span.start"),
        number(&values[1], "span.end"),
    ]
}

fn entity_observation(
    value: &Json,
) -> Result<prepare::EntityObservation, prepare::PreparationError> {
    Ok(prepare::EntityObservation {
        local_id: u32::try_from(number(value.get("local_id").unwrap(), "local_id"))
            .expect("golden local id fits u32"),
        kind: text(value.get("kind").unwrap(), "kind"),
        key: text(value.get("key").unwrap(), "key"),
        normalized_key: String::new(),
        surface: text(value.get("surface").unwrap(), "surface"),
        span: span(value.get("span").unwrap()),
        attributes: attributes(value.get("attributes").unwrap()),
        confidence: confidence(value.get("confidence")),
    })
}

fn relation_observation(
    value: &Json,
) -> Result<prepare::RelationObservation, prepare::PreparationError> {
    Ok(prepare::RelationObservation {
        subject_local_id: u32::try_from(number(
            value.get("subject_local_id").unwrap(),
            "subject_local_id",
        ))
        .expect("golden local id fits u32"),
        predicate: text(value.get("predicate").unwrap(), "predicate"),
        object_local_id: u32::try_from(number(
            value.get("object_local_id").unwrap(),
            "object_local_id",
        ))
        .expect("golden local id fits u32"),
        span: span(value.get("span").unwrap()),
        attributes: attributes(value.get("attributes").unwrap()),
        confidence: confidence(value.get("confidence")),
    })
}

fn observations(value: &Json, is_entity: bool) -> Result<Vec<Json>, prepare::PreparationError> {
    if let Some(repeat) = value.get("repeat") {
        let count = number(repeat, "repeat");
        let template = value.get("value").unwrap();
        let start = value
            .get("local_id_start")
            .or_else(|| template.get("local_id"))
            .map(|item| number(item, "local_id"))
            .unwrap_or(0);
        return Ok((0..count)
            .map(|index| {
                let mut item = template.clone();
                if is_entity && let Json::Object(fields) = &mut item {
                    if let Some((_, id)) = fields.iter_mut().find(|(key, _)| key == "local_id") {
                        *id = Json::Number(Number::from(start + index));
                    } else {
                        fields.push((
                            "local_id".to_owned(),
                            Json::Number(Number::from(start + index)),
                        ));
                    }
                }
                item
            })
            .collect());
    }
    Ok(value
        .as_array()
        .unwrap_or_else(|| panic!("expected array observations, got {value:?}"))
        .to_vec())
}

fn parse_rules(value: &Json) -> (Vec<prepare::EntityRule>, Vec<prepare::RelationRule>) {
    let entity_rules = value
        .get("entity_rules")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| prepare::EntityRule {
            rule_id: text(rule.get("rule_id").unwrap(), "rule_id"),
            literal: text(rule.get("literal").unwrap(), "literal"),
            kind: text(rule.get("kind").unwrap(), "kind"),
            key: text(rule.get("key").unwrap(), "key"),
        })
        .collect();
    let relation_rules = value
        .get("relation_rules")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| prepare::RelationRule {
            rule_id: text(rule.get("rule_id").unwrap(), "rule_id"),
            predicate: text(rule.get("predicate").unwrap(), "predicate"),
            subject_kind: text(rule.get("subject_kind").unwrap(), "subject_kind"),
            object_kind: text(rule.get("object_kind").unwrap(), "object_kind"),
        })
        .collect();
    (entity_rules, relation_rules)
}

fn extraction_json(extraction: &prepare::Extraction, dictionary: bool) -> Json {
    let entities: Vec<_> = extraction
        .entities
        .iter()
        .map(|entity| {
            let mut result = vec![
                ("local_id".to_owned(), json!(entity.local_id)),
                ("kind".to_owned(), json!(entity.kind)),
                (
                    if dictionary { "key" } else { "normalized_key" }.to_owned(),
                    json!(if dictionary {
                        entity.key.as_str()
                    } else {
                        entity.normalized_key.as_str()
                    }),
                ),
                ("surface".to_owned(), json!(entity.surface)),
                ("span".to_owned(), json!(entity.span)),
                (
                    "attributes".to_owned(),
                    attributes_value(&entity.attributes),
                ),
                (
                    "confidence".to_owned(),
                    entity
                        .confidence
                        .map_or(serde_json::Value::Null, |v| json!(v)),
                ),
            ];
            result.shrink_to_fit();
            jvalue(serde_json::Value::Object(result.into_iter().collect()))
        })
        .collect();
    let relations: Vec<_> = extraction
        .relations
        .iter()
        .map(|relation| {
            jvalue(json!({
                "subject_local_id": relation.subject_local_id,
                "predicate": relation.predicate,
                "object_local_id": relation.object_local_id,
                "span": relation.span,
                "attributes": attributes_value(&relation.attributes),
                "confidence": relation.confidence
            }))
        })
        .collect();
    Json::object([
        ("entities", Json::Array(entities)),
        ("relations", Json::Array(relations)),
    ])
}

fn stage_extraction(inputs: &Json) -> Result<prepare::Extraction, prepare::PreparationError> {
    let source = text(inputs.get("source").unwrap(), "extraction.source");
    let entities = observations(inputs.get("entities").unwrap(), true)?
        .iter()
        .map(entity_observation)
        .collect::<Result<Vec<_>, _>>()?;
    let relations = observations(inputs.get("relations").unwrap(), false)?
        .iter()
        .map(relation_observation)
        .collect::<Result<Vec<_>, _>>()?;
    prepare::validate_extraction(&source, entities, relations)
}

fn entity_identity_json(identity: &prepare::EntityIdentity) -> serde_json::Value {
    json!({
        "binding": identity.binding,
        "tenant": identity.tenant,
        "kind": identity.kind,
        "normalized_key": identity.normalized_key
    })
}

fn resolution_json(resolution: &prepare::Resolution) -> Json {
    let entities = resolution
        .entities
        .iter()
        .map(entity_identity_json)
        .collect::<Vec<_>>();
    let mentions = resolution
        .mentions
        .iter()
        .map(|mention| {
            json!({
                "chunk": mention.chunk,
                "entity": entity_identity_json(&mention.entity),
                "source_span": mention.source_span,
                "chunk_span": mention.chunk_span,
                "surface": mention.surface,
                "attributes": attributes_value(&mention.attributes),
                "confidence": mention.confidence
            })
        })
        .collect::<Vec<_>>();
    let relations = resolution
        .relations
        .iter()
        .map(|relation| {
            json!({
                "chunk": relation.chunk,
                "subject": entity_identity_json(&relation.subject),
                "predicate": relation.predicate,
                "object": entity_identity_json(&relation.object),
                "source_span": relation.source_span,
                "chunk_span": relation.chunk_span,
                "attributes": attributes_value(&relation.attributes),
                "confidence": relation.confidence
            })
        })
        .collect::<Vec<_>>();
    Json::object([
        ("entities", jvalue(json!(entities))),
        ("mentions", jvalue(json!(mentions))),
        ("relations", jvalue(json!(relations))),
    ])
}

fn full_case(case: &Json) -> Result<Json, prepare::PreparationError> {
    let raw = fixture_source(case.get("source").unwrap());
    let identity = source_identity(case.get("source_identity").unwrap());
    let source = prepare::validate_source(&raw, &identity)?;
    let strategy = chunk_strategy(case.get("strategy").unwrap())?;
    let chunks = prepare::split_chunks(
        &raw,
        strategy,
        number(case.get("chunk_bytes").unwrap(), "chunk_bytes"),
        number(case.get("overlap_bytes").unwrap(), "overlap_bytes"),
    )?;
    let binding = text(case.get("binding").unwrap(), "binding");
    prepare::validate_binding(&binding)?;
    let (entity_rules, relation_rules) = parse_rules(case);
    let expanded_fixed = case
        .get("extractions")
        .map(|value| expand_pattern(value, &BTreeMap::new()));
    let fixed = expanded_fixed.as_ref().and_then(Json::as_array);
    let mut fixed_by_chunk = BTreeMap::new();
    if let Some(fixed) = fixed {
        for extraction in fixed {
            let ordinal = number(extraction.get("chunk").unwrap(), "extraction.chunk");
            assert!(ordinal < chunks.len(), "fixture references a missing chunk");
            assert!(
                !fixed_by_chunk.contains_key(&ordinal),
                "fixture repeats a chunk extraction"
            );
            let entities = observations(extraction.get("entities").unwrap(), true)?
                .iter()
                .map(entity_observation)
                .collect::<Result<Vec<_>, _>>()?;
            let relations = observations(extraction.get("relations").unwrap(), false)?
                .iter()
                .map(relation_observation)
                .collect::<Result<Vec<_>, _>>()?;
            fixed_by_chunk.insert(ordinal, (entities, relations));
        }
    }
    let mut extractions = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let extracted = if fixed.is_some() {
            let (entities, relations) = fixed_by_chunk
                .remove(&(chunk.ordinal as usize))
                .unwrap_or_default();
            prepare::validate_extraction(&chunk.text, entities, relations)?
        } else {
            let candidate =
                prepare::dictionary_extract(&chunk.text, &entity_rules, &relation_rules)?;
            prepare::validate_extraction(&chunk.text, candidate.entities, candidate.relations)?
        };
        extractions.push(extracted);
    }
    let resolved = prepare::resolve(&binding, &source.tenant, &chunks, &extractions)?;
    Ok(Json::object([
        ("source", source_json(source)),
        ("chunks", chunks_json(&chunks)),
        (
            "entities",
            jvalue(json!(
                resolved
                    .entities
                    .iter()
                    .map(entity_identity_json)
                    .collect::<Vec<_>>()
            )),
        ),
        (
            "mentions",
            resolution_json(&resolved).get("mentions").unwrap().clone(),
        ),
        (
            "relations",
            resolution_json(&resolved).get("relations").unwrap().clone(),
        ),
    ]))
}

fn stage_check(check: &Json) -> Result<Json, prepare::PreparationError> {
    let operation = text(check.get("operation").unwrap(), "operation");
    let inputs = expand_pattern(check.get("inputs").unwrap(), &BTreeMap::new());
    match operation.as_str() {
        "source" => {
            let raw = fixture_source(inputs.get("source").unwrap());
            let identity = source_identity(inputs.get("identity").unwrap());
            prepare::validate_source(&raw, &identity).map(source_json)
        }
        "chunks" => {
            let raw = fixture_source(inputs.get("source").unwrap());
            let strategy = chunk_strategy(inputs.get("strategy").unwrap())?;
            prepare::split_chunks(
                &raw,
                strategy,
                number(inputs.get("chunk_bytes").unwrap(), "chunk_bytes"),
                number(inputs.get("overlap_bytes").unwrap(), "overlap_bytes"),
            )
            .map(|chunks| Json::object([("chunks", chunk_spans_json(&chunks))]))
        }
        "key" => prepare::normalize_key(&text(inputs.get("key").unwrap(), "key")).map(Json::String),
        "dictionary" => {
            let raw = fixture_source(inputs.get("source").unwrap());
            let source = std::str::from_utf8(&raw).expect("dictionary golden source is UTF-8");
            let (entities, relations) = parse_rules(&inputs);
            prepare::dictionary_extract(source, &entities, &relations)
                .map(|output| extraction_json(&output, true))
        }
        "extraction" => stage_extraction(&inputs).map(|output| extraction_json(&output, false)),
        _ => panic!("unexpected golden operation {operation}"),
    }
}

#[test]
fn accepted_independent_golden_suite() {
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ingestion")
        .join("golden.json");
    let fixture_text = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|error| panic!("cannot read golden {}: {error}", fixture_path.display()));
    let golden = serde_json::from_str::<serde_json::Value>(&fixture_text)
        .map(Json::from_value)
        .unwrap_or_else(|error| panic!("invalid golden {}: {error}", fixture_path.display()));
    assert_eq!(
        golden.get("schema").and_then(Json::as_str),
        Some("vlite.ingestion-preparation-golden"),
        "golden schema"
    );
    assert_eq!(
        golden.get("version").and_then(Json::as_i64),
        Some(1),
        "golden version"
    );
    let cases = golden.get("cases").unwrap().as_array().unwrap();
    let checks = golden.get("checks").unwrap().as_array().unwrap();
    assert_eq!(cases.len(), 26, "golden case count");
    assert_eq!(checks.len(), 51, "golden stage-check count");

    let mut executed = 0usize;
    let mut skipped = Vec::new();
    for case in cases {
        let name = text(case.get("name").unwrap(), "case.name");
        let actual = full_case(case).unwrap_or_else(err_json);
        let expected = expand_expected(case);
        assert!(
            actual == expected,
            "golden case {name}: {}",
            Json::first_difference(&actual, &expected, "$")
                .unwrap_or_else(|| "outputs differ".to_owned())
        );
        executed += 1;
    }
    for check in checks {
        let name = text(check.get("name").unwrap(), "check.name");
        if check.get("operation").and_then(Json::as_str) == Some("main_error") {
            skipped.push(name);
            continue;
        }
        let actual = stage_check(check).unwrap_or_else(err_json);
        let expected = expand_pattern(check.get("expected").unwrap(), &BTreeMap::new());
        assert!(
            actual == expected,
            "golden stage check {name}: {}",
            Json::first_difference(&actual, &expected, "$")
                .unwrap_or_else(|| "outputs differ".to_owned())
        );
        executed += 1;
    }
    assert_eq!(
        skipped,
        ["main-error-is-nonzero-with-sanitized-message-not-traceback"],
        "only Python CLI main_error is not executable in the Rust preparation harness"
    );
    assert_eq!(executed, cases.len() + checks.len() - 1);
}

#[test]
fn overlapping_chunks_keep_scalar_boundaries_and_make_progress() {
    let text = ("😀aé").repeat(8_000);
    let raw = text.as_bytes();
    let chunks = prepare::split_chunks(raw, prepare::ChunkStrategy::FixedUtf8V1, 1_024, 256)
        .expect("valid bounded source");
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].start, 0);
    assert_eq!(chunks.last().unwrap().end, raw.len());
    for pair in chunks.windows(2) {
        assert!(pair[1].start > pair[0].start);
        assert!(pair[1].start <= pair[0].end);
        assert!(pair[0].end - pair[1].start <= 256);
        assert!(text.is_char_boundary(pair[0].end));
        assert!(text.is_char_boundary(pair[1].start));
    }
}

fn test_entity(local_id: u32, key: String, span: [usize; 2]) -> prepare::EntityObservation {
    prepare::EntityObservation {
        local_id,
        kind: "K".to_owned(),
        key: key.clone(),
        normalized_key: key,
        surface: "x".to_owned(),
        span,
        attributes: prepare::Attributes::new(),
        confidence: None,
    }
}

fn test_chunk(ordinal: usize, source_offset: usize) -> prepare::Chunk {
    prepare::Chunk {
        ordinal: ordinal as u32,
        start: source_offset,
        end: source_offset + 512,
        text: String::new(),
        text_hex: String::new(),
        sha256: String::new(),
    }
}

fn entity_rule(id: &str, literal: &str, kind: &str, key: &str) -> prepare::EntityRule {
    prepare::EntityRule {
        rule_id: id.into(),
        literal: literal.into(),
        kind: kind.into(),
        key: key.into(),
    }
}

fn relation_rule(id: &str, predicate: &str) -> prepare::RelationRule {
    prepare::RelationRule {
        rule_id: id.into(),
        predicate: predicate.into(),
        subject_kind: "A".into(),
        object_kind: "B".into(),
    }
}

fn test_relation(
    subject: u32,
    predicate: &str,
    object: u32,
    span: [usize; 2],
) -> prepare::RelationObservation {
    prepare::RelationObservation {
        subject_local_id: subject,
        predicate: predicate.into(),
        object_local_id: object,
        span,
        attributes: prepare::Attributes::new(),
        confidence: None,
    }
}

#[test]
fn b1_dictionary_relations_follow_span_then_endpoint_predicate_order() {
    let rules = [
        entity_rule("a", "A", "A", "a"),
        entity_rule("b", "B", "B", "b"),
    ];
    let output = prepare::dictionary_extract("BABA", &rules, &[relation_rule("r", "p")]).unwrap();
    let pairs: Vec<_> = output
        .relations
        .iter()
        .map(|r| (r.subject_local_id, r.object_local_id))
        .collect();
    assert_eq!(pairs, [(1, 0), (3, 0), (1, 2), (3, 2)]);
    let output = prepare::dictionary_extract(
        "AB",
        &rules,
        &[relation_rule("2", "z"), relation_rule("1", "y")],
    )
    .unwrap();
    assert_eq!(
        output
            .relations
            .iter()
            .map(|r| r.predicate.as_str())
            .collect::<Vec<_>>(),
        ["y", "z"]
    );
}

#[test]
fn dictionary_relation_equal_span_orders_subject_local_ids() {
    let rules = [
        entity_rule("a1", "A", "A", "a"),
        entity_rule("a2", "A", "A", "a"),
        entity_rule("b", "B", "B", "b"),
    ];
    let output = prepare::dictionary_extract("AB", &rules, &[relation_rule("r", "p")]).unwrap();
    assert_eq!(
        output
            .relations
            .iter()
            .map(|r| (r.subject_local_id, r.object_local_id))
            .collect::<Vec<_>>(),
        [(0, 2), (1, 2)]
    );
}

#[test]
fn dictionary_relation_equal_span_orders_object_local_ids() {
    let rules = [
        entity_rule("a", "A", "A", "a"),
        entity_rule("b1", "B", "B", "b"),
        entity_rule("b2", "B", "B", "b"),
    ];
    let output = prepare::dictionary_extract("AB", &rules, &[relation_rule("r", "p")]).unwrap();
    assert_eq!(
        output
            .relations
            .iter()
            .map(|r| (r.subject_local_id, r.object_local_id))
            .collect::<Vec<_>>(),
        [(0, 1), (0, 2)]
    );
}

#[test]
fn b2_extraction_rejects_empty_and_nul_kind_predicate() {
    for invalid in ["", "K\0"] {
        let mut entity = test_entity(0, "x".into(), [0, 1]);
        entity.kind = invalid.into();
        assert_eq!(
            prepare::validate_extraction("xx", vec![entity], vec![])
                .unwrap_err()
                .category(),
            "entity_kind"
        );
        assert_eq!(
            prepare::validate_extraction(
                "xx",
                vec![test_entity(0, "x".into(), [0, 1])],
                vec![test_relation(0, invalid, 0, [0, 1])]
            )
            .unwrap_err()
            .category(),
            "predicate"
        );
    }
}

#[test]
fn b3_dictionary_validates_all_rules_eagerly() {
    let valid = entity_rule("e", "A", "A", "a");
    for invalid in ["", "bad\0"] {
        for field in ["rule_id", "kind", "key"] {
            let mut rule = valid.clone();
            match field {
                "rule_id" => rule.rule_id = invalid.into(),
                "kind" => rule.kind = invalid.into(),
                _ => rule.key = invalid.into(),
            }
            let label = "entity_rules";
            assert_eq!(
                prepare::dictionary_extract("unmatched", &[rule], &[])
                    .unwrap_err()
                    .category(),
                label,
                "{field}"
            );
        }
        for field in ["rule_id", "predicate", "subject_kind", "object_kind"] {
            let mut rule = relation_rule("r", "p");
            match field {
                "rule_id" => rule.rule_id = invalid.into(),
                "predicate" => rule.predicate = invalid.into(),
                "subject_kind" => rule.subject_kind = invalid.into(),
                _ => rule.object_kind = invalid.into(),
            }
            let label = "relation_rules";
            assert_eq!(
                prepare::dictionary_extract("unmatched", &[], &[rule])
                    .unwrap_err()
                    .category(),
                label,
                "{field}"
            );
        }
    }
    let mut empty_literal = valid.clone();
    empty_literal.literal.clear();
    assert_eq!(
        prepare::dictionary_extract("A", &[empty_literal], &[])
            .unwrap_err()
            .category(),
        "entity_rules"
    );
    assert_eq!(
        prepare::dictionary_extract("unmatched", &[valid.clone(), valid.clone()], &[])
            .unwrap_err()
            .category(),
        "entity_rule_duplicate"
    );
    let relation = relation_rule("r", "p");
    assert_eq!(
        prepare::dictionary_extract("unmatched", &[], &[relation.clone(), relation])
            .unwrap_err()
            .category(),
        "relation_rule_duplicate"
    );
    for (key, label) in [
        ("   ".to_owned(), "entity_key_empty"),
        ("x".repeat(257), "entity_key_too_long"),
    ] {
        let mut rule = valid.clone();
        rule.key = key;
        assert_eq!(
            prepare::dictionary_extract("unmatched", &[rule], &[])
                .unwrap_err()
                .category(),
            label
        );
    }
}
#[test]
fn document_resolution_ceilings_accept_the_limit_and_reject_one_more() {
    let mut entity_chunks = Vec::new();
    let mut entity_extractions = Vec::new();
    for ordinal in 0..16 {
        let observations = (0..256)
            .map(|index| test_entity(index, format!("entity-{ordinal}-{index}"), [0, 1]))
            .collect();
        entity_chunks.push(test_chunk(ordinal, ordinal * 512));
        entity_extractions.push(prepare::Extraction {
            entities: observations,
            relations: Vec::new(),
        });
    }

    let exact_entities = prepare::resolve("binding", "tenant", &entity_chunks, &entity_extractions)
        .expect("4,096 document entities are accepted");
    assert_eq!(exact_entities.entities.len(), 4_096);
    entity_chunks.push(test_chunk(16, 16 * 512));
    entity_extractions.push(prepare::Extraction {
        entities: vec![test_entity(0, "entity-over".to_owned(), [0, 1])],
        relations: Vec::new(),
    });
    assert_eq!(
        prepare::resolve("binding", "tenant", &entity_chunks, &entity_extractions)
            .unwrap_err()
            .category(),
        "document_entity_limit"
    );

    let mut relation_chunks = Vec::new();
    let mut relation_extractions = Vec::new();
    for ordinal in 0..16 {
        let entities = vec![
            test_entity(0, "left".to_owned(), [0, 1]),
            test_entity(1, "right".to_owned(), [1, 2]),
        ];
        let relations = (0..512)
            .map(|index| prepare::RelationObservation {
                subject_local_id: 0,
                predicate: format!("predicate-{}-{index}", ordinal),
                object_local_id: 1,
                span: [0, 2],
                attributes: prepare::Attributes::new(),
                confidence: None,
            })
            .collect();
        relation_chunks.push(test_chunk(ordinal, ordinal * 512));
        relation_extractions.push(prepare::Extraction {
            entities,
            relations,
        });
    }
    let exact_relations =
        prepare::resolve("binding", "tenant", &relation_chunks, &relation_extractions)
            .expect("8,192 document relations are accepted");
    assert_eq!(exact_relations.relations.len(), 8_192);
    relation_chunks.push(test_chunk(16, 16 * 512));
    relation_extractions.push(prepare::Extraction {
        entities: vec![
            test_entity(0, "left".to_owned(), [0, 1]),
            test_entity(1, "right".to_owned(), [1, 2]),
        ],
        relations: vec![prepare::RelationObservation {
            subject_local_id: 0,
            predicate: "one-more".to_owned(),
            object_local_id: 1,
            span: [0, 2],
            attributes: prepare::Attributes::new(),
            confidence: None,
        }],
    });
    assert_eq!(
        prepare::resolve("binding", "tenant", &relation_chunks, &relation_extractions)
            .unwrap_err()
            .category(),
        "document_relation_limit"
    );

    let mut mention_chunks = Vec::new();
    let mut mention_extractions = Vec::new();
    for ordinal in 0..64 {
        let entities = (0..256)
            .map(|index| {
                test_entity(
                    index,
                    "shared".to_owned(),
                    [index as usize, index as usize + 1],
                )
            })
            .collect();
        mention_chunks.push(test_chunk(ordinal, ordinal * 256));
        mention_extractions.push(prepare::Extraction {
            entities,
            relations: Vec::new(),
        });
    }
    let exact_mentions =
        prepare::resolve("binding", "tenant", &mention_chunks, &mention_extractions)
            .expect("16,384 document mentions are accepted");
    assert_eq!(exact_mentions.mentions.len(), 16_384);
    mention_chunks.push(test_chunk(64, 64 * 256));
    mention_extractions.push(prepare::Extraction {
        entities: vec![test_entity(0, "shared".to_owned(), [0, 1])],
        relations: Vec::new(),
    });
    assert_eq!(
        prepare::resolve("binding", "tenant", &mention_chunks, &mention_extractions)
            .unwrap_err()
            .category(),
        "document_mention_limit"
    );
}

#[test]
fn source_identity_and_binding_reject_empty_or_nul_fields() {
    let valid = prepare::SourceIdentity {
        tenant: "t".into(),
        source_id: "s".into(),
        uri: None,
    };
    for value in ["", "bad\0"] {
        let mut identity = valid.clone();
        identity.tenant = value.into();
        assert_eq!(
            prepare::validate_source(b"x", &identity)
                .unwrap_err()
                .category(),
            "source_identity"
        );
        let mut identity = valid.clone();
        identity.source_id = value.into();
        assert_eq!(
            prepare::validate_source(b"x", &identity)
                .unwrap_err()
                .category(),
            "source_identity"
        );
        assert_eq!(
            prepare::validate_binding(value).unwrap_err().category(),
            "source_identity"
        );
    }
    let mut identity = valid;
    identity.uri = Some("uri\0".into());
    assert_eq!(
        prepare::validate_source(b"x", &identity)
            .unwrap_err()
            .category(),
        "source_identity"
    );
}

#[test]
fn ascii_keys_preserve_vertical_tab_and_form_feed() {
    assert_eq!(
        prepare::normalize_key(" \x0bA\x0c\tB\x0b ").unwrap(),
        "\x0ba\x0c b\x0b"
    );
    assert_eq!(prepare::normalize_key("\x0c").unwrap(), "\x0c");
}

#[test]
fn dictionary_equal_offset_uses_rule_id_and_multi_hop_failure_links() {
    let output = prepare::dictionary_extract(
        "x",
        &[
            entity_rule("z", "x", "K", "last"),
            entity_rule("a", "x", "K", "first"),
        ],
        &[],
    )
    .unwrap();
    assert_eq!(
        output
            .entities
            .iter()
            .map(|e| (e.local_id, e.key.as_str(), e.span))
            .collect::<Vec<_>>(),
        [(0, "first", [0, 1]), (1, "last", [0, 1])]
    );
    let build_rules = [
        entity_rule("long", "abcp", "K", "long"),
        entity_rule("bc", "bcq", "K", "bc"),
        entity_rule("c", "cr", "K", "c"),
        entity_rule("suffix", "p", "K", "suffix"),
    ];
    let output = prepare::dictionary_extract("abcp", &build_rules, &[]).unwrap();
    assert_eq!(
        output
            .entities
            .iter()
            .map(|e| (e.key.as_str(), e.span))
            .collect::<Vec<_>>(),
        [("long", [0, 4]), ("suffix", [3, 4])]
    );
    let match_rules = [
        entity_rule("long", "abcq", "K", "long"),
        entity_rule("bc", "bcr", "K", "bc"),
        entity_rule("c", "cs", "K", "c"),
        entity_rule("suffix", "p", "K", "suffix"),
    ];
    let output = prepare::dictionary_extract("abcp", &match_rules, &[]).unwrap();
    assert_eq!(
        output
            .entities
            .iter()
            .map(|e| (e.key.as_str(), e.span))
            .collect::<Vec<_>>(),
        [("suffix", [3, 4])]
    );
}

#[test]
fn relation_dedup_retains_predicate_and_span() {
    let relations = vec![
        test_relation(0, "p", 0, [0, 1]),
        test_relation(0, "q", 0, [0, 1]),
        test_relation(0, "p", 0, [1, 2]),
        test_relation(0, "p", 0, [0, 1]),
    ];
    let output =
        prepare::validate_extraction("xx", vec![test_entity(0, "x".into(), [0, 1])], relations)
            .unwrap();
    assert_eq!(
        output
            .relations
            .iter()
            .map(|r| (r.predicate.as_str(), r.span))
            .collect::<Vec<_>>(),
        [("p", [0, 1]), ("q", [0, 1]), ("p", [1, 2])]
    );
}

#[test]
fn attribute_validation_and_identity_preserve_numeric_types_and_zero() {
    use prepare::Attribute;
    let attributes = |value| prepare::Attributes::from([("n".into(), value)]);
    let signed = prepare::attributes_key(&attributes(Attribute::Integer(0)));
    let unsigned = prepare::attributes_key(&attributes(Attribute::Unsigned(0)));
    let float = prepare::attributes_key(&attributes(Attribute::Float(0.0)));
    assert_ne!(signed, unsigned);
    assert_ne!(signed, float);
    assert_ne!(unsigned, float);
    assert_eq!(
        float,
        prepare::attributes_key(&attributes(Attribute::Float(-0.0)))
    );
    for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        assert_eq!(
            prepare::validate_attributes(attributes(Attribute::Float(value)))
                .unwrap_err()
                .category(),
            "attribute_value"
        );
    }
    assert_eq!(
        prepare::validate_attributes(prepare::Attributes::from([("".into(), Attribute::Null)]))
            .unwrap_err()
            .category(),
        "attribute_key"
    );
    assert!(
        prepare::validate_attributes(prepare::Attributes::from([(
            "k".repeat(4096),
            Attribute::Null
        )]))
        .is_ok()
    );
    assert_eq!(
        prepare::validate_attributes(prepare::Attributes::from([(
            "k".repeat(4097),
            Attribute::Null
        )]))
        .unwrap_err()
        .category(),
        "attribute_string_limit"
    );
    let mut entity = test_entity(0, "x".into(), [0, 1]);
    entity.confidence = Some(-0.0);
    let mut relation = test_relation(0, "p", 0, [0, 1]);
    relation.confidence = Some(-0.0);
    let output = prepare::validate_extraction("x", vec![entity.clone()], vec![relation]).unwrap();
    assert_eq!(
        output.entities[0].confidence.unwrap().to_bits(),
        0.0f64.to_bits()
    );
    assert_eq!(
        output.relations[0].confidence.unwrap().to_bits(),
        0.0f64.to_bits()
    );
    entity.confidence = Some(-1e-9);
    assert_eq!(
        prepare::validate_extraction("x", vec![entity], vec![])
            .unwrap_err()
            .category(),
        "confidence_invalid"
    );
}

fn resolution_extraction() -> prepare::Extraction {
    let mut a = test_entity(0, "z".into(), [0, 1]);
    a.kind = "A".into();
    let mut b = test_entity(1, "y".into(), [0, 1]);
    b.kind = "A".into();
    let mut c = test_entity(2, "a".into(), [0, 1]);
    c.kind = "B".into();
    prepare::Extraction {
        entities: vec![a, c, b],
        relations: vec![
            test_relation(0, "z", 2, [0, 1]),
            test_relation(0, "p", 2, [0, 1]),
            test_relation(0, "p", 1, [0, 1]),
            test_relation(1, "p", 2, [0, 1]),
        ],
    }
}

#[test]
fn resolution_source_offsets_include_second_chunk_start() {
    let chunks = [test_chunk(0, 0), test_chunk(1, 1024)];
    let output = prepare::resolve(
        "b",
        "t",
        &chunks,
        &[resolution_extraction(), resolution_extraction()],
    )
    .unwrap();
    assert_eq!(
        output
            .mentions
            .iter()
            .map(|m| m.source_span)
            .collect::<Vec<_>>(),
        [
            [0, 1],
            [0, 1],
            [0, 1],
            [1024, 1025],
            [1024, 1025],
            [1024, 1025]
        ]
    );
    assert_eq!(
        output
            .relations
            .iter()
            .map(|r| r.source_span)
            .collect::<Vec<_>>(),
        [
            [0, 1],
            [0, 1],
            [0, 1],
            [0, 1],
            [1024, 1025],
            [1024, 1025],
            [1024, 1025],
            [1024, 1025]
        ]
    );
}

#[test]
fn resolution_chunk_keys_and_oracle_defined_output_order() {
    // Oracle-defined presentation order, not a frozen persisted-order contract.
    let chunks = [test_chunk(1, 0), test_chunk(0, 0)];
    let output = prepare::resolve(
        "b",
        "t",
        &chunks,
        &[resolution_extraction(), resolution_extraction()],
    )
    .unwrap();
    assert_eq!(
        output
            .entities
            .iter()
            .map(|e| (e.kind.as_str(), e.normalized_key.as_str()))
            .collect::<Vec<_>>(),
        [("A", "y"), ("A", "z"), ("B", "a")]
    );
    assert_eq!(
        output
            .mentions
            .iter()
            .map(|m| (
                m.chunk,
                m.entity.kind.as_str(),
                m.entity.normalized_key.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            (0, "A", "y"),
            (0, "A", "z"),
            (0, "B", "a"),
            (1, "A", "y"),
            (1, "A", "z"),
            (1, "B", "a")
        ]
    );
    assert_eq!(
        output
            .relations
            .iter()
            .map(|r| (
                r.chunk,
                r.subject.normalized_key.as_str(),
                r.predicate.as_str(),
                r.object.normalized_key.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            (0, "y", "p", "a"),
            (0, "z", "p", "y"),
            (0, "z", "p", "a"),
            (0, "z", "z", "a"),
            (1, "y", "p", "a"),
            (1, "z", "p", "y"),
            (1, "z", "p", "a"),
            (1, "z", "z", "a")
        ]
    );
}

#[test]
fn resolved_relation_confidence_conflict_rejected_after_alias_resolution() {
    let mut extraction = resolution_extraction();
    let mut alias = extraction.entities[0].clone();
    alias.local_id = 3;
    extraction.entities.push(alias);
    let mut relation = extraction.relations[0].clone();
    relation.subject_local_id = 3;
    relation.confidence = Some(0.7);
    extraction.relations.push(relation);
    assert_eq!(
        prepare::resolve("b", "t", &[test_chunk(0, 0)], &[extraction])
            .unwrap_err()
            .category(),
        "relation_conflict"
    );
}

#[test]
fn resolution_rejects_extra_extractions_and_untrimmed_strategy_names() {
    assert_eq!(
        prepare::resolve("b", "t", &[], &[resolution_extraction()])
            .unwrap_err()
            .category(),
        "local_reference_invalid"
    );
    assert_eq!(
        prepare::ChunkStrategy::parse(" FixedUtf8V1")
            .unwrap_err()
            .category(),
        "chunk_strategy"
    );
}

#[test]
fn h1_mention_source_span_overflow_is_sanitized() {
    let mut chunk = test_chunk(0, 0);
    chunk.start = usize::MAX;
    for span in [[0, 1], [1, 2]] {
        let extraction = prepare::Extraction {
            entities: vec![test_entity(0, "x".into(), span)],
            relations: vec![],
        };
        assert_eq!(
            prepare::resolve("b", "t", &[chunk.clone()], &[extraction])
                .unwrap_err()
                .category(),
            "evidence_span"
        );
    }
}

#[test]
fn h1_relation_source_span_overflow_is_sanitized() {
    let mut chunk = test_chunk(0, 0);
    chunk.start = usize::MAX - 1;
    let extraction = prepare::Extraction {
        entities: vec![test_entity(0, "x".into(), [0, 1])],
        relations: vec![test_relation(0, "p", 0, [0, 2])],
    };
    assert_eq!(
        prepare::resolve("b", "t", &[chunk], &[extraction])
            .unwrap_err()
            .category(),
        "evidence_span"
    );
}
