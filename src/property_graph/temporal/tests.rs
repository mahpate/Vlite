//! Gap 4 adapter tests: the independent interval oracle against the engine
//! model, and the temporal header capability.

use std::fs;
use std::path::Path;

use serde_json::Value;

use super::super::engine::temporal::{self, Segment};
use super::super::engine::{GraphError, Properties, PropertyValue};
use super::Activation;
use crate::format::{self, FLAG_TEMPORAL};
use crate::{Database, Error};

// --- Independent oracle (`tests/temporal_interval_oracle.py`) -------------

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/temporal_intervals/golden.json"
);

fn fixture() -> Value {
    serde_json::from_str(&fs::read_to_string(FIXTURE).unwrap()).unwrap()
}

/// JSON property values map to VLite property types; a nested value (the
/// fixture uses one to check per-segment properties) becomes its JSON text.
fn property(value: &Value) -> PropertyValue {
    match value {
        Value::String(text) => PropertyValue::String(text.clone()),
        Value::Bool(flag) => PropertyValue::Boolean(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => PropertyValue::Integer(integer),
            None => PropertyValue::Float(number.as_f64().unwrap()),
        },
        other => PropertyValue::String(other.to_string()),
    }
}

fn bound(value: &Value) -> Result<Option<i64>, &'static str> {
    match value {
        Value::Null => Ok(None),
        Value::Number(number) if !number.is_f64() => number.as_i64().map(Some).ok_or("timestamp"),
        _ => Err("timestamp"),
    }
}

/// The JSON-level oracle categories (`series_type`, `segment_fields`,
/// `timestamp`, `properties`), checked in the oracle's order; the engine
/// owns `bounds`, `order`, `overlap` and `segment_limit`.
fn segment(value: &Value) -> Result<Segment, &'static str> {
    let Some(object) = value.as_object() else {
        return Err("segment_fields");
    };
    let mut fields: Vec<&str> = object.keys().map(String::as_str).collect();
    fields.sort_unstable();
    if fields != ["properties", "valid_from", "valid_to"] {
        return Err("segment_fields");
    }
    let valid_from = bound(&object["valid_from"])?;
    let valid_to = bound(&object["valid_to"])?;
    let Some(properties) = object["properties"].as_object() else {
        return Err("properties");
    };
    Ok(Segment {
        valid_from,
        valid_to,
        properties: properties
            .iter()
            .map(|(name, value)| (name.clone(), property(value)))
            .collect::<Properties>(),
    })
}

/// Converts and validates a series, returning the oracle category of the
/// first error. The segment limit is checked first, as the oracle does.
fn series(value: &Value) -> Result<Vec<Segment>, String> {
    let Some(items) = value.as_array() else {
        return Err("series_type".into());
    };
    if items.len() > temporal::MAX_SEGMENTS {
        return Err("segment_limit".into());
    }
    let mut segments = Vec::new();
    for item in items {
        segments.push(segment(item)?);
        // Per segment, the engine checks bounds, order and overlap before the
        // next segment's JSON shape, exactly like the oracle.
        if let Err(error) = temporal::validate_series(&segments) {
            return Err(category(error));
        }
    }
    Ok(segments)
}

fn category(error: GraphError) -> String {
    match error {
        GraphError::InvalidTemporal(reason) => reason.split(':').next().unwrap().to_owned(),
        other => panic!("not a temporal error: {other:?}"),
    }
}

/// Entities `{retained, expires_at_ms}` of edge, source and target.
fn hidden(entities: &[&Value], clock: u64) -> bool {
    entities.iter().any(|entity| {
        !entity["retained"].as_bool().unwrap()
            || entity["expires_at_ms"]
                .as_u64()
                .is_some_and(|deadline| clock >= deadline)
    })
}

/// Checks every query of a case and returns how many there were.
fn check_queries(name: &str, segments: &[Segment], entities: &[&Value], queries: &Value) -> usize {
    let queries = queries.as_array().unwrap();
    for query in queries {
        let as_of = query["T"].as_i64().unwrap();
        let clock = query["C"].as_u64().unwrap();
        let expected = match &query["expected"] {
            Value::Null => None,
            value => Some(segment(value).unwrap()),
        };
        let actual = if hidden(entities, clock) {
            None
        } else {
            temporal::segment_at(segments, as_of)
        };
        assert_eq!(actual, expected.as_ref(), "{name}: T={as_of} C={clock}");
    }
    queries.len()
}

#[test]
fn oracle_fixture_matches_the_engine_model() {
    let document = fixture();
    assert_eq!(document["schema"], "vlite.temporal-interval-golden");
    assert_eq!(document["version"], 1);
    let mut queries = 0;
    let cases = document["series"].as_array().unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let segments = series(&case["segments"]).unwrap_or_else(|error| panic!("{name}: {error}"));
        let entities = [&case["edge"], &case["source"], &case["target"]];
        queries += check_queries(name, &segments, &entities, &case["queries"]);
    }
    let invalid = document["invalid_series"].as_array().unwrap();
    for case in invalid {
        let name = case["name"].as_str().unwrap();
        let expected = case["expected_error"].as_str().unwrap();
        assert_eq!(
            series(&case["segments"]).err().as_deref(),
            Some(expected),
            "{name}"
        );
    }
    let limits = document["limit_cases"].as_array().unwrap();
    for case in limits {
        let name = case["name"].as_str().unwrap();
        let count = case["segment_count"].as_i64().unwrap();
        let segments: Vec<Segment> = (0..count)
            .map(|i| Segment {
                valid_from: Some(i),
                valid_to: Some(i + 1),
                properties: Properties::from([(
                    "value".into(),
                    PropertyValue::String("same".into()),
                )]),
            })
            .collect();
        match case["expected_error"].as_str() {
            None => {
                assert_eq!(temporal::validate_series(&segments), Ok(()), "{name}");
                queries += check_queries(name, &segments, &[], &case["queries"]);
            }
            Some(expected) => assert_eq!(
                temporal::validate_series(&segments).map_err(category),
                Err(expected.to_owned()),
                "{name}"
            ),
        }
    }
    // Every group was exercised (the fixture's own counts at authoring time).
    assert!(cases.len() >= 13 && invalid.len() >= 24 && limits.len() >= 2);
    assert!(queries >= 50, "{queries} queries");
}

// --- Header capability ----------------------------------------------------

mod activate;
mod revisions;
mod writes;

fn set_header_flags(path: &Path, flags: u16) {
    let mut bytes = fs::read(path).unwrap();
    bytes[26..28].copy_from_slice(&flags.to_le_bytes());
    let crc = format::crc32(&bytes[..28]);
    bytes[28..32].copy_from_slice(&crc.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn header_flags(path: &Path) -> u16 {
    let bytes = fs::read(path).unwrap();
    u16::from_le_bytes([bytes[26], bytes[27]])
}

#[test]
fn the_temporal_flag_is_64_and_collides_with_no_other_flag() {
    assert_eq!(FLAG_TEMPORAL, 64);
    for other in [
        format::FLAG_ENCRYPTED,
        format::FLAG_TIMESTAMPS,
        format::FLAG_GRAPH,
        format::FLAG_HISTORY,
        format::FLAG_COMPACT,
        format::FLAG_PRECISION,
    ] {
        assert_eq!(other & FLAG_TEMPORAL, 0, "{other}");
    }
}

#[test]
fn builds_without_temporal_support_refuse_the_flag_before_loading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.create_graph("g").unwrap();
    db.close().unwrap();
    let flags = header_flags(&path);
    assert_eq!(flags & format::FLAG_GRAPH, format::FLAG_GRAPH);
    set_header_flags(&path, flags | FLAG_TEMPORAL);

    let _off = Activation::set(false);
    assert!(matches!(
        Database::open(&path),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    assert!(matches!(
        Database::open_read_only(&path),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    {
        let _on = Activation::on();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.graph("g").unwrap().info().edge_count, 0);
    }
    // The activation is per thread and restored when dropped.
    assert!(!super::supported());
}

#[test]
fn the_temporal_flag_requires_the_graph_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.vlite");
    Database::create(&path, 2).unwrap().close().unwrap();
    // New files are graph-enabled; a v5 file without graph frames may clear
    // the graph flag and still opens.
    let flags = header_flags(&path) & !format::FLAG_GRAPH;
    set_header_flags(&path, flags);
    let _on = Activation::on();
    Database::open(&path).unwrap().close().unwrap();
    set_header_flags(&path, flags | FLAG_TEMPORAL);
    assert!(matches!(
        Database::open(&path),
        Err(Error::Corrupt { offset: 0, .. })
    ));
}
